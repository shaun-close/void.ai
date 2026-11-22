//! Anthropic-style `/v1/messages` streaming.

use std::collections::BTreeMap;

use futures_util::future::BoxFuture;
use serde_json::{Value, json};

use super::sse::SseEvent;
use super::{
    Backend, BackendError, ChatRequest, EventStream, Mapper, StopReason, StreamEvent,
    connect_error, drive, http_client, http_error, join_url, merge_json,
};
use crate::config::Profile;
use crate::model::{Role, Usage};

pub const ANTHROPIC_VERSION: &str = "2023-06-01";

pub struct AnthropicBackend {
    base_url: String,
    key: Option<String>,
    client: reqwest::Client,
}

impl AnthropicBackend {
    pub fn new(profile: &Profile, api_key: Option<String>) -> Self {
        Self {
            base_url: profile.base_url.clone(),
            key: api_key,
            client: http_client(&profile.base_url),
        }
    }

    fn headers(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let rb = rb.header("anthropic-version", ANTHROPIC_VERSION);
        match &self.key {
            Some(k) => rb.header("x-api-key", k),
            None => rb,
        }
    }
}

/// The JSON body for a streaming messages request. The system prompt is top-level; past
/// reasoning and empty turns are not sent.
pub fn request_body(req: &ChatRequest) -> Value {
    let messages: Vec<Value> = req
        .messages
        .iter()
        .filter_map(|m| {
            let role = match m.role {
                Role::User => "user",
                Role::Assistant => "assistant",
                // System text goes in `system`; tool turns arrive with milestone 2.
                Role::System | Role::Tool => return None,
            };
            let text = m.text();
            (!text.is_empty()).then(|| json!({"role": role, "content": text}))
        })
        .collect();
    let mut body = match json!({
        "model": req.model,
        "system": req.system,
        "messages": messages,
        "max_tokens": req.max_output,
        "stream": true,
    }) {
        Value::Object(m) => m,
        _ => unreachable!(),
    };
    merge_json(&mut body, &req.extra_body);
    Value::Object(body)
}

impl Backend for AnthropicBackend {
    fn stream(&self, req: ChatRequest) -> EventStream {
        let rb = self
            .client
            .post(join_url(&self.base_url, "v1/messages"))
            .json(&request_body(&req));
        drive(
            self.headers(rb),
            AnthropicStream::default(),
            self.key.clone(),
        )
    }

    fn list_models(&self) -> BoxFuture<'static, Result<Vec<String>, BackendError>> {
        let rb = self.headers(self.client.get(join_url(&self.base_url, "v1/models")));
        let key = self.key.clone();
        Box::pin(async move {
            let resp = rb
                .send()
                .await
                .map_err(|e| connect_error(&e, key.as_deref()))?;
            if !resp.status().is_success() {
                return Err(http_error(resp, key.as_deref()).await);
            }
            let v: Value = resp
                .json()
                .await
                .map_err(|e| BackendError::Protocol(e.to_string()))?;
            Ok(super::openai::model_ids(&v))
        })
    }
}

/// Turns messages-API events into stream events.
#[derive(Debug, Default)]
pub struct AnthropicStream {
    input_tokens: u32,
    tools: BTreeMap<u64, (String, String, String)>,
    finish: Option<StopReason>,
    ended: bool,
    done_sent: bool,
}

impl AnthropicStream {
    pub fn on_event(&mut self, ev: &SseEvent) -> Result<Vec<StreamEvent>, BackendError> {
        Mapper::on_event(self, ev)
    }
    pub fn ended(&self) -> bool {
        Mapper::ended(self)
    }
    pub fn on_eof(&mut self) -> Vec<StreamEvent> {
        Mapper::on_eof(self)
    }
}

fn u32_at(v: &Value, ptr: &str) -> Option<u32> {
    v.pointer(ptr).and_then(Value::as_u64).map(|n| n as u32)
}

impl Mapper for AnthropicStream {
    fn on_event(&mut self, ev: &SseEvent) -> Result<Vec<StreamEvent>, BackendError> {
        let v: Value = serde_json::from_str(&ev.data)
            .map_err(|e| BackendError::Protocol(format!("bad event ({e})")))?;
        let kind = v
            .get("type")
            .and_then(Value::as_str)
            .or(ev.event.as_deref())
            .unwrap_or("");
        let index = v.get("index").and_then(Value::as_u64).unwrap_or(0);
        let mut out = Vec::new();
        match kind {
            "message_start" => {
                self.input_tokens = u32_at(&v, "/message/usage/input_tokens").unwrap_or(0);
            }
            "content_block_start" => {
                let block = &v["content_block"];
                match block["type"].as_str() {
                    Some("tool_use") => {
                        let s = |k: &str| block[k].as_str().unwrap_or("").to_string();
                        self.tools
                            .insert(index, (s("id"), s("name"), String::new()));
                    }
                    Some("text") => {
                        if let Some(t) = block["text"].as_str().filter(|t| !t.is_empty()) {
                            out.push(StreamEvent::TextDelta(t.into()));
                        }
                    }
                    Some("thinking") => {
                        if let Some(t) = block["thinking"].as_str().filter(|t| !t.is_empty()) {
                            out.push(StreamEvent::ReasoningDelta(t.into()));
                        }
                    }
                    _ => {}
                }
            }
            "content_block_delta" => {
                let d = &v["delta"];
                let text = |k: &str| d[k].as_str().filter(|t| !t.is_empty()).map(str::to_owned);
                match d["type"].as_str() {
                    Some("text_delta") => out.extend(text("text").map(StreamEvent::TextDelta)),
                    Some("thinking_delta") => {
                        out.extend(text("thinking").map(StreamEvent::ReasoningDelta))
                    }
                    Some("input_json_delta") => {
                        if let (Some(t), Some(p)) =
                            (self.tools.get_mut(&index), d["partial_json"].as_str())
                        {
                            t.2.push_str(p);
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                if let Some((id, name, arguments)) = self.tools.remove(&index) {
                    out.push(StreamEvent::ToolCall {
                        id,
                        name,
                        arguments,
                    });
                }
            }
            "message_delta" => {
                if let Some(r) = v.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    self.finish = Some(match r {
                        "end_turn" | "stop_sequence" => StopReason::EndTurn,
                        "max_tokens" => StopReason::MaxTokens,
                        "tool_use" => StopReason::ToolUse,
                        other => StopReason::Other(other.into()),
                    });
                }
                if let Some(output) = u32_at(&v, "/usage/output_tokens") {
                    out.push(StreamEvent::Usage(Usage {
                        prompt_tokens: u32_at(&v, "/usage/input_tokens")
                            .unwrap_or(self.input_tokens),
                        completion_tokens: output,
                    }));
                }
            }
            "message_stop" => self.ended = true,
            "error" => {
                let msg = v
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| v.to_string());
                return Err(BackendError::Protocol(msg));
            }
            _ => {}
        }
        Ok(out)
    }

    fn ended(&self) -> bool {
        self.ended
    }

    fn on_eof(&mut self) -> Vec<StreamEvent> {
        if self.done_sent {
            return Vec::new();
        }
        self.done_sent = true;
        let reason = self
            .finish
            .take()
            .unwrap_or_else(|| StopReason::Other("eof".into()));
        vec![StreamEvent::Done(reason)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::sse::{SseEvent, SseParser};
    use crate::backend::testsrv::{json_response, serve_once, sse_response};
    use crate::config::{ApiKeySource, BackendKind, Profile};
    use crate::model::{Message, Part, Role, Usage};
    use futures_util::StreamExt;
    use serde_json::json;

    fn profile(base_url: &str) -> Profile {
        Profile {
            id: "a".into(),
            name: "a".into(),
            kind: BackendKind::Anthropic,
            base_url: base_url.into(),
            api_key: ApiKeySource::None,
            model: "m".into(),
            context_limit: 1000,
            max_output: 321,
            extra_body: Default::default(),
        }
    }

    fn request() -> ChatRequest {
        ChatRequest {
            model: "m".into(),
            system: "be nice".into(),
            messages: vec![
                Message::user("hi"),
                Message::new(
                    Role::Assistant,
                    vec![Part::Reasoning("hidden".into()), Part::Text("hello".into())],
                ),
                Message::new(Role::Assistant, vec![]),
                Message::user("again"),
            ],
            max_output: 321,
            extra_body: json!({"temperature": 0.5}).as_object().cloned().unwrap(),
        }
    }

    fn map_fixture(text: &str) -> Vec<StreamEvent> {
        let mut parser = SseParser::new();
        let mut m = AnthropicStream::default();
        let mut out = Vec::new();
        for ev in parser
            .feed(text.as_bytes())
            .into_iter()
            .chain(parser.finish())
        {
            out.extend(m.on_event(&ev).unwrap());
            if m.ended() {
                break;
            }
        }
        out.extend(m.on_eof());
        out
    }

    #[test]
    fn anthropic_body_has_system_and_version() {
        let body = request_body(&request());
        assert_eq!(body["system"], "be nice");
        assert_eq!(body["max_tokens"], 321);
        assert_eq!(body["stream"], true);
        assert_eq!(body["temperature"], 0.5);
        assert_eq!(
            body["messages"],
            json!([
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": "hello"},
                {"role": "user", "content": "again"},
            ])
        );
        assert!(!body.to_string().contains("hidden"));
    }

    #[test]
    fn anthropic_maps_text_fixture() {
        assert_eq!(
            map_fixture(include_str!("../../tests/fixtures/anthropic_text.sse")),
            vec![
                StreamEvent::TextDelta("Héllo".into()),
                StreamEvent::TextDelta(" → world".into()),
                StreamEvent::Usage(Usage {
                    prompt_tokens: 21,
                    completion_tokens: 5
                }),
                StreamEvent::Done(StopReason::EndTurn),
            ]
        );
    }

    #[test]
    fn anthropic_maps_thinking_fixture() {
        assert_eq!(
            map_fixture(include_str!("../../tests/fixtures/anthropic_thinking.sse")),
            vec![
                StreamEvent::ReasoningDelta("Short answer".into()),
                StreamEvent::ReasoningDelta(" is best.".into()),
                StreamEvent::TextDelta("Pong.".into()),
                StreamEvent::ToolCall {
                    id: "toolu_1".into(),
                    name: "read".into(),
                    arguments: "{\"path\":\"a.rs\"}".into(),
                },
                StreamEvent::Usage(Usage {
                    prompt_tokens: 40,
                    completion_tokens: 17
                }),
                StreamEvent::Done(StopReason::ToolUse),
            ]
        );
    }

    #[test]
    fn anthropic_error_event_is_protocol_error() {
        let mut m = AnthropicStream::default();
        let ev = SseEvent {
            event: Some("error".into()),
            data: r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#
                .into(),
        };
        assert_eq!(
            m.on_event(&ev).unwrap_err(),
            BackendError::Protocol("Overloaded".into())
        );
    }

    #[tokio::test]
    async fn anthropic_streams_over_http() {
        let fixture = include_str!("../../tests/fixtures/anthropic_text.sse");
        let (base, req) = serve_once(sse_response(fixture, 6)).await;
        let backend = AnthropicBackend::new(&profile(&base), Some("k-ant".into()));
        let evs: Vec<StreamEvent> = backend
            .stream(request())
            .map(Result::unwrap)
            .collect()
            .await;
        assert_eq!(evs, map_fixture(fixture));
        let raw = req.await.unwrap().to_lowercase();
        assert!(raw.starts_with("post /v1/messages http/1.1"));
        assert!(raw.contains("x-api-key: k-ant\r\n"));
        assert!(raw.contains("anthropic-version: 2023-06-01\r\n"));
    }

    #[tokio::test]
    async fn anthropic_http_error_uses_server_message() {
        let (base, _req) = serve_once(json_response(
            400,
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"max_tokens too large"}}"#,
        ))
        .await;
        let backend = AnthropicBackend::new(&profile(&base), None);
        let evs: Vec<_> = backend.stream(request()).collect().await;
        assert_eq!(
            evs,
            vec![Err(BackendError::Http {
                status: 400,
                message: "max_tokens too large".into()
            })]
        );
    }

    #[tokio::test]
    async fn anthropic_lists_models() {
        let (base, req) = serve_once(json_response(
            200,
            r#"{"data":[{"id":"m1","type":"model"}]}"#,
        ))
        .await;
        let backend = AnthropicBackend::new(&profile(&base), None);
        assert_eq!(backend.list_models().await.unwrap(), vec!["m1"]);
        assert!(req.await.unwrap().starts_with("GET /v1/models HTTP/1.1"));
    }

    #[tokio::test]
    async fn make_backend_builds_anthropic() {
        let b = crate::backend::make_backend(&profile("http://127.0.0.1:1"), None);
        // Not the "isn't available yet" placeholder: a real attempt that fails to connect.
        let evs = b.stream(request()).collect::<Vec<_>>().await;
        assert!(
            !matches!(evs.as_slice(), [Err(BackendError::Protocol(m))] if m.contains("isn't available"))
        );
    }
}

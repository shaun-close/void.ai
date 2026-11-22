//! OpenAI-compatible `/v1/chat/completions` streaming (Strata, llama-server, Ollama `/v1`).

use std::collections::BTreeMap;

use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};

use super::sse::SseEvent;
use super::{
    Backend, BackendError, ChatRequest, EventStream, Mapper, StopReason, StreamEvent, drive,
    http_client, http_error, join_url, merge_json,
};
use crate::config::Profile;
use crate::model::{Part, Role, Usage};

pub struct OpenAiBackend {
    base_url: String,
    key: Option<String>,
    client: reqwest::Client,
}

impl OpenAiBackend {
    pub fn new(profile: &Profile, api_key: Option<String>) -> Self {
        Self {
            base_url: profile.base_url.clone(),
            key: api_key,
            client: http_client(&profile.base_url),
        }
    }

    fn with_auth(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.key {
            Some(k) => rb.bearer_auth(k),
            None => rb,
        }
    }
}

/// The JSON body for a streaming chat completion. Past reasoning is not sent back.
pub fn request_body(req: &ChatRequest) -> Value {
    let mut messages = vec![json!({"role": "system", "content": req.system})];
    for m in &req.messages {
        match m.role {
            Role::System => messages.push(json!({"role": "system", "content": m.text()})),
            Role::User => messages.push(json!({"role": "user", "content": m.text()})),
            Role::Assistant => {
                let calls: Vec<Value> = m
                    .parts
                    .iter()
                    .filter_map(|p| match p {
                        Part::ToolCall {
                            id,
                            name,
                            arguments,
                        } => Some(json!({
                            "id": id,
                            "type": "function",
                            "function": {"name": name, "arguments": arguments},
                        })),
                        _ => None,
                    })
                    .collect();
                let text = m.text();
                if calls.is_empty() {
                    messages.push(json!({"role": "assistant", "content": text}));
                } else {
                    let content = if text.is_empty() {
                        Value::Null
                    } else {
                        Value::String(text)
                    };
                    messages.push(
                        json!({"role": "assistant", "content": content, "tool_calls": calls}),
                    );
                }
            }
            Role::Tool => {
                for p in &m.parts {
                    if let Part::ToolResult { call_id, content } = p {
                        messages.push(
                            json!({"role": "tool", "tool_call_id": call_id, "content": content}),
                        );
                    }
                }
            }
        }
    }
    let mut body = match json!({
        "model": req.model,
        "messages": messages,
        "stream": true,
        "stream_options": {"include_usage": true},
        "max_tokens": req.max_output,
    }) {
        Value::Object(m) => m,
        _ => unreachable!(),
    };
    if !req.tools.is_empty() {
        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                json!({"type": "function", "function": {
                "name": t.name, "description": t.description, "parameters": t.parameters}})
            })
            .collect();
        body.insert("tools".into(), Value::Array(tools));
    }
    merge_json(&mut body, &req.extra_body);
    Value::Object(body)
}

impl Backend for OpenAiBackend {
    fn stream(&self, req: ChatRequest) -> EventStream {
        let rb = self
            .client
            .post(join_url(&self.base_url, "chat/completions"))
            .json(&request_body(&req));
        drive(
            self.with_auth(rb),
            OpenAiStream::default(),
            self.key.clone(),
        )
    }

    fn list_models(&self) -> BoxFuture<'static, Result<Vec<String>, BackendError>> {
        let rb = self.with_auth(self.client.get(join_url(&self.base_url, "models")));
        let key = self.key.clone();
        Box::pin(async move {
            let resp = rb
                .send()
                .await
                .map_err(|e| super::connect_error(&e, key.as_deref()))?;
            if !resp.status().is_success() {
                return Err(http_error(resp, key.as_deref()).await);
            }
            let v: Value = resp
                .json()
                .await
                .map_err(|e| BackendError::Protocol(e.to_string()))?;
            Ok(model_ids(&v))
        })
    }
}

pub(crate) fn model_ids(v: &Value) -> Vec<String> {
    v["data"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|m| m["id"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// Turns chat-completion chunks into stream events.
#[derive(Debug, Default)]
pub struct OpenAiStream {
    tools: BTreeMap<u64, (String, String, String)>,
    finish: Option<StopReason>,
    ended: bool,
    done_sent: bool,
}

impl OpenAiStream {
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

fn delta_str<'a>(delta: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    delta
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

impl Mapper for OpenAiStream {
    fn on_event(&mut self, ev: &SseEvent) -> Result<Vec<StreamEvent>, BackendError> {
        if ev.data.trim() == "[DONE]" {
            self.ended = true;
            return Ok(Vec::new());
        }
        let v: Value = serde_json::from_str(&ev.data)
            .map_err(|e| BackendError::Protocol(format!("bad chunk ({e})")))?;
        if let Some(err) = v.get("error") {
            let msg = err
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| err.to_string());
            return Err(BackendError::Protocol(msg));
        }
        let mut out = Vec::new();
        if let Some(choice) = v.pointer("/choices/0") {
            if let Some(delta) = choice.get("delta").and_then(Value::as_object) {
                if let Some(r) =
                    delta_str(delta, "reasoning_content").or_else(|| delta_str(delta, "reasoning"))
                {
                    out.push(StreamEvent::ReasoningDelta(r.to_string()));
                }
                if let Some(t) = delta_str(delta, "content") {
                    out.push(StreamEvent::TextDelta(t.to_string()));
                }
                for tc in delta
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let index = tc.get("index").and_then(Value::as_u64).unwrap_or(0);
                    let entry = self.tools.entry(index).or_default();
                    if let Some(id) = tc.get("id").and_then(Value::as_str) {
                        entry.0.push_str(id);
                    }
                    if let Some(f) = tc.get("function") {
                        if let Some(n) = f.get("name").and_then(Value::as_str) {
                            entry.1.push_str(n);
                        }
                        if let Some(a) = f.get("arguments").and_then(Value::as_str) {
                            entry.2.push_str(a);
                        }
                    }
                }
            }
            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                self.finish = Some(match reason {
                    "stop" => StopReason::EndTurn,
                    "length" => StopReason::MaxTokens,
                    "tool_calls" | "function_call" => StopReason::ToolUse,
                    other => StopReason::Other(other.to_string()),
                });
                for (_, (id, name, arguments)) in std::mem::take(&mut self.tools) {
                    out.push(StreamEvent::ToolCall {
                        id,
                        name,
                        arguments,
                    });
                }
            }
        }
        if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
            let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0) as u32;
            out.push(StreamEvent::Usage(Usage {
                prompt_tokens: n("prompt_tokens"),
                completion_tokens: n("completion_tokens"),
            }));
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
    use crate::backend::testsrv::{json_response, serve_once, sse_response};
    use crate::backend::{Backend, sse::SseParser};
    use crate::config::{ApiKeySource, BackendKind, Config, Profile};
    use crate::model::{Message, Part, Role, Usage};
    use futures_util::StreamExt;
    use serde_json::json;

    fn profile(base_url: &str) -> Profile {
        Profile {
            id: "t".into(),
            name: "t".into(),
            kind: BackendKind::OpenAi,
            base_url: base_url.into(),
            api_key: ApiKeySource::None,
            model: "m".into(),
            context_limit: 1000,
            max_output: 100,
            extra_body: Default::default(),
        }
    }

    fn request(extra: serde_json::Value) -> ChatRequest {
        let mut assistant = Message::new(
            Role::Assistant,
            vec![
                Part::Reasoning("secret thoughts".into()),
                Part::Text("earlier".into()),
            ],
        );
        assistant.usage = Some(Usage::default());
        ChatRequest {
            model: "m".into(),
            system: "be nice".into(),
            messages: vec![Message::user("hi"), assistant, Message::user("again")],
            max_output: 100,
            extra_body: extra.as_object().cloned().unwrap_or_default(),
            tools: vec![],
        }
    }

    fn map_fixture(text: &str) -> Vec<StreamEvent> {
        let mut parser = SseParser::new();
        let mut m = OpenAiStream::default();
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
    fn openai_body_merges_extra_body() {
        let body = request_body(&request(json!({
            "chat_template_kwargs": {"enable_thinking": false},
            "temperature": 0.2
        })));
        assert_eq!(body["model"], "m");
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert_eq!(body["max_tokens"], 100);
        assert_eq!(body["temperature"], 0.2);
        assert_eq!(body["chat_template_kwargs"]["enable_thinking"], false);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[0], json!({"role": "system", "content": "be nice"}));
        assert_eq!(msgs[2], json!({"role": "assistant", "content": "earlier"}));
        assert_eq!(msgs.len(), 4);
        assert!(!body.to_string().contains("secret thoughts"));
    }

    #[test]
    fn extra_body_deep_merges_objects() {
        let body = request_body(&request(json!({"stream_options": {"x": 1}})));
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert_eq!(body["stream_options"]["x"], 1);
    }

    #[test]
    fn openai_maps_text_fixture() {
        let evs = map_fixture(include_str!("../../tests/fixtures/openai_text.sse"));
        assert_eq!(
            evs,
            vec![
                StreamEvent::TextDelta("Héllo".into()),
                StreamEvent::TextDelta(" → world".into()),
                StreamEvent::Usage(Usage {
                    prompt_tokens: 12,
                    completion_tokens: 4
                }),
                StreamEvent::Done(StopReason::EndTurn),
            ]
        );
    }

    #[test]
    fn openai_maps_reasoning_fixture() {
        let evs = map_fixture(include_str!("../../tests/fixtures/openai_reasoning.sse"));
        assert_eq!(
            evs,
            vec![
                StreamEvent::ReasoningDelta("The user wants".into()),
                StreamEvent::ReasoningDelta(" a greeting — ok.".into()),
                StreamEvent::TextDelta("Hi".into()),
                StreamEvent::TextDelta(" there.".into()),
                StreamEvent::Usage(Usage {
                    prompt_tokens: 30,
                    completion_tokens: 11
                }),
                StreamEvent::Done(StopReason::EndTurn),
            ]
        );
    }

    #[test]
    fn openai_accepts_reasoning_field_alias() {
        let evs = map_fixture(
            "data: {\"choices\":[{\"delta\":{\"reasoning\":\"r\"},\"finish_reason\":null}]}\n\n",
        );
        assert_eq!(
            evs,
            vec![
                StreamEvent::ReasoningDelta("r".into()),
                StreamEvent::Done(StopReason::Other("eof".into())),
            ]
        );
    }

    #[test]
    fn openai_assembles_tool_calls() {
        let evs = map_fixture(include_str!("../../tests/fixtures/openai_tools.sse"));
        assert_eq!(
            evs,
            vec![
                StreamEvent::ToolCall {
                    id: "call_1".into(),
                    name: "read".into(),
                    arguments: "{\"path\":\"a.rs\"}".into(),
                },
                StreamEvent::Done(StopReason::ToolUse),
            ]
        );
    }

    #[test]
    fn openai_error_chunk_is_protocol_error() {
        let mut m = OpenAiStream::default();
        let ev = crate::backend::sse::SseEvent {
            event: None,
            data: r#"{"error":{"message":"context too long"}}"#.into(),
        };
        let err = m.on_event(&ev).unwrap_err();
        assert!(matches!(err, BackendError::Protocol(ref s) if s.contains("context too long")));
    }

    #[tokio::test]
    async fn openai_streams_over_http() {
        let fixture = include_str!("../../tests/fixtures/openai_text.sse");
        let (base, req) = serve_once(sse_response(fixture, 5)).await;
        let backend = OpenAiBackend::new(&profile(&format!("{base}/v1")), Some("k".into()));
        let evs: Vec<_> = backend.stream(request(json!({}))).collect().await;
        let evs: Vec<StreamEvent> = evs.into_iter().map(Result::unwrap).collect();
        assert_eq!(evs, map_fixture(fixture));
        let raw = req.await.unwrap();
        assert!(raw.starts_with("POST /v1/chat/completions HTTP/1.1"));
        assert!(raw.to_lowercase().contains("authorization: bearer k\r\n"));
    }

    #[tokio::test]
    async fn openai_http_error_is_reported_without_key() {
        let (base, _req) = serve_once(json_response(
            401,
            r#"{"error":{"message":"invalid api key sk-test-123"}}"#,
        ))
        .await;
        let backend =
            OpenAiBackend::new(&profile(&format!("{base}/v1")), Some("sk-test-123".into()));
        let evs: Vec<_> = backend.stream(request(json!({}))).collect().await;
        assert_eq!(evs.len(), 1);
        let err = evs.into_iter().next().unwrap().unwrap_err();
        assert!(
            matches!(err, BackendError::Http { status: 401, ref message } if message.contains("invalid api key"))
        );
        assert!(!err.to_string().contains("sk-test-123"));
    }

    #[tokio::test]
    async fn openai_connection_refused_is_connect_error() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let backend = OpenAiBackend::new(&profile(&format!("http://{addr}/v1")), None);
        let evs: Vec<_> = backend.stream(request(json!({}))).collect().await;
        assert!(matches!(evs.as_slice(), [Err(BackendError::Connect(_))]));
    }

    #[tokio::test]
    async fn openai_lists_models() {
        let (base, req) = serve_once(json_response(
            200,
            r#"{"object":"list","data":[{"id":"qwen3.8-flash-next-iq3_xxs","object":"model"}]}"#,
        ))
        .await;
        let backend = OpenAiBackend::new(&profile(&format!("{base}/v1")), None);
        assert_eq!(
            backend.list_models().await.unwrap(),
            vec!["qwen3.8-flash-next-iq3_xxs"]
        );
        assert!(req.await.unwrap().starts_with("GET /v1/models HTTP/1.1"));
    }

    #[test]
    fn make_backend_accepts_default_profiles() {
        for p in Config::default().profiles {
            let _ = crate::backend::make_backend(&p, None);
        }
    }

    fn read_spec() -> crate::backend::ToolSpec {
        crate::backend::ToolSpec {
            name: "read".into(),
            description: "Read a file".into(),
            parameters: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
        }
    }

    fn tool_history() -> Vec<Message> {
        vec![
            Message::user("go"),
            Message::new(
                Role::Assistant,
                vec![
                    Part::Text("Let me look.".into()),
                    Part::ToolCall {
                        id: "c1".into(),
                        name: "read".into(),
                        arguments: "{\"path\":\"a\"}".into(),
                    },
                    Part::ToolCall {
                        id: "c2".into(),
                        name: "list".into(),
                        arguments: "{}".into(),
                    },
                ],
            ),
            Message::new(
                Role::Tool,
                vec![Part::ToolResult {
                    call_id: "c1".into(),
                    content: "A".into(),
                }],
            ),
            Message::new(
                Role::Tool,
                vec![Part::ToolResult {
                    call_id: "c2".into(),
                    content: "B".into(),
                }],
            ),
        ]
    }

    #[test]
    fn openai_body_includes_tools() {
        let mut r = request(json!({}));
        r.tools = vec![read_spec()];
        let b = request_body(&r);
        assert_eq!(
            b["tools"],
            json!([{"type":"function","function":{"name":"read","description":"Read a file","parameters":read_spec().parameters}}])
        );
        assert!(request_body(&request(json!({}))).get("tools").is_none());
    }

    #[test]
    fn openai_maps_tool_history() {
        let mut r = request(json!({}));
        r.messages = tool_history();
        let b = request_body(&r);
        let m = b["messages"].as_array().unwrap();
        assert_eq!(m.len(), 5);
        assert_eq!(
            m[2],
            json!({"role":"assistant","content":"Let me look.","tool_calls":[
                {"id":"c1","type":"function","function":{"name":"read","arguments":"{\"path\":\"a\"}"}},
                {"id":"c2","type":"function","function":{"name":"list","arguments":"{}"}}]})
        );
        assert_eq!(
            m[3],
            json!({"role":"tool","tool_call_id":"c1","content":"A"})
        );
        assert_eq!(
            m[4],
            json!({"role":"tool","tool_call_id":"c2","content":"B"})
        );
        // An assistant turn with only tool calls has null content.
        r.messages[1].parts.remove(0);
        assert_eq!(
            request_body(&r)["messages"][2]["content"],
            serde_json::Value::Null
        );
    }
}

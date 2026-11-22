//! Chat backends: a provider-neutral request and event stream, and one implementation per API.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;
use futures_util::stream::{self, BoxStream, StreamExt};
use serde_json::{Map, Value};

use crate::config::{BackendKind, Profile};
use crate::model::{Message, Usage};

pub mod openai;
pub mod sse;
#[cfg(test)]
pub(crate) mod testsrv;

use sse::{SseEvent, SseParser};

/// What a backend is asked for, independent of its wire format.
#[derive(Clone, Debug, PartialEq)]
pub struct ChatRequest {
    pub model: String,
    pub system: String,
    pub messages: Vec<Message>,
    pub max_output: u32,
    pub extra_body: Map<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    ToolUse,
    Other(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StreamEvent {
    TextDelta(String),
    ReasoningDelta(String),
    ToolCall {
        id: String,
        name: String,
        arguments: String,
    },
    Usage(Usage),
    Done(StopReason),
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BackendError {
    #[error("couldn't reach the backend: {0}")]
    Connect(String),
    #[error("the backend answered {status}: {message}")]
    Http { status: u16, message: String },
    #[error("the backend sent something unexpected: {0}")]
    Protocol(String),
}

pub type EventStream = BoxStream<'static, Result<StreamEvent, BackendError>>;

pub trait Backend: Send + Sync {
    /// Stream a reply. The stream ends after `Done` or after the first error.
    fn stream(&self, req: ChatRequest) -> EventStream;
    fn list_models(&self) -> BoxFuture<'static, Result<Vec<String>, BackendError>>;
}

pub fn make_backend(profile: &Profile, api_key: Option<String>) -> Arc<dyn Backend> {
    match profile.kind {
        BackendKind::OpenAi => Arc::new(openai::OpenAiBackend::new(profile, api_key)),
        BackendKind::Anthropic => Arc::new(Unsupported("the Anthropic backend")),
    }
}

/// Placeholder for a backend kind that isn't built yet.
struct Unsupported(&'static str);

impl Backend for Unsupported {
    fn stream(&self, _req: ChatRequest) -> EventStream {
        let msg = format!("{} isn't available yet", self.0);
        stream::iter([Err(BackendError::Protocol(msg))]).boxed()
    }
    fn list_models(&self) -> BoxFuture<'static, Result<Vec<String>, BackendError>> {
        let msg = format!("{} isn't available yet", self.0);
        Box::pin(async move { Err(BackendError::Protocol(msg)) })
    }
}

/// Merge `extra` into `base`: objects merge key by key, anything else in `extra` wins.
pub(crate) fn merge_json(base: &mut Map<String, Value>, extra: &Map<String, Value>) {
    for (k, v) in extra {
        match (base.get_mut(k), v) {
            (Some(Value::Object(b)), Value::Object(e)) => merge_json(b, e),
            _ => {
                base.insert(k.clone(), v.clone());
            }
        }
    }
}

pub(crate) fn http_client(base_url: &str) -> reqwest::Client {
    let mut b = reqwest::Client::builder().connect_timeout(Duration::from_secs(10));
    let local = reqwest::Url::parse(base_url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .is_some_and(|h| matches!(h.as_str(), "127.0.0.1" | "localhost" | "[::1]" | "::1"));
    if local {
        b = b.no_proxy();
    }
    b.build().unwrap_or_default()
}

pub(crate) fn join_url(base: &str, path: &str) -> String {
    format!(
        "{}/{}",
        base.trim_end_matches('/'),
        path.trim_start_matches('/')
    )
}

/// Never let a key reach the UI, even if a server echoes it back.
pub(crate) fn scrub(text: &str, key: Option<&str>) -> String {
    match key {
        Some(k) if !k.is_empty() => text.replace(k, "***"),
        _ => text.to_string(),
    }
}

pub(crate) fn connect_error(e: &reqwest::Error, key: Option<&str>) -> BackendError {
    let mut msg = e.to_string();
    let mut src = std::error::Error::source(e);
    while let Some(s) = src {
        msg.push_str(": ");
        msg.push_str(&s.to_string());
        src = s.source();
    }
    BackendError::Connect(scrub(&msg, key))
}

/// Turn a non-success response into an `Http` error with the server's own message when it gave one.
pub(crate) async fn http_error(resp: reqwest::Response, key: Option<&str>) -> BackendError {
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    let from_json = serde_json::from_str::<Value>(&body).ok().and_then(|v| {
        v.pointer("/error/message")
            .or_else(|| v.get("error").filter(|e| e.is_string()))
            .or_else(|| v.get("message"))
            .and_then(Value::as_str)
            .map(str::to_owned)
    });
    let message = match from_json {
        Some(m) => m,
        None if body.trim().is_empty() => status.canonical_reason().unwrap_or("error").to_string(),
        None => body.trim().chars().take(300).collect(),
    };
    BackendError::Http {
        status: status.as_u16(),
        message: scrub(&message, key),
    }
}

/// Maps one API's SSE events to `StreamEvent`s.
pub(crate) trait Mapper: Send + 'static {
    fn on_event(&mut self, ev: &SseEvent) -> Result<Vec<StreamEvent>, BackendError>;
    /// The API said the stream is over (e.g. `[DONE]`); stop reading.
    fn ended(&self) -> bool;
    /// Events owed at the end of the body (e.g. a `Done` that never came).
    fn on_eof(&mut self) -> Vec<StreamEvent>;
}

struct Drive<M> {
    body: BoxStream<'static, reqwest::Result<bytes::Bytes>>,
    parser: SseParser,
    mapper: M,
    queue: VecDeque<Result<StreamEvent, BackendError>>,
    finished: bool,
    key: Option<String>,
}

impl<M: Mapper> Drive<M> {
    fn take(&mut self, events: Vec<SseEvent>) {
        for ev in events {
            if self.finished {
                return;
            }
            match self.mapper.on_event(&ev) {
                Ok(v) => self.queue.extend(v.into_iter().map(Ok)),
                Err(e) => {
                    self.queue.push_back(Err(e));
                    self.finished = true;
                    return;
                }
            }
            if self.mapper.ended() {
                self.queue.extend(self.mapper.on_eof().into_iter().map(Ok));
                self.finished = true;
            }
        }
    }
}

/// Send `request`, then parse the event stream with `mapper`.
pub(crate) fn drive<M: Mapper>(
    request: reqwest::RequestBuilder,
    mapper: M,
    key: Option<String>,
) -> EventStream {
    let start = async move {
        let resp = match request.send().await {
            Ok(r) => r,
            Err(e) => return Err(connect_error(&e, key.as_deref())),
        };
        if !resp.status().is_success() {
            return Err(http_error(resp, key.as_deref()).await);
        }
        Ok(Drive {
            body: resp.bytes_stream().boxed(),
            parser: SseParser::new(),
            mapper,
            queue: VecDeque::new(),
            finished: false,
            key,
        })
    };
    stream::once(start)
        .flat_map(|r| match r {
            Err(e) => stream::iter([Err(e)]).boxed(),
            Ok(d) => stream::unfold(d, |mut d| async move {
                loop {
                    if let Some(x) = d.queue.pop_front() {
                        return Some((x, d));
                    }
                    if d.finished {
                        return None;
                    }
                    match d.body.next().await {
                        Some(Ok(bytes)) => {
                            let evs = d.parser.feed(&bytes);
                            d.take(evs);
                        }
                        Some(Err(e)) => {
                            let err = connect_error(&e, d.key.as_deref());
                            d.queue.push_back(Err(err));
                            d.finished = true;
                        }
                        None => {
                            let evs = d.parser.finish();
                            d.take(evs);
                            if !d.finished {
                                d.queue.extend(d.mapper.on_eof().into_iter().map(Ok));
                                d.finished = true;
                            }
                        }
                    }
                }
            })
            .boxed(),
        })
        .boxed()
}

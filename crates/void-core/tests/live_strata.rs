//! Live checks against a running Strata server (`strata launch --preset daily`).
//! Run with: cargo test -p void-core --test live_strata -- --ignored --nocapture

use futures_util::StreamExt;
use void_core::backend::{ChatRequest, StreamEvent, make_backend};
use void_core::config::{Config, Profile, resolve_api_key};
use void_core::model::Message;

async fn backend_for(id: &str) -> (Profile, std::sync::Arc<dyn void_core::backend::Backend>) {
    let profile = Config::default().profile(id).unwrap().clone();
    let key = resolve_api_key(&profile.api_key).await.expect("API key");
    let b = make_backend(&profile, key);
    (profile, b)
}

async fn pong(id: &str) {
    let (profile, b) = backend_for(id).await;
    let mut extra = profile.extra_body.clone();
    // Keep the live check quick: no thinking where the template supports turning it off.
    if extra.contains_key("chat_template_kwargs") {
        extra.insert(
            "chat_template_kwargs".into(),
            serde_json::json!({"enable_thinking": false}),
        );
    }
    let req = ChatRequest {
        model: profile.model.clone(),
        system: "Answer in one word.".into(),
        messages: vec![Message::user("Reply with the word pong.")],
        max_output: 256,
        extra_body: extra,
        tools: vec![],
    };
    let events: Vec<StreamEvent> = b
        .stream(req)
        .map(|r| r.expect("stream error"))
        .collect()
        .await;
    let text: String = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::TextDelta(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    println!("{id}: {text:?} ({} events)", events.len());
    assert!(text.to_lowercase().contains("pong"), "got {text:?}");
    assert!(
        events.iter().any(|e| matches!(e, StreamEvent::Usage(_))),
        "no usage"
    );
    assert!(
        matches!(events.last(), Some(StreamEvent::Done(_))),
        "no Done"
    );
}

#[tokio::test]
#[ignore]
async fn live_strata_openai_streams() {
    pong("strata").await;
}

#[tokio::test]
#[ignore]
async fn live_strata_anthropic_streams() {
    pong("strata-anthropic").await;
}

#[tokio::test]
#[ignore]
async fn live_strata_lists_models() {
    let (_, b) = backend_for("strata").await;
    let models = b.list_models().await.unwrap();
    println!("models: {models:?}");
    assert!(
        models.iter().any(|m| m == "qwen3.8-flash-next-iq3_xxs"),
        "got {models:?}"
    );
}

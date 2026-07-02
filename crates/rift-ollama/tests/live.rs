//! Live-server hardening checks against a real Ollama instance. Skipped
//! unless RIFT_LIVE_OLLAMA is set — run manually or in a nightly job:
//!
//!   RIFT_LIVE_OLLAMA=http://100.102.217.61:11434 RIFT_LIVE_MODEL=ornith:35b \
//!     cargo test -p rift-ollama --test live -- --nocapture
//!
//! Assertions are protocol-level (no errors, sane framing, stats present),
//! not behavioral — model output varies.

use rift_ollama::OllamaClient;
use rift_provider::{ChatOptions, ChatRequest, Message, Provider, StreamDelta, ToolDef};

fn live_target() -> Option<(String, String)> {
    let host = std::env::var("RIFT_LIVE_OLLAMA").ok()?;
    let model = std::env::var("RIFT_LIVE_MODEL").unwrap_or_else(|_| "ornith:35b".into());
    Some((host, model))
}

#[tokio::test]
async fn live_model_listing_and_capabilities() {
    let Some((host, model)) = live_target() else {
        eprintln!("skipped: set RIFT_LIVE_OLLAMA (and optionally RIFT_LIVE_MODEL)");
        return;
    };
    let client = OllamaClient::new(&host);
    let models = client.tags().await.expect("tags");
    assert!(!models.is_empty(), "server lists no models");
    let caps = client.show(&model).await.expect("show");
    assert!(caps.supports("tools"), "'{model}' lacks the tools capability rift requires");
}

#[tokio::test]
async fn live_stream_completes_with_stats() {
    let Some((host, model)) = live_target() else {
        eprintln!("skipped: set RIFT_LIVE_OLLAMA");
        return;
    };
    let client = OllamaClient::new(&host);
    let req = ChatRequest {
        model,
        messages: vec![Message::user("Reply with exactly the word: pong")],
        tools: vec![],
        stream: true,
        think: Some(false),
        keep_alive: Some("10m".into()),
        options: Some(ChatOptions { num_ctx: Some(4096), temperature: Some(0.0), num_predict: Some(50) }),
    };
    let mut streamed = String::new();
    let mut on_delta = |d: StreamDelta| {
        if let StreamDelta::Content(c) = d {
            streamed.push_str(&c);
        }
    };
    let outcome = client.chat_stream(&req, &mut on_delta).await.expect("chat_stream");
    assert!(!outcome.message.content.is_empty(), "empty completion");
    assert_eq!(outcome.message.content, streamed, "deltas must reassemble to the final message");
    assert!(outcome.done_reason.is_some(), "no done_reason");
    assert!(outcome.stats.eval_count > 0, "no eval stats");
    assert!(!outcome.truncation_suspected, "tiny prompt flagged as truncated");
}

#[tokio::test]
async fn live_tool_call_round_trip() {
    let Some((host, model)) = live_target() else {
        eprintln!("skipped: set RIFT_LIVE_OLLAMA");
        return;
    };
    let client = OllamaClient::new(&host);
    let tools = vec![ToolDef::function(
        "echo",
        "Echo the given text back verbatim. Always use this tool when asked to echo.",
        serde_json::json!({"type": "object", "required": ["text"], "properties": {"text": {"type": "string"}}}),
    )];
    let req = ChatRequest {
        model,
        messages: vec![Message::user("Use the echo tool to echo the text 'ping'.")],
        tools,
        stream: true,
        think: Some(false),
        keep_alive: Some("10m".into()),
        options: Some(ChatOptions { num_ctx: Some(4096), temperature: Some(0.0), num_predict: Some(200) }),
    };
    let mut on_delta = |_: StreamDelta| {};
    let outcome = client.chat_stream(&req, &mut on_delta).await.expect("chat_stream");
    // Protocol assertions only: IF the model called the tool, the call must
    // be well-formed (named, object arguments).
    for call in &outcome.message.tool_calls {
        assert_eq!(call.function.name, "echo", "unexpected tool name");
        assert!(call.function.arguments.contains_key("text"), "arguments missing 'text': {:?}", call.function.arguments);
    }
    if outcome.message.tool_calls.is_empty() {
        eprintln!("note: model answered without calling the tool (allowed, but weakens this check)");
    }
}

//! Live-server hardening checks against the real Anthropic API. Skipped
//! unless RIFT_LIVE_ANTHROPIC is set to an API key — run manually or in a
//! nightly job (these bill real tokens, a few cents per run):
//!
//!   RIFT_LIVE_ANTHROPIC=sk-ant-... cargo test -p rift-anthropic --test live -- --nocapture
//!
//! RIFT_LIVE_MODEL overrides the model (default claude-opus-4-8).
//! Assertions are protocol-level; model output varies.

use rift_anthropic::AnthropicClient;
use rift_provider::{ChatOptions, ChatRequest, Message, Provider, StreamDelta, ToolDef};

fn live_client() -> Option<(AnthropicClient, String)> {
    let key = std::env::var("RIFT_LIVE_ANTHROPIC").ok()?;
    let model = std::env::var("RIFT_LIVE_MODEL").unwrap_or_else(|_| "claude-opus-4-8".into());
    Some((AnthropicClient::new("https://api.anthropic.com", Some(key)), model))
}

#[tokio::test]
async fn live_model_listing_and_show() {
    let Some((client, model)) = live_client() else {
        eprintln!("skipped: set RIFT_LIVE_ANTHROPIC to an API key (and optionally RIFT_LIVE_MODEL)");
        return;
    };
    let models = client.tags().await.expect("GET /v1/models");
    assert!(!models.is_empty(), "no models listed");
    assert!(models.iter().any(|m| m.name == model), "'{model}' not in the listing");
    let caps = client.show(&model).await.expect("GET /v1/models/{id}");
    assert!(caps.supports("tools"), "'{model}' lacks tool support");
    assert!(caps.context_length.unwrap_or(0) >= 100_000, "implausible context window");
}

#[tokio::test]
async fn live_stream_completes_with_usage() {
    let Some((client, model)) = live_client() else {
        eprintln!("skipped: set RIFT_LIVE_ANTHROPIC");
        return;
    };
    let req = ChatRequest {
        model,
        messages: vec![Message::user("Reply with exactly the word: pong")],
        tools: vec![],
        stream: true,
        think: None,
        keep_alive: None,
        options: Some(ChatOptions { num_ctx: None, temperature: None, num_predict: Some(100) }),
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
    assert!(outcome.done_reason.is_some(), "no stop_reason");
    assert!(outcome.stats.prompt_eval_count > 0, "no input token usage");
    assert!(outcome.stats.eval_count > 0, "no output token usage");
}

#[tokio::test]
async fn live_tool_call_round_trip() {
    let Some((client, model)) = live_client() else {
        eprintln!("skipped: set RIFT_LIVE_ANTHROPIC");
        return;
    };
    let tools = vec![ToolDef::function(
        "echo",
        "Echo the given text back verbatim. Always use this tool when asked to echo.",
        serde_json::json!({"type": "object", "required": ["text"], "properties": {"text": {"type": "string"}}}),
    )];
    let req = ChatRequest {
        model: model.clone(),
        messages: vec![Message::user("Use the echo tool to echo the text 'ping'.")],
        tools: tools.clone(),
        stream: true,
        think: None,
        keep_alive: None,
        options: Some(ChatOptions { num_ctx: None, temperature: None, num_predict: Some(300) }),
    };
    let mut on_delta = |_: StreamDelta| {};
    let outcome = client.chat_stream(&req, &mut on_delta).await.expect("chat_stream (call)");
    assert!(!outcome.message.tool_calls.is_empty(), "model did not call the tool");
    let call = &outcome.message.tool_calls[0];
    assert_eq!(call.function.name, "echo");
    assert!(call.id.as_deref().unwrap_or("").starts_with("toolu_"), "unexpected id: {:?}", call.id);
    assert_eq!(outcome.done_reason.as_deref(), Some("tool_use"));

    // Round trip: answer the call and get a final completion — this exercises
    // the assistant echo (provider_data raw blocks) + merged tool_result path.
    let mut result = Message::tool_result("echo", "ping");
    result.tool_call_id = call.id.clone();
    let req2 = ChatRequest {
        model,
        messages: vec![
            Message::user("Use the echo tool to echo the text 'ping'."),
            outcome.message.clone(),
            result,
        ],
        tools,
        stream: true,
        think: None,
        keep_alive: None,
        options: Some(ChatOptions { num_ctx: None, temperature: None, num_predict: Some(300) }),
    };
    let outcome2 = client.chat_stream(&req2, &mut on_delta).await.expect("chat_stream (result)");
    assert!(!outcome2.message.content.is_empty(), "no final answer after the tool result");
    assert_ne!(outcome2.done_reason.as_deref(), Some("tool_use"), "model looped on tools");
}

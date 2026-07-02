//! Live-server hardening checks against a real OpenAI-compatible endpoint
//! (vLLM, LM Studio, llama.cpp server, LiteLLM, OpenRouter, or Ollama's own
//! /v1 shim). Skipped unless RIFT_LIVE_OPENAI is set:
//!
//!   RIFT_LIVE_OPENAI=http://100.102.217.61:11434/v1 RIFT_LIVE_MODEL=ornith:35b \
//!     cargo test -p rift-openai --test live -- --nocapture
//!
//! RIFT_LIVE_OPENAI_KEY supplies a bearer token when the server needs one.
//! Assertions are protocol-level; model output varies.

use rift_openai::OpenAiClient;
use rift_provider::{ChatOptions, ChatRequest, Message, Provider, StreamDelta, ToolDef};

fn live_client() -> Option<(OpenAiClient, String)> {
    let url = std::env::var("RIFT_LIVE_OPENAI").ok()?;
    let key = std::env::var("RIFT_LIVE_OPENAI_KEY").ok();
    let model = std::env::var("RIFT_LIVE_MODEL").unwrap_or_else(|_| "ornith:35b".into());
    Some((OpenAiClient::new(url, key), model))
}

#[tokio::test]
async fn live_model_listing() {
    let Some((client, _)) = live_client() else {
        eprintln!("skipped: set RIFT_LIVE_OPENAI (and optionally RIFT_LIVE_MODEL / RIFT_LIVE_OPENAI_KEY)");
        return;
    };
    let models = client.tags().await.expect("GET /models");
    assert!(!models.is_empty(), "server lists no models");
}

#[tokio::test]
async fn live_stream_completes_with_usage() {
    let Some((client, model)) = live_client() else {
        eprintln!("skipped: set RIFT_LIVE_OPENAI");
        return;
    };
    let req = ChatRequest {
        model,
        messages: vec![Message::user("Reply with exactly the word: pong")],
        tools: vec![],
        stream: true,
        think: None,
        keep_alive: None,
        options: Some(ChatOptions { num_ctx: None, temperature: Some(0.0), num_predict: Some(50) }),
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
    assert!(outcome.done_reason.is_some(), "no finish_reason");
    // usage comes via stream_options (or the retry path drops it — then 0 is
    // legal); either way the call itself must have succeeded.
    eprintln!(
        "usage: prompt={} completion={} (0s are legal on servers that reject stream_options)",
        outcome.stats.prompt_eval_count, outcome.stats.eval_count
    );
}

#[tokio::test]
async fn live_tool_call_round_trip() {
    let Some((client, model)) = live_client() else {
        eprintln!("skipped: set RIFT_LIVE_OPENAI");
        return;
    };
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
        think: None,
        keep_alive: None,
        options: Some(ChatOptions { num_ctx: None, temperature: Some(0.0), num_predict: Some(200) }),
    };
    let mut on_delta = |_: StreamDelta| {};
    let outcome = client.chat_stream(&req, &mut on_delta).await.expect("chat_stream");
    for call in &outcome.message.tool_calls {
        assert_eq!(call.function.name, "echo", "unexpected tool name");
        assert!(call.function.arguments.contains_key("text"), "arguments missing 'text': {:?}", call.function.arguments);
        // Correlation contract: if the server supplied an id it must be
        // non-empty (empty ids break tool-result pairing downstream).
        if let Some(id) = &call.id {
            assert!(!id.is_empty(), "server sent an empty tool-call id");
        }
    }
    if outcome.message.tool_calls.is_empty() {
        eprintln!("note: model answered without calling the tool (allowed, but weakens this check)");
    }
}

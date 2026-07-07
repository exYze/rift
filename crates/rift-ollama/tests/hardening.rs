//! Per-provider hardening suite (native Ollama /api/chat): NDJSON framing,
//! error lines, silent front-truncation detection — reproduced against a
//! mock server. The live-server variant is in `live.rs`.

use rift_ollama::OllamaClient;
use rift_provider::test_support::{MockResponse, MockServer};
use rift_provider::{ChatOptions, ChatRequest, ChatStats, Message, Provider, StreamDelta};

fn chat_req(num_ctx: Option<u64>) -> ChatRequest {
    ChatRequest {
        model: "test-model".into(),
        messages: vec![Message::user("hi")],
        tools: vec![],
        stream: true,
        think: None,
        effort: None,
        keep_alive: None,
        options: num_ctx.map(|n| ChatOptions { num_ctx: Some(n), temperature: None, num_predict: None }),
    }
}

struct Run {
    outcome: anyhow::Result<(Message, Option<String>, ChatStats, bool)>,
    deltas: Vec<StreamDelta>,
}

async fn run(server: &MockServer, req: &ChatRequest) -> Run {
    let client = OllamaClient::new(&server.base_url);
    let mut deltas = Vec::new();
    let mut on_delta = |d: StreamDelta| deltas.push(d);
    let outcome = client
        .chat_stream(req, &mut on_delta)
        .await
        .map(|o| (o.message, o.done_reason, o.stats, o.truncation_suspected));
    Run { outcome, deltas }
}

// NDJSON lines split across network reads must reassemble.
#[tokio::test]
async fn ndjson_reassembles_split_reads() {
    let server = MockServer::start(vec![MockResponse::stream(&[
        "{\"message\":{\"role\":\"assistant\",\"content\":\"Hel\"}}\n{\"message\":{\"role\":\"assist",
        "ant\",\"content\":\"lo\"}}\n",
        "{\"done\":true,\"done_reason\":\"stop\",\"prompt_eval_count\":11,\"eval_count\":2,\"eval_duration\":1000000}\n",
    ])])
    .await;
    let r = run(&server, &chat_req(None)).await;
    let (message, done_reason, stats, _) = r.outcome.expect("stream should succeed");
    assert_eq!(message.content, "Hello");
    assert_eq!(done_reason.as_deref(), Some("stop"));
    assert_eq!((stats.prompt_eval_count, stats.eval_count), (11, 2));
}

// A final line without a trailing newline (non-streaming responses, abrupt
// server flushes) must still be processed — it carries the stats.
#[tokio::test]
async fn final_line_without_newline_is_flushed() {
    let server = MockServer::start(vec![MockResponse::stream(&[
        "{\"message\":{\"role\":\"assistant\",\"content\":\"ok\"}}\n",
        "{\"done\":true,\"done_reason\":\"stop\",\"prompt_eval_count\":5,\"eval_count\":1}",
    ])])
    .await;
    let r = run(&server, &chat_req(None)).await;
    let (message, done_reason, stats, _) = r.outcome.expect("stream should succeed");
    assert_eq!(message.content, "ok");
    assert_eq!(done_reason.as_deref(), Some("stop"));
    assert_eq!(stats.prompt_eval_count, 5);
}

// Ollama reports failures as an `error` line mid-stream; it must fail the
// call with the server's message.
#[tokio::test]
async fn error_line_fails_the_call() {
    let server = MockServer::start(vec![MockResponse::stream(&[
        "{\"message\":{\"role\":\"assistant\",\"content\":\"par\"}}\n",
        "{\"error\":\"model runner has unexpectedly stopped\"}\n",
    ])])
    .await;
    let r = run(&server, &chat_req(None)).await;
    let err = r.outcome.expect_err("error line must fail the stream");
    assert!(format!("{err:#}").contains("model runner"), "got: {err:#}");
}

// Ollama silently front-truncates prompts over num_ctx; a prompt_eval_count
// within ~2% of num_ctx is the only signal. Detection must fire there — and
// must NOT fire on a comfortably smaller prompt.
#[tokio::test]
async fn silent_front_truncation_is_detected() {
    for (prompt_eval, expect) in [(995u64, true), (400u64, false)] {
        let done = format!("{{\"done\":true,\"done_reason\":\"stop\",\"prompt_eval_count\":{prompt_eval},\"eval_count\":1}}\n");
        let server = MockServer::start(vec![MockResponse::stream(&[
            "{\"message\":{\"role\":\"assistant\",\"content\":\"x\"}}\n",
            &done,
        ])])
        .await;
        let r = run(&server, &chat_req(Some(1000))).await;
        let (.., truncation_suspected) = r.outcome.expect("stream should succeed");
        assert_eq!(
            truncation_suspected, expect,
            "prompt_eval_count={prompt_eval} vs num_ctx=1000 should give {expect}"
        );
    }
}

// Native-API tool calls arrive whole in one chunk; they surface as deltas
// and accumulate into the returned message.
#[tokio::test]
async fn tool_calls_arrive_whole() {
    let server = MockServer::start(vec![MockResponse::stream(&[
        "{\"message\":{\"role\":\"assistant\",\"content\":\"\",\"tool_calls\":[{\"function\":{\"name\":\"read\",\"arguments\":{\"path\":\"a.rs\"}}}]}}\n",
        "{\"done\":true,\"done_reason\":\"stop\"}\n",
    ])])
    .await;
    let r = run(&server, &chat_req(None)).await;
    let (message, ..) = r.outcome.expect("stream should succeed");
    assert_eq!(message.tool_calls.len(), 1);
    assert_eq!(message.tool_calls[0].function.name, "read");
    assert_eq!(message.tool_calls[0].function.arguments["path"], "a.rs");
    assert!(r.deltas.iter().any(|d| matches!(d, StreamDelta::ToolCall(_))));
}

// Non-2xx from a reverse proxy is often an HTML page with no JSON error —
// the HTTP status must survive into the error message.
#[tokio::test]
async fn proxy_error_keeps_the_status_code() {
    let server = MockServer::start(vec![MockResponse {
        status: 502,
        content_type: "text/html",
        chunks: vec!["<html><body>Bad Gateway</body></html>".into()],
    }])
    .await;
    let client = OllamaClient::new(&server.base_url);
    let err = format!("{:#}", client.tags().await.expect_err("502 must fail"));
    assert!(err.contains("502"), "status missing: {err}");
}

// Effort levels use Ollama's string `think` form and win over the bool.
#[tokio::test]
async fn effort_becomes_string_think() {
    let server = MockServer::start(vec![MockResponse::stream(&[
        "{\"message\":{\"role\":\"assistant\",\"content\":\"ok\"},\"done\":true,\"done_reason\":\"stop\"}\n",
    ])])
    .await;
    let mut req = chat_req(None);
    req.think = Some(true);
    req.effort = Some("high".into());
    let r = run(&server, &req).await;
    r.outcome.expect("stream should succeed");
    let raw = &server.requests().await[0];
    assert!(raw.contains("\"think\":\"high\""), "think must carry the level string: {raw}");
    assert!(!raw.contains("\"effort\""), "the neutral effort field must not leak to the wire: {raw}");
}

//! Per-provider hardening suite (native Anthropic Messages API): SSE event
//! framing across split reads, input_json_delta accumulation, mid-stream
//! error events, thinking-block round-trips, and wire-shape assertions —
//! reproduced against a mock server. The live variant is in `live.rs`.

use rift_anthropic::AnthropicClient;
use rift_provider::test_support::{MockResponse, MockServer};
use rift_provider::{ChatOptions, ChatRequest, ChatStats, Message, Provider, StreamDelta};

fn chat_req(think: Option<bool>) -> ChatRequest {
    ChatRequest {
        model: "claude-opus-4-8".into(),
        messages: vec![Message::system("be brief"), Message::user("hi")],
        tools: vec![],
        stream: true,
        think,
        keep_alive: None,
        options: Some(ChatOptions { num_ctx: None, temperature: Some(0.0), num_predict: Some(500) }),
    }
}

struct Run {
    outcome: anyhow::Result<(Message, Option<String>, ChatStats)>,
    deltas: Vec<StreamDelta>,
}

async fn run(server: &MockServer, req: &ChatRequest) -> Run {
    let client = AnthropicClient::new(&server.base_url, Some("sk-test-key".into()));
    let mut deltas = Vec::new();
    let mut on_delta = |d: StreamDelta| deltas.push(d);
    let outcome = client
        .chat_stream(req, &mut on_delta)
        .await
        .map(|o| (o.message, o.done_reason, o.stats));
    Run { outcome, deltas }
}

// SSE events split mid-JSON across network reads must reassemble; text
// accumulates in order and usage/stop_reason survive.
#[tokio::test]
async fn stream_reassembles_across_split_reads() {
    let server = MockServer::start(vec![MockResponse::stream(&[
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":42}}}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hel",
        "lo\"}}\n\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" world\"}}\n\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":7}}\n\ndata: {\"type\":\"message_stop\"}\n\n",
    ])])
    .await;
    let r = run(&server, &chat_req(None)).await;
    let (message, done_reason, stats) = r.outcome.expect("stream should succeed");
    assert_eq!(message.content, "Hello world");
    assert_eq!(done_reason.as_deref(), Some("end_turn"));
    assert_eq!((stats.prompt_eval_count, stats.eval_count), (42, 7));
}

// The request wire shape: required headers, system lifted to the top level,
// max_tokens present, thinking omitted unless requested.
#[tokio::test]
async fn request_carries_headers_system_and_max_tokens() {
    let server = MockServer::start(vec![MockResponse::stream(&[
        "data: {\"type\":\"message_stop\"}\n\n",
    ])])
    .await;
    let _ = run(&server, &chat_req(None)).await;
    let requests = server.requests().await;
    assert_eq!(requests.len(), 1);
    let raw = &requests[0];
    assert!(raw.contains("POST /v1/messages"), "wrong path: {raw}");
    assert!(raw.to_lowercase().contains("x-api-key: sk-test-key"), "missing api key header");
    assert!(raw.to_lowercase().contains("anthropic-version: 2023-06-01"), "missing version header");
    assert!(raw.contains("\"system\":\"be brief\""), "system not lifted: {raw}");
    assert!(raw.contains("\"max_tokens\":500"), "max_tokens missing: {raw}");
    assert!(!raw.contains("\"thinking\""), "thinking must be omitted when not requested");
}

// Tool use: id+name from content_block_start, arguments accumulated from
// input_json_delta fragments, surfaced whole on content_block_stop.
#[tokio::test]
async fn tool_use_input_accumulates_from_json_deltas() {
    let server = MockServer::start(vec![MockResponse::stream(&[
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_abc\",\"name\":\"bash\"}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"comm\"}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"and\\\": \\\"ls\\\"}\"}}\n\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":3}}\n\ndata: {\"type\":\"message_stop\"}\n\n",
    ])])
    .await;
    let r = run(&server, &chat_req(None)).await;
    let (message, done_reason, _) = r.outcome.expect("stream should succeed");
    assert_eq!(done_reason.as_deref(), Some("tool_use"));
    assert_eq!(message.tool_calls.len(), 1);
    assert_eq!(message.tool_calls[0].id.as_deref(), Some("toolu_abc"));
    assert_eq!(message.tool_calls[0].function.arguments["command"], "ls");
    assert!(r.deltas.iter().any(|d| matches!(d, StreamDelta::ToolCall(_))));
}

// Thinking blocks stream as Thinking deltas and round-trip their signature
// through provider_data (the API validates it on replay).
#[tokio::test]
async fn thinking_streams_and_signature_survives_in_provider_data() {
    let server = MockServer::start(vec![MockResponse::stream(&[
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"pondering\"}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sigXYZ\"}}\n\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"answer\"}}\n\ndata: {\"type\":\"message_stop\"}\n\n",
    ])])
    .await;
    let r = run(&server, &chat_req(Some(true))).await;
    let (message, ..) = r.outcome.expect("stream should succeed");
    assert_eq!(message.thinking.as_deref(), Some("pondering"));
    assert_eq!(message.content, "answer");
    assert!(r.deltas.iter().any(|d| matches!(d, StreamDelta::Thinking(t) if t == "pondering")));
    let raw = message.provider_data.expect("raw blocks must be kept");
    let blocks = raw["content"].as_array().unwrap();
    assert_eq!(blocks[0]["type"], "thinking");
    assert_eq!(blocks[0]["signature"], "sigXYZ");
    // And the request asked for adaptive thinking (never disabled/budget).
    // serde_json sorts object keys, so match fields independently.
    let requests = server.requests().await;
    assert!(requests[0].contains("\"thinking\""), "thinking not requested");
    assert!(requests[0].contains("\"type\":\"adaptive\""), "adaptive not requested");
    assert!(!requests[0].contains("temperature"), "sampling params must drop with thinking on");
}

// Mid-stream `error` events (overloads) must fail the call loudly.
#[tokio::test]
async fn mid_stream_error_event_fails_the_call() {
    let server = MockServer::start(vec![MockResponse::stream(&[
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n",
    ])])
    .await;
    let r = run(&server, &chat_req(None)).await;
    let err = r.outcome.expect_err("error event must fail the stream");
    assert!(format!("{err:#}").contains("Overloaded"), "got: {err:#}");
}

// Non-2xx responses surface the status and the API's error message shape.
#[tokio::test]
async fn http_error_includes_status_and_message() {
    let server = MockServer::start(vec![MockResponse::json(
        529,
        "{\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"server overloaded\"}}",
    )])
    .await;
    let r = run(&server, &chat_req(None)).await;
    let err = format!("{:#}", r.outcome.expect_err("529 must fail"));
    assert!(err.contains("529"), "status missing: {err}");
    assert!(err.contains("server overloaded"), "message missing: {err}");
}

// A missing API key fails fast with guidance, before any network call.
#[tokio::test]
async fn missing_api_key_fails_with_guidance() {
    let client = AnthropicClient::new("http://127.0.0.1:9", None);
    let mut on_delta = |_: StreamDelta| {};
    let err = client
        .chat_stream(&chat_req(None), &mut on_delta)
        .await
        .expect_err("no key must fail");
    assert!(format!("{err:#}").contains("ANTHROPIC_API_KEY"), "got: {err:#}");
}

// GET /v1/models maps into neutral ModelEntry values.
#[tokio::test]
async fn model_listing_maps_ids() {
    let server = MockServer::start(vec![MockResponse::json(
        200,
        "{\"data\":[{\"id\":\"claude-opus-4-8\",\"display_name\":\"Claude Opus 4.8\"},{\"id\":\"claude-sonnet-5\"}]}",
    )])
    .await;
    let client = AnthropicClient::new(&server.base_url, Some("sk-test-key".into()));
    let models = client.tags().await.expect("tags");
    let names: Vec<&str> = models.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(names, ["claude-opus-4-8", "claude-sonnet-5"]);
}

// GET /v1/models/{id} supplies the context window and capabilities.
#[tokio::test]
async fn show_maps_context_window_and_capabilities() {
    let server = MockServer::start(vec![MockResponse::json(
        200,
        "{\"id\":\"claude-opus-4-8\",\"max_input_tokens\":1000000,\"max_tokens\":128000,\"capabilities\":{\"thinking\":{\"supported\":true}}}",
    )])
    .await;
    let client = AnthropicClient::new(&server.base_url, Some("sk-test-key".into()));
    let caps = client.show("claude-opus-4-8").await.expect("show");
    assert!(caps.supports("tools"));
    assert!(caps.supports("thinking"));
    assert_eq!(caps.context_length, Some(1_000_000));
}

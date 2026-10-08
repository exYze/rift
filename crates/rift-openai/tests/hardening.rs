//! Per-provider hardening suite (OpenAI-compat): the streaming quirks and
//! failure modes observed across vLLM, LM Studio, llama.cpp server, LiteLLM,
//! OpenRouter, and Ollama's /v1 shim, reproduced deterministically against a
//! mock server. The live-server variant of these checks is in `live.rs`.

use rift_openai::OpenAiClient;
use rift_provider::test_support::{MockResponse, MockServer};
use rift_provider::{ChatOptions, ChatRequest, ChatStats, Message, Provider, StreamDelta, ToolCall};

fn chat_req() -> ChatRequest {
    ChatRequest {
        model: "test-model".into(),
        messages: vec![Message::user("hi")],
        tools: vec![],
        stream: true,
        think: None,
        effort: None,
        keep_alive: None,
        options: Some(ChatOptions { num_ctx: None, temperature: Some(0.0), num_predict: None }),
    }
}

struct Run {
    outcome: anyhow::Result<(Message, Option<String>, ChatStats)>,
    deltas: Vec<StreamDelta>,
}

async fn run(server: &MockServer, req: &ChatRequest) -> Run {
    let client = OpenAiClient::new(&server.base_url, None);
    let mut deltas = Vec::new();
    let mut on_delta = |d: StreamDelta| deltas.push(d);
    let outcome = client
        .chat_stream(req, &mut on_delta)
        .await
        .map(|o| (o.message, o.done_reason, o.stats));
    Run { outcome, deltas }
}

fn content_of(deltas: &[StreamDelta]) -> String {
    deltas
        .iter()
        .filter_map(|d| match d {
            StreamDelta::Content(c) => Some(c.as_str()),
            _ => None,
        })
        .collect()
}

fn tool_calls_of(deltas: &[StreamDelta]) -> Vec<ToolCall> {
    deltas
        .iter()
        .filter_map(|d| match d {
            StreamDelta::ToolCall(c) => Some(c.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn requests_identify_rift_and_keep_bearer_auth() {
    let server = MockServer::start(vec![
        MockResponse::json(200, r#"{"data":[{"id":"test-model"}]}"#),
        MockResponse::json(
            200,
            r#"{"data":[{"id":"test-model","context_length":4096}]}"#,
        ),
        MockResponse::stream(&[
            "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n",
        ]),
    ])
    .await;
    let client = OpenAiClient::new(&server.base_url, Some("fake-api-key".into()));

    client.tags().await.expect("tags should succeed");
    client
        .show("test-model")
        .await
        .expect("show should succeed");
    let mut on_delta = |_| {};
    client
        .chat_stream(&chat_req(), &mut on_delta)
        .await
        .expect("chat should succeed");

    let requests = server.requests().await;
    assert_eq!(requests.len(), 3);
    let expected_user_agent = format!("user-agent: rift/{}", env!("CARGO_PKG_VERSION"));
    for (request, path) in requests.iter().zip([
        "GET /v1/models",
        "GET /v1/models",
        "POST /v1/chat/completions",
    ]) {
        let headers = request
            .split("\r\n\r\n")
            .next()
            .unwrap_or_default()
            .to_lowercase();
        assert!(
            request.starts_with(path),
            "unexpected request path: {request}"
        );
        assert!(
            headers.lines().any(|line| line == expected_user_agent),
            "missing expected User-Agent in: {request}"
        );
        assert!(
            headers
                .lines()
                .any(|line| line == "authorization: bearer fake-api-key"),
            "missing bearer auth in: {request}"
        );
    }
}

// An SSE event split mid-JSON across network reads must reassemble; content
// arrives in order.
#[tokio::test]
async fn content_reassembles_across_split_reads() {
    let server = MockServer::start(vec![MockResponse::stream(&[
        "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"lo\"",
        "}}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\" world\"}}]}\n\n",
        "data: [DONE]\n\n",
    ])])
    .await;
    let r = run(&server, &chat_req()).await;
    let (message, ..) = r.outcome.expect("stream should succeed");
    assert_eq!(message.content, "Hello world");
    assert_eq!(content_of(&r.deltas), "Hello world");
}

// Several llama.cpp/LiteLLM configs close the stream without `data: [DONE]`
// and without a trailing newline; the final event usually carries
// finish_reason and usage — it must not be dropped.
#[tokio::test]
async fn missing_done_sentinel_still_flushes_final_event() {
    let server = MockServer::start(vec![MockResponse::stream(&[
        "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":3}}",
    ])])
    .await;
    let r = run(&server, &chat_req()).await;
    let (message, done_reason, stats) = r.outcome.expect("stream should succeed");
    assert_eq!(message.content, "ok");
    assert_eq!(done_reason.as_deref(), Some("stop"));
    assert_eq!((stats.prompt_eval_count, stats.eval_count), (7, 3));
}

// Mid-stream `error` events (OpenRouter/LiteLLM rate limits, upstream
// failures) must fail the call loudly, not return a truncated message.
#[tokio::test]
async fn mid_stream_error_event_fails_the_call() {
    let server = MockServer::start(vec![MockResponse::stream(&[
        "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n",
        "data: {\"error\":{\"message\":\"rate limit exceeded\",\"code\":429}}\n\n",
    ])])
    .await;
    let r = run(&server, &chat_req()).await;
    let err = r.outcome.expect_err("error event must fail the stream");
    assert!(format!("{err:#}").contains("rate limit exceeded"), "got: {err:#}");
}

// Streamed tool calls: id+name in the first fragment, arguments accumulated
// by index across fragments, surfaced whole with parsed arguments.
#[tokio::test]
async fn tool_call_fragments_accumulate_by_index() {
    let server = MockServer::start(vec![MockResponse::stream(&[
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_9\",\"function\":{\"name\":\"bash\",\"arguments\":\"{\\\"comm\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"and\\\":\\\"ls\\\"}\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n",
    ])])
    .await;
    let r = run(&server, &chat_req()).await;
    let (message, done_reason, _) = r.outcome.expect("stream should succeed");
    assert_eq!(done_reason.as_deref(), Some("tool_calls"));
    assert_eq!(message.tool_calls.len(), 1);
    let call = &message.tool_calls[0];
    assert_eq!(call.id.as_deref(), Some("call_9"));
    assert_eq!(call.function.name, "bash");
    assert_eq!(call.function.arguments["command"], "ls");
    assert_eq!(tool_calls_of(&r.deltas).len(), 1);
}

// Servers that stream tool calls without ids: the call must surface with
// id=None (the agent synthesizes one before it enters history).
#[tokio::test]
async fn id_less_tool_call_surfaces_with_none_id() {
    let server = MockServer::start(vec![MockResponse::stream(&[
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"name\":\"read\",\"arguments\":\"{}\"}}]}}]}\n\n",
        "data: [DONE]\n\n",
    ])])
    .await;
    let r = run(&server, &chat_req()).await;
    let (message, ..) = r.outcome.expect("stream should succeed");
    assert_eq!(message.tool_calls.len(), 1);
    assert!(message.tool_calls[0].id.is_none());
}

// Older servers 400 on stream_options wholesale; one retry without the
// parameter must transparently recover.
#[tokio::test]
async fn stream_options_rejection_retries_without_it() {
    let server = MockServer::start(vec![
        MockResponse::json(400, "{\"error\":{\"message\":\"unrecognized parameter: stream_options\"}}"),
        MockResponse::stream(&["data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n"]),
    ])
    .await;
    let r = run(&server, &chat_req()).await;
    let (message, ..) = r.outcome.expect("retry without stream_options should succeed");
    assert_eq!(message.content, "ok");
    let requests = server.requests().await;
    assert_eq!(requests.len(), 2);
    assert!(requests[0].contains("stream_options"));
    assert!(!requests[1].contains("stream_options"), "retry must drop stream_options");
}

// Arguments cut off by the token limit must error with the tool named and a
// truncation hint — never silently execute with empty arguments.
#[tokio::test]
async fn truncated_tool_arguments_error_with_context() {
    let server = MockServer::start(vec![MockResponse::stream(&[
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"write\",\"arguments\":\"{\\\"path\\\": \\\"a.\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}]}\n\ndata: [DONE]\n\n",
    ])])
    .await;
    let r = run(&server, &chat_req()).await;
    let err = r.outcome.expect_err("truncated arguments must fail");
    let msg = format!("{err:#}");
    assert!(msg.contains("write"), "must name the tool: {msg}");
    assert!(msg.contains("truncated"), "must hint at the token limit: {msg}");
}

// A wire-controlled tool_calls index must be rejected, not allocated.
#[tokio::test]
async fn absurd_tool_call_index_is_rejected() {
    let server = MockServer::start(vec![MockResponse::stream(&[
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":4000000000,\"function\":{\"name\":\"x\"}}]}}]}\n\n",
    ])])
    .await;
    let r = run(&server, &chat_req()).await;
    let err = r.outcome.expect_err("absurd index must fail");
    assert!(format!("{err:#}").contains("index"), "got: {err:#}");
}

// Non-2xx responses surface the status and the server's error message.
#[tokio::test]
async fn http_error_includes_status_and_message() {
    for (status, message) in [
        (401, "invalid API key"),
        (402, "quota exhausted"),
        (500, "backend exploded"),
    ] {
        let body = serde_json::json!({"error": {"message": message}}).to_string();
        let server = MockServer::start(vec![
            MockResponse::json(status, &body),
            MockResponse::stream(&[
                "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n",
            ]),
        ])
        .await;
        let r = run(&server, &chat_req()).await;
        let err = format!("{:#}", r.outcome.expect_err("HTTP error must fail"));
        assert!(err.contains(&status.to_string()), "status missing: {err}");
        assert!(err.contains(message), "message missing: {err}");
        assert_eq!(
            server.requests().await.len(),
            1,
            "HTTP errors must not retry"
        );
    }
}

// Reasoning deltas (DeepSeek-style `reasoning_content`) stream as thinking,
// separate from content.
#[tokio::test]
async fn reasoning_streams_as_thinking() {
    let server = MockServer::start(vec![MockResponse::stream(&[
        "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"pondering\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"answer\"}}]}\n\ndata: [DONE]\n\n",
    ])])
    .await;
    let r = run(&server, &chat_req()).await;
    let (message, ..) = r.outcome.expect("stream should succeed");
    assert_eq!(message.thinking.as_deref(), Some("pondering"));
    assert_eq!(message.content, "answer");
    assert!(r.deltas.iter().any(|d| matches!(d, StreamDelta::Thinking(t) if t == "pondering")));
}

// Effort levels travel as `reasoning_effort` + an explicit thinking toggle,
// sampling params are dropped in thinking mode, and the assistant's prior
// reasoning_content is passed back (DeepSeek requires it in tool loops).
#[tokio::test]
async fn effort_and_reasoning_content_reach_the_wire() {
    let server = MockServer::start(vec![MockResponse::stream(&[
        "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n",
    ])])
    .await;
    let mut req = chat_req();
    req.think = Some(true);
    req.effort = Some("max".into());
    let mut prior = Message {
        role: rift_provider::Role::Assistant,
        content: "step one done".into(),
        thinking: Some("earlier reasoning".into()),
        tool_calls: vec![],
        tool_name: None,
        tool_call_id: None,
        provider_data: None,
        images: vec![],
    };
    prior.thinking = Some("earlier reasoning".into());
    req.messages.push(prior);
    let r = run(&server, &req).await;
    r.outcome.expect("stream should succeed");
    let raw = &server.requests().await[0];
    assert!(raw.contains("\"reasoning_effort\":\"max\""), "missing reasoning_effort: {raw}");
    assert!(raw.contains("\"thinking\":{\"type\":\"enabled\"}"), "missing thinking toggle: {raw}");
    assert!(
        raw.contains("\"chat_template_kwargs\":{\"reasoning_effort\":\"max\",\"thinking\":true}"),
        "missing vLLM chat_template_kwargs form: {raw}"
    );
    assert!(raw.contains("\"reasoning_content\":\"earlier reasoning\""), "reasoning not passed back: {raw}");
    assert!(!raw.contains("\"temperature\""), "sampling params must drop in thinking mode: {raw}");
}

// Servers that reject reasoning params get one retry without them, so an
// explicitly-set effort degrades gracefully instead of failing the turn.
#[tokio::test]
async fn reasoning_params_stripped_and_retried_on_400() {
    let server = MockServer::start(vec![
        MockResponse::json(400, "{\"error\":{\"message\":\"Unrecognized request argument: reasoning_effort\"}}"),
        MockResponse::stream(&["data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n"]),
    ])
    .await;
    let mut req = chat_req();
    req.effort = Some("high".into());
    let r = run(&server, &req).await;
    let (message, ..) = r.outcome.expect("retry without reasoning params should succeed");
    assert_eq!(message.content, "ok");
    let reqs = server.requests().await;
    assert_eq!(reqs.len(), 2);
    assert!(reqs[0].contains("reasoning_effort"));
    assert!(!reqs[1].contains("reasoning_effort"), "retry must strip the rejected param: {}", reqs[1]);
}

// Vision attachments: user messages with images become content-part arrays
// (text + image_url with the data URL intact).
#[tokio::test]
async fn image_attachments_become_content_parts() {
    let server = MockServer::start(vec![MockResponse::stream(&[
        "data: {\"choices\":[{\"delta\":{\"content\":\"a cat\"}}]}\n\ndata: [DONE]\n\n",
    ])])
    .await;
    let mut req = chat_req();
    let mut msg = Message::user("what is in this picture?");
    msg.images = vec!["data:image/png;base64,AAAABBBB".into()];
    req.messages = vec![msg];
    let r = run(&server, &req).await;
    r.outcome.expect("stream should succeed");
    let raw = &server.requests().await[0];
    assert!(raw.contains("\"type\":\"text\""), "text part missing: {raw}");
    assert!(
        raw.contains("\"image_url\":{\"url\":\"data:image/png;base64,AAAABBBB\"}"),
        "image_url part missing: {raw}"
    );
}

// Empty replies, including reasoning-only replies whose old thinking the
// agent strips, must not poison the next turn or a resumed session.
#[tokio::test]
async fn empty_stream_reply_is_not_replayed_on_next_turn() {
    for thinking in [None, Some("unfinished reasoning")] {
        let first_stream = format!(
            "data: {}\n\ndata: [DONE]\n\n",
            serde_json::json!({"choices": [{"delta": {
                "reasoning_content": thinking,
            }, "finish_reason": "stop"}]}),
        );
        let server = MockServer::start(vec![
            MockResponse::stream(&[&first_stream]),
            MockResponse::stream(&[
                "data: {\"choices\":[{\"delta\":{\"content\":\"recovered\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
            ]),
        ]).await;
        let client = OpenAiClient::new(format!("{}/coding/v1", server.base_url), None);
        let mut req = chat_req();
        req.model = "kimi-for-coding".into();
        let first = client.chat_stream(&req, &mut |_| {}).await.unwrap();
        assert!(first.message.content.is_empty());
        assert!(first.message.tool_calls.is_empty());
        assert_eq!(first.message.thinking.as_deref(), thinking);
        req.messages.push(first.message);
        req.messages.push(Message::user("continue"));
        // Session save/load retains the empty assistant, and request building
        // in the agent removes thinking from turns before the latest user.
        req.messages =
            serde_json::from_value(serde_json::to_value(&req.messages).unwrap()).unwrap();
        req.messages[1].thinking = None;
        let next = client.chat_stream(&req, &mut |_| {}).await.unwrap();
        assert_eq!(next.message.content, "recovered");
        let requests = server.requests().await;
        assert_eq!(requests.len(), 2, "must recover without a 400 retry");
        assert!(requests[1].starts_with("POST /coding/v1/chat/completions "));
        let body: serde_json::Value =
            serde_json::from_str(requests[1].split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(
            body["messages"],
            serde_json::json!([
                {"role": "user", "content": "hi"},
                {"role": "user", "content": "continue"},
            ]),
            "thinking={thinking:?}"
        );
        assert_eq!(req.messages.len(), 3, "saved history must remain intact");
    }
}

// A mock OpenAI-compatible server exercises a nested API root.
#[tokio::test]
async fn reasoning_and_tool_history_round_trip_on_nested_api_root() {
    let server = MockServer::start(vec![
        MockResponse::json(200, r#"{"data":[{"id":"test-model"}]}"#),
        MockResponse::stream(&[
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"inspect the file\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\\\"main.rs\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n",
        ]),
        MockResponse::stream(&[
            "data: {\"choices\":[{\"delta\":{\"content\":\"finished\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
        ]),
    ]).await;
    let client = OpenAiClient::new(
        format!("{}/coding/v1", server.base_url),
        Some("fake-api-key".into()),
    );
    assert_eq!(client.tags().await.unwrap()[0].name, "test-model");
    let mut req = chat_req();
    let mut deltas = Vec::new();
    let first = client
        .chat_stream(&req, &mut |d| deltas.push(d))
        .await
        .unwrap();
    assert_eq!(first.message.thinking.as_deref(), Some("inspect the file"));
    assert!(deltas
        .iter()
        .any(|d| matches!(d, StreamDelta::Thinking(t) if t == "inspect the file")));
    assert_eq!(first.message.tool_calls[0].id.as_deref(), Some("call_1"));
    assert_eq!(
        first.message.tool_calls[0].function.arguments["path"],
        "main.rs"
    );
    let mut result = Message::tool_result("read", "fn main() {}");
    result.tool_call_id = first.message.tool_calls[0].id.clone();
    req.messages.push(first.message);
    req.messages.push(result);
    let final_reply = client.chat_stream(&req, &mut |_| {}).await.unwrap();
    assert_eq!(final_reply.message.content, "finished");
    let requests = server.requests().await;
    assert_eq!(requests.len(), 3);
    for (raw, line) in requests.iter().zip([
        "GET /coding/v1/models HTTP/1.1",
        "POST /coding/v1/chat/completions HTTP/1.1",
        "POST /coding/v1/chat/completions HTTP/1.1",
    ]) {
        let head = raw.split("\r\n\r\n").next().unwrap().to_lowercase();
        assert_eq!(raw.lines().next().unwrap(), line);
        assert!(head
            .lines()
            .any(|h| h == "authorization: bearer fake-api-key"));
        assert!(head
            .lines()
            .any(|h| h == format!("user-agent: rift/{}", env!("CARGO_PKG_VERSION"))));
    }
    for raw in &requests[1..] {
        let body: serde_json::Value =
            serde_json::from_str(raw.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(body["model"], "test-model");
    }
    let replay: serde_json::Value =
        serde_json::from_str(requests[2].split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(
        replay["messages"][1]["reasoning_content"],
        "inspect the file"
    );
    assert_eq!(replay["messages"][1]["tool_calls"][0]["id"], "call_1");
    assert_eq!(replay["messages"][2]["role"], "tool");
    assert_eq!(replay["messages"][2]["tool_call_id"], "call_1");
}

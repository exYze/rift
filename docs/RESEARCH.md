# Research digest (verified 2026-06-10)

## Ollama native /api/chat protocol — verified live against localhost:11434

- Tools: `tools: [{type:"function", function:{name, description, parameters:<JSON Schema>}}]`.
- Tool results: `{"role":"tool", "tool_name":"<name>", "content":"..."}` — correlation by NAME, not id. The native API historically has no tool-call ids, but **this server returns `id: "call_xxx"` on tool calls** (newer Ollama) — modeled as `Option<String>`, preserved round-trip.
- `function.arguments` is a **parsed JSON object**, never a string, never partial.
- Streaming (NDJSON): `thinking` and `content` stream as fragments in `message`; each tool call arrives **whole in a single chunk**. Final chunk: `done:true, done_reason, prompt_eval_count, eval_count, *_duration` (ns).
- `think`: top-level bool (string enum "low|med|high" for gpt-oss). Thinking models default ON. Sending `think:true` to a non-thinking model = 400.
- Capabilities: `/api/show {model}` → `capabilities: ["completion","tools","thinking",...]`; this server's `/api/tags` includes capabilities too (newer build). `model_info` has `<arch>.context_length`.
- **Silent truncation pitfall**: default `num_ctx` 4096; prompts exceeding it are truncated FROM THE FRONT with no error (drops system prompt + tool schemas → tool calling silently degrades). This was the #1 root cause of opencode-on-Ollama complaints. Mitigation implemented: explicit `options.num_ctx` per request + compare `prompt_eval_count` ≈ num_ctx to detect.
- Failure mode: some models emit tool calls as plain JSON text in `content` (template parsing misses). Mitigation implemented: `extract_textual_tool_calls` fallback, known-names-only.
- `format` (structured outputs) conflicts with tools — don't combine in one request.
- Changing `num_ctx` between requests forces model reload (slow first call). `keep_alive: "10m"` used.

## Server inventory (localhost:11434)

Tools-capable models: gemma4:12b (tools,thinking,vision), gemma4:26b (tools,thinking), gemma4:31b, qwen3.6:27b, qwen3.6:35b (vision,tools,thinking), nemotron3:33b. Embeddings: nomic-embed-text:v1.5. gemma4:26b verified working end-to-end (62 tok/s eval).

## opencode architecture notes (github.com/anomalyco/opencode)

- Client/server split: TS/Bun server owns state; TUI (OpenTUI, TS+Zig) is a thin SSE-driven renderer. v1.0 TUI rewrite caused a long tail of rendering regressions.
- Tools: read (2000 lines/50KB caps), edit (exact-string + per-path lock), write (returns LSP diagnostics), grep (ripgrep, 100-match cap), glob, bash (permission globs), apply_patch, task/subagents.
- Compaction: layered — prune old tool outputs first (keep last 2 turns, only if >40k tok reclaimable ≥20k), then LLM summarization with fixed 5-heading structure; last user message replayed after compaction.
- Local models go through OpenAI-compat shim — root cause of many issues (streamed tool_call deltas unparsed #20995, tool-name mismatch #21354, textual tool JSON #1034). We use the native API instead.
- TUI complaint cluster: auto-scroll fights user scroll during streaming (#16622, #7648), corrupted panes (#11109), tmux rendering stalls (#16566). Our fix: pre-wrapped flat line buffer, bottom-anchored offset, appended lines advance the offset when scrolled up.
- MCP: local (stdio spawn) + remote (HTTP) servers, namespaced tools, OAuth w/ DCR for remote.

## Benchmark targets (for the ≥10% efficiency goal)

Candidate baselines: opencode + same Ollama model; aider benchmark harness (polyglot); SWE-bench-lite subset; token-efficiency metric = task success per prompt token. Headless `gw --prompt` is the harness entry point. TBD in a later iteration.

---
name: Bug report
about: Something broke or behaved wrong
labels: bug
---

**What happened**

**What you expected**

**Setup**
- rift version (`rift --version`):
- OS:
- Provider + model (e.g. Ollama `gemma4:26b`, `anthropic/claude-sonnet-4-6`):
- Server (Ollama version / vLLM / LM Studio / cloud):

**Repro steps**
1.

**Turn trace (best single thing you can attach)**
Re-run with `--trace /tmp/rift-trace.jsonl` and paste the relevant lines.
Traces hold token counts, tool calls, and failure counters — no file
contents beyond a capped prompt head.

```jsonl
```

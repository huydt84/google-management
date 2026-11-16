# 0004. One OpenAI-compatible LLM client; LiteLLM in Phase 2; no reliance on tool calling

- Status: Superseded by [0010](0010-llm-client-lanes-residency.md)
- Date: 2026-11-16

## Context

Models are served locally by `mlx-server-runtime`, which exposes `POST /v1/chat/completions` in the
OpenAI format, serves **one model per instance**, and does not support `tools`, `response_format` or
JSON mode. The owner wants token and throughput management (LiteLLM) and a second, smaller model for
triage decisions.

## Decision

- Every model call goes through `gm.llm.LLMClient`, speaking the OpenAI chat-completions format to a
  configurable base URL.
- Prompts request a single JSON object; the client parses tolerantly (strip reasoning tags and code
  fences, take the outermost object, validate, fall back to text).
- Phase 0–1 point the client straight at mlx-server-runtime. Phase 2 inserts a LiteLLM proxy with
  model aliases (`agent-main`, `triage-small`), virtual keys per component, limits matched to the
  runtime's concurrency, and an **opt-in** cloud fallback.
- Two runtime instances (different port and IPC socket) serve the two models.

## Consequences

- Swapping the endpoint (mlx → LiteLLM → anything OpenAI-compatible) is a configuration change.
- Structured output depends on prompt discipline and parsing, not server guarantees; all callers must
  handle unparseable output.
- Running two models costs RAM; sizing is measured in Phase 2.

## Alternatives considered

- **Call the runtime directly forever:** no per-component budgets, metrics or fallback.
- **Use an SDK that requires tool calling / JSON mode:** unsupported by the runtime.
- **Add tool calling to mlx-server-runtime first:** valuable, but belongs in that repository; the
  JSON-over-text protocol remains as a fallback even after it exists.

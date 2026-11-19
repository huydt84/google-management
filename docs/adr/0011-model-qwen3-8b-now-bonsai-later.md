# 0011. Qwen3 default and Bonsai-27B evaluation

- Status: Accepted
- Date: 2026-11-16

## Context

The Mac is an M4 Pro with 24 GB of unified memory, shared with the owner's daily apps.
`prism-ml/Bonsai-27B-mlx-1bit` is a 1-bit Qwen model with hybrid attention. mlx-air 0.3
can serve it on the native backend. Its chat template starts a reasoning block for every
response, and the chat endpoint does not expose a switch to disable that reasoning.

## Decision

- Default to `mlx-community/Qwen3-8B-4bit`. It supports direct answers for short tasks and
  handles Japanese, Vietnamese and English. Keep one resident model within the configured
  memory budget.
- Bonsai can be evaluated in direct-answer mode by preparing the
  [local checkpoint](../../deploy/README.md#prepare-bonsai-for-direct-answers) and selecting its
  absolute path in Settings or with `GM_LLM_MODEL`. The
  [model clients](../../crates/gm/src/llm/model_client.rs) handle each model's reasoning
  and response format. [Sampling](../../llm.yaml) is configured by model ID.
- Before changing the default, run `gm eval` against both models on the same curated
  set. Check triage accuracy, JSON parsing and summary latency against the
  [product requirements](../prd.md#8-non-functional-requirements).
  Use the same `GM_LLM_MODEL` value as the model ID supplied to `mlx-air serve --model`
  for each run.

## Consequences

- Qwen3 remains the comparison baseline.
- The stock Bonsai model ID uses the original thinking template. The direct checkpoint
  reuses the same cached weights and supplies an empty reasoning block in its template.
  The application eval checks the final answers.

## Alternatives considered

- **Bonsai on the Python mlx-lm backend:** the native backend can load Bonsai and keeps
  the runtime's native batching.
- **Larger 4-bit models:** more weight memory on the shared Mac.

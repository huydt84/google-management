# 0011. Qwen3-8B 4-bit now; Bonsai-27B 1-bit when the runtime supports it

- Status: Accepted
- Date: 2026-11-16

## Context

The Mac is an M4 Pro with 24 GB of unified memory, shared with the owner's daily apps. The owner wanted
`prism-ml/Bonsai-27B-mlx-1bit` (Qwen3.5 architecture, hybrid attention with 3 linear-attention layers
per full-attention layer, 64 layers, 1-bit g128 weights, ~5.1 GB pack, ~6 GB peak). mlx-server-runtime
today runs on upstream MLX 0.31.2 (no 1-bit kernels), and its native backend supports Qwen2, Qwen3,
Gemma3 and Lfm2Moe, not the Qwen3.5 hybrid architecture.

## Decision

- **Now:** `mlx-community/Qwen3-8B-4bit` (~5 GB weights), memory budget ≤ 8 GB including KV cache. It
  is natively supported (continuous batching, prefix cache) and handles Japanese, Vietnamese and English.
- **Later:** switch to Bonsai-27B by changing `GM_LLM_MODEL` (and the runtime's `[worker].model`) once
  mlx-server-runtime has (1) the Prism MLX fork or upstream 1-bit kernels and (2) native Qwen3.5
  hybrid-attention support. That work belongs in the mlx-server-runtime repository.
- Before any switch, run `gm eval` on the curated eval set and compare it with the current model.

## Consequences

- gm code must not depend on model specifics beyond the prompt contract (the no-think tag,
  thinking and its budget are configured per model in `llm.yaml`, [ADR-0018](0018-llm-inference-profile.md)).
- Quality of 8B summaries and triage is the baseline the eval set measures.

## Alternatives considered

- **Bonsai now via the Python mlx-lm path:** bypasses mlx-server-runtime's batching and cache, and
  still needs the fork.
- **Larger 4-bit models (14B+):** better quality, but 9–10 GB+ resident memory.

# Architecture Decision Records

All decisions, including small ones, are listed in the [decision log](../decisions.md).

Each record captures one significant decision: the context, what was decided, and the consequences.
Records are immutable once accepted; a later decision that changes one supersedes it with a new
record.

| ADR | Title | Status |
|-----|-------|--------|
| [0001](0001-postgres-durable-bus.md) | Durable event bus on Postgres | Accepted |
| [0002](0002-gmail-ingestion-by-pull.md) | Ingest Gmail by polling or pulling, no public webhooks | Accepted |
| [0003](0003-intent-router-before-agent.md) | Intent router and fixed recipes before a free-form agent | Accepted |
| [0004](0004-openai-compatible-llm-via-litellm.md) | One OpenAI-compatible LLM client; LiteLLM in Phase 2; no reliance on tool calling | Superseded by 0010 |
| [0005](0005-single-process-localhost-deployment.md) | Single process, localhost only | Accepted (language amended by 0009) |
| [0006](0006-telegram-as-mobile-channel.md) | Telegram as the mobile interaction channel | Accepted (behind a channel interface) |
| [0007](0007-sheets-for-notes-and-ledger.md) | Google Sheets via `drive.file` for the ledger instead of Keep | Deferred (backlog) |
| [0008](0008-per-user-settings.md) | Settings stored per user and edited in the app; deployment stays single-user | Accepted |
| [0009](0009-rust-single-binary.md) | Rust single binary replaces the Python implementation | Accepted |
| [0010](0010-llm-client-lanes-residency.md) | Direct LLM client with priority lanes and model hours; no gateway | Accepted |
| [0011](0011-model-qwen3-8b-now-bonsai-later.md) | Qwen3-8B 4-bit now; Bonsai-27B 1-bit when the runtime supports it | Accepted |
| [0012](0012-rules-engine-v2.md) | Rules engine v2: YAML, first match wins, extractors, tested examples | Accepted |
| [0013](0013-ops-launchd-keychain-tailscale.md) | launchd services, Keychain secrets, Tailscale for remote access | Accepted |
| [0014](0014-web-ui-askama-htmx-fluent.md) | Web UI: askama, htmx, Fluent (vi / en / ja) | Accepted |
| [0015](0015-notification-policy-and-digest.md) | Notification policy, night behavior and the 09:05 digest | Accepted |
| [0016](0016-operations-baseline.md) | Operations baseline: CI, tests, observability, backups | Accepted |
| [0017](0017-eval-data-handling.md) | Evaluation data: redacted fixtures in git, raw set local only | Accepted |

Template for new records:

```markdown
# NNNN. Title

- Status: Proposed | Accepted | Superseded by NNNN
- Date: YYYY-MM-DD

## Context
## Decision
## Consequences
## Alternatives considered
```

# 0017. Evaluation data: redacted fixtures in git, raw curated set local only

- Status: Accepted
- Date: 2026-11-16

## Context

Rules and prompts need regression checks on real mail patterns. The owner's mail contains personal
data (name, amounts, order numbers, login IPs). Data retention is 60 days.

## Decision

- **Rule fixtures** (in git, `tests/fixtures/mail/`): headers only (sender address, display name,
  subject pattern, labels, List-Unsubscribe presence) with names, amounts, IPs, order and tracking
  numbers replaced by placeholders. Used by rule tests in CI.
- **LLM eval set** (local only, `GM_EVAL_DIR`, default `~/.local/share/gm/eval/`, gitignored and
  outside the repo): curated full mails added with `gm eval add <gmail-id> --expect ...`, at most
  200, kept until removed (exempt from 60-day retention). `gm eval run` scores triage decisions and
  summary JSON validity/length against the local model only.
- Eval data is never sent to any external service and never committed.

## Consequences

- CI covers rules; prompt/model quality is checked locally before a model or prompt change.

## Alternatives considered

- **Everything local:** CI could not catch rule regressions.
- **Synthetic fixtures only:** misses real format quirks.
- **Redacted LLM eval:** redaction can change model behavior.

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

## Amendment (2026-11-17, Phase 2)

- Cases are added from the dashboard ("Add to eval" with an expected-action picker on each event
  row, highlighted when the owner gave 👎 to a model decision), not with `gm eval add`. Each case is a
  plain JSON file (`<gmail id>.json`, mode 0600, folder 0700) holding the raw Gmail message, the
  expected action and notes. A 201st case is refused until files are removed.
- The command is `gm eval` (no `run`). It refuses a `GM_LLM_BASE_URL` whose host isn't 127.0.0.1,
  localhost or ::1, and reports accuracy per action, the confusion matrix, JSON parse rate, latency
  p50/p95 and tokens.
- The Model tab runs the same evaluation against the ready local model and displays the latest
  completed result. Web and CLI runs store the report alongside the cases with mode 0600. The
  report contains metrics and case IDs. Raw mail stays in the case files.

## Consequences

- CI covers rules; prompt/model quality is checked locally before a model or prompt change.

## Alternatives considered

- **Everything local:** CI could not catch rule regressions.
- **Synthetic fixtures only:** misses real format quirks.
- **Redacted LLM eval:** redaction can change model behavior.

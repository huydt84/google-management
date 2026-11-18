# 0012. Rules engine v2: YAML, first match wins, extractors, tested examples

- Status: Accepted
- Date: 2026-11-16

## Context

The owner's mail (measured): ~20/day; Rakuten ~4/day with transactional and marketing mail from the
**same address** (`info@mail.rakuten-card.co.jp` sends usage notices, statements and 楽天カード通信
campaigns); LinkedIn job alerts ~3/day; newsletters; Japanese services; occasional mail from people.
Phase 0's matcher (labels, sender, subject, List-Unsubscribe; actions ignore/log/notify) cannot tell
these apart or extract card amounts without the LLM.

## Decision

- Rules stay in `rules.yaml` (path `GM_RULES_PATH`), reloaded on change. First matching rule wins.
- **Matchers** (all inside one `when` must match): `from` (address glob), `from_name` (display-name
  substring or regex), `subject` (regex), `labels_any`, `labels_none`, `list_unsubscribe` (bool),
  `body` (regex on the first 4 KB of cleaned text; use sparingly).
- **Extractors:** named regex captures (`extract: {amount: '...', date: '...'}`) on subject or
  cleaned body, used in notification and digest templates without the LLM.
- **Actions:** `ignore`, `digest` (with `section` and `summarize: true|false`), `notify` (with
  `priority: silent|normal|high|urgent` and `summarize: true|false`), `triage` (hand off to model
  triage). The default for unmatched mail is `triage`.
- Rule-only notifications (`summarize: false`, no LLM needed) are delivered at any hour with their
  priority (`silent` without sound); notifications that need a summary wait for the model
  (amended 2026-11-16: a ping without its summary only repeats the Gmail app notification).
- Each rule may list `examples` (redacted header fixtures). A test loads `rules.yaml` and checks every
  example lands on its rule, so CI catches regressions.
- The Mail rules page displays the configured match conditions and lets each user override a named
  rule's action. The mailbox owner's override applies after matching, before the verdict is saved.
  An inherited or same-action route uses the rule's settings. Switching to Notify uses normal
  priority with a summary. Switching to Digest puts the mail in Other without a summary. Ignore
  sends nothing. The rules file still defines the fallback for unmatched mail.

Code: [`rules.rs`](../../crates/gm/src/rules.rs), [`web/rules.rs`](../../crates/gm/src/web/rules.rs).

## Consequences

- Most mail is decided for free; the model only sees unmatched mail and mail that a rule wants
  summarized.
- Matchers and default actions are reviewable in git. Personal routing choices live in
  [per-user settings](0008-per-user-settings.md).

## Alternatives considered

- **DB + web editor now:** friendlier, multi-user ready; a large UI before the port is done.
- **Model-only triage:** adapts on its own, but spends the daytime model budget on noise.

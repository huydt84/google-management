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
- **Actions:** `ignore`, `digest` (with `digest_section` and `summarize: true|false`), `notify` (with
  `priority: silent|normal|high|urgent` and `summarize: true|false`), `triage` (hand off to model
  triage). The default for unmatched mail is `triage`.
- Rule-only notifications (`summarize: false`, no LLM needed) are delivered at any hour with their
  priority (`silent` without sound); notifications that need a summary wait for the model
  (amended 2026-11-16, decision P4).
- Each rule may list `examples` (redacted header fixtures). A test loads `rules.yaml` and checks every
  example lands on its rule, so CI catches regressions.

## Consequences

- Most mail is decided for free; the model only sees unmatched mail and mail that a rule wants
  summarized.
- Rules are reviewable in git. A web editor can come later and read and write the same file.

## Alternatives considered

- **DB + web editor now:** friendlier, multi-user ready; a large UI before the port is done.
- **Model-only triage:** adapts on its own, but spends the daytime model budget on noise.

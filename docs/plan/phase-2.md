# Phase 2: Morning digest and model triage

**Goal:** everything that doesn't ping the owner arrives in one digest at 09:05; mail no rule covers
is judged by the model; the owner can correct it; the model only occupies memory during model hours;
model and prompt changes are measured; the owner hears about problems without watching a dashboard.

Prerequisite: Phase 1. 2A, 2B, 2D, 2E run in parallel. 2C needs Telegram callbacks (the minimal part
of 3A: client + poller for `callback_query`) and 2B. 2F needs 2B.

Decisions: [decision log](../decisions.md) P3, P4, T1–T16, L3–L5, L10, L11, O3;
[ADR-0010](../adr/0010-llm-client-lanes-residency.md), [ADR-0015](../adr/0015-notification-policy-and-digest.md),
[ADR-0017](../adr/0017-eval-data-handling.md).

Before starting, confirm with the owner anything this plan leaves open (marked **ask**).

## 2A Digest builder and scheduler

**Owns:** `src/digest.rs` (new), `src/scheduler.rs` (new), `prompts/digest.md`, `prompts/jobs.md`,
migration `0002_digest.sql`, digest strings in `locales/*/main.ftl`, `templates/digest.html`.

1. **Scheduler** (`scheduler.rs`): a background loop that publishes `schedule.due {kind, at}` at fixed
   local times (`digest` at 09:05; residency `load` 09:00 and `unload` 19:00 for 2D). Missed times
   (sleep) fire once on wake. Times come from config (`GM_DIGEST_AT`, default `09:05`).
2. **Per-item summaries** (`digest.item.summarize`, batch lane): `triage` publishes it for `digest`
   events whose verdict has `summarize: true`. During model hours it runs on arrival; at night it
   defers to 09:00 (L10). Output: `{line}` (1–2 sentences, summary language); for the `jobs` section
   `{jobs: [{title, company, location, link}]}`. Stored in `events.summary`.
3. **Digest build** on `schedule.due{digest}`: collect `digest` events since the last digest, group by
   `verdict.section` in rule order, render with rule-extracted values where present (card usage,
   coupon expiry, points) and item summaries otherwise. Job alerts are deduplicated by (title, company).
   Items without a summary yet are listed by subject and marked pending.
4. **Send and edit**: one Telegram message (split at 4096), stored in `digests` (date, message ids,
   event ids). When pending items finish, edit the message (`editMessageText`). Empty digest: **ask**
   (send "nothing new" vs send nothing; recommend nothing).
5. **Web**: `/digest` shows today's and past digests.

Tests: grouping and ordering; dedupe of job alerts; night deferral then 09:00 run; edit after late
summaries; nothing sent twice on retry (idempotent by date).

## 2B Model triage

**Owns:** `src/triage_model.rs` (new), `prompts/triage.md`, the `Action::Triage` branch of
`pipeline.rs`.

1. `triage` publishes `gmail.message.classify` for action `triage` instead of today's interim notify.
2. The `classify` handler (notify lane, `/no_think`, ≤ 80 output tokens) asks the model for
   `{action: notify|digest|ignore, reason}` given headers and the first ~1,500 tokens of the cleaned
   body. Invalid output after the retry → notify (fail open, so nothing important is lost).
3. Night or model unavailable: defer (09:00, or retry every 5 min), as for summaries.
4. The decision and reason go into `events.data.verdict` (`rule = "model"`), visible on the dashboard.
5. Then it continues as if a rule had said that action (notify → summarize; digest → 2A).

Tests: each action; invalid JSON → notify; deferral; prompt keeps the shared prefix.

## 2C Feedback

**Owns:** `src/feedback.rs` (new), migration `0003_feedback.sql`, the feedback part of the Telegram
poller.

1. Notifications and model-triaged digest items carry 👍 / 👎 (`f:<event_id>:up|down`).
2. Feedback is stored in `triage_feedback` and shown on the dashboard.
3. 👎 on a rule-matched mail suggests editing that rule (link to the rule name); 👎 on a
   model-triaged mail is an eval case for 2F. **Ask** whether 👎 should also offer "mute this sender"
   (which would write rules from the app; rules are currently a file under version control).

## 2D Residency controller

**Owns:** `src/residency.rs` (new), `deploy/launchd/com.gm.mlx.plist` (switch from KeepAlive to
on-demand).

1. On `schedule.due{load}`: `launchctl kickstart gui/<uid>/com.gm.mlx`, then poll `GET /ready` on the
   runtime until ready (timeout → health alert). On `unload`: wait for in-flight requests (the
   admission counter), then `launchctl kill TERM gui/<uid>/com.gm.mlx`.
2. On startup inside model hours, ensure the model is up; outside, ensure it is down.
3. `ModelHours` remains the single source of truth; the pipeline already defers outside them.
4. Expose `gm_model_resident` in `/metrics` and the state on the dashboard.

Tests: fake `launchctl` command runner and wiremock `/ready`; unload waits for in-flight requests.

## 2E Health, dead letters, sender counts

**Owns:** `src/health.rs` (new), `templates/jobs.html`, `templates/senders.html`, dashboard health
panel.

1. Health checks every minute: dead jobs > 0, ingest lag > 10 min, model down during model hours,
   account `needs_reconnect`. A Telegram alert after 15 minutes of the same failure, one message per
   incident, and a recovery message.
2. Dashboard health panel with the same checks.
3. `/jobs?status=dead` with Retry (back to `pending`, attempts reset).
4. `/senders`: counts of ignored mail by sender over 30 days (T15), so a sender can be promoted to the
   newsletter list by editing `rules.yaml`.

## 2F `gm eval`

**Owns:** `src/eval.rs` (new), `gm eval` in `main.rs`.

1. Eval set: up to 200 mails as JSON files in `~/.local/share/gm/eval/` (gitignored location outside
   the repo), each with the expected action and, optionally, notes. Added by a dashboard button "add
   to eval set" on an event (fetches the mail once, stores it locally). Exempt from retention.
2. `gm eval` runs rules + model triage + summary on each mail against the **local** model only and
   prints accuracy per action, confusion matrix, JSON parse rate, latency p50/p95 and tokens.
3. Run it before changing the model (Bonsai), prompts or `rules.yaml` defaults.

## Phase 2 is done when

The digest arrives at 09:05 with correct sections; unmatched mail is classified with reasons;
feedback is recorded; mlx runs only during model hours; health alerts fire in a test; `gm eval`
produces a report.

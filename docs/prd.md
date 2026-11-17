# Product Requirements Document: Google Management

| | |
|---|---|
| Status | Phases 1–4 implemented in Rust |
| Owner | The machine owner (single user) |
| Last updated | 2026-11-18 |
| Related | [Architecture](architecture.md) · [Specs](spec/) · [ADRs](adr/) · [Plan](plan/README.md) |

## 1. Summary

Google Management is a personal assistant that runs on the owner's Mac. It watches the owner's Gmail
and Calendar, decides which mail matters, pings the phone about the few that do, collects the rest
into a morning digest, and lets the owner act on mail ("translate", "reply that I agree") with
explicit approval for anything that leaves the account.

All model work runs locally on `mlx-server-runtime` (Qwen3-8B 4-bit), so mail never leaves the Mac.

## 2. Problem

Measured on the owner's mailbox (metadata only, 2026-11): about **20 mails a day**, mostly Japanese,
then English and Vietnamese.

- Around 4 a day come from Rakuten. The same sender address sends card-usage alerts, statements,
  security notices and promotions; shop addresses mix shipping notices with newsletters.
- Around 3 a day are LinkedIn job alerts; several more are newsletters and promotions.
- The few mails that matter (a card charge, a statement, a login alert, a message from a person) are
  buried. Gmail's categories don't know which Rakuten mail is which.
- Reading Japanese mail on the phone takes effort; acting on a mail takes several manual steps.
- Cloud assistants would need the whole mailbox and can be steered by malicious mail.

## 3. Vision and principles

> "My phone tells me only what matters; everything else waits for one morning digest. On my machine,
> under my control."

1. **Private.** Data and inference stay on the Mac; no cloud model.
2. **The owner stays in control.** Every external action is approved; every action is audited.
3. **Quiet.** A ping costs attention. Rules decide most mail for free; the model handles the rest.
4. **Never lose an event.** Sleep, restarts and failures are caught up; duplicates are prevented.
5. **Testable policy.** Triage rules are a versioned file with examples checked in CI.

## 4. Users and context

One user today: the owner. Settings and account ownership are keyed per user so more users can be
added later ([ADR-0008](adr/0008-per-user-settings.md)).

- Personal gmail.com account; a MacBook Pro M4 Pro (24 GB) that is on most of the day.
- Reads Vietnamese, English and Japanese; UI available in all three.
- Telegram on the phone; the web UI is reachable on the tailnet.
- Technical: comfortable editing YAML, running Docker and reading logs.

Out of scope: multiple users, shared mailboxes, hosting for others.

## 5. Goals and non-goals

| ID | Goal |
|----|------|
| G1 | Ping the owner about important mail within minutes, with a short summary in their language. |
| G2 | Everything else lands in one digest at 09:05, grouped and summarized. |
| G3 | Keep noise low and tunable: rules first, model triage for the rest, feedback improves both. |
| G4 | Let the owner act on mail from the phone (translate, analyze, reply), with approval for anything external. |
| G5 | Data and inference local; no recurring cost. |
| G6 | Run unattended on the Mac and recover from sleep, restarts and failures. |

Non-goals: a full mail client; multi-user or public hosting; autonomous external actions; financial
transactions; a mobile app (Telegram is the mobile interface).

## 6. User stories

Status: ✅ done · 🔜 planned.

### Phase 0 (Python, superseded by Phase 1)

Sign-in limited to the owner, one-time Gmail connection, Telegram notification with summary, rule
triage with hot reload, LLM-failure fallback, catch-up after sleep, summary language setting. All
carried over to Phase 1.

### Phase 1: Rust port, rules v2, operations

| ID | Story | Acceptance | Status |
|----|-------|------------|--------|
| US-1.1 | Only I can sign in. | id_token checked (audience, issuer, expiry, verified email = `GM_ALLOWED_EMAIL`), PKCE, signed cookies. | ✅ |
| US-1.2 | I connect Gmail once and it keeps working. | Encrypted refresh token; Pub/Sub pull (gm provisions it) with a 15-min safety poll; `needs_reconnect` shown when Google revokes it. | ✅ |
| US-1.3 | A Rakuten Card charge pings me silently with the amount. | Rule `rakuten-card-flash` extracts amount and date; silent Telegram message, day or night. | ✅ |
| US-1.4 | Security mail (Rakuten login, 【重要】, Google/Apple account) is flagged 🚨. | Urgent rules; the rule-only login notice pings at any hour; summarized ones ping once the summary exists (09:00 at night). | ✅ |
| US-1.5 | Promotions, shop newsletters, review requests and LinkedIn noise don't ping me. | Ignore rules in `rules.yaml`. | ✅ |
| US-1.6 | Card statements, LinkedIn messages and unmatched mail ping me with a summary. | Notify rules; summaries during model hours; normal/high wait for 09:00 at night. | ✅ |
| US-1.7 | Shipped / delayed orders ping me quietly. | Silent notify with summary. | ✅ |
| US-1.8 | I edit rules without restarting and can't break them by accident. | Hot reload; invalid file keeps the old rules; `gm rules` + CI check examples. | ✅ |
| US-1.9 | The UI speaks my language; summaries are in my chosen language. | en / vi / ja UI; per-user summary language. | ✅ |
| US-1.10 | No plaintext secrets on disk. | Keychain; `.env` non-secret only. | ✅ |
| US-1.11 | It starts on login, backs itself up and I can see its health. | launchd plists, nightly `pg_dump` (14 kept), JSON logs, `/metrics`. | ✅ (installation is a manual step) |
| US-1.12 | Old data goes away. | 60-day retention. | ✅ |

### Phase 2: Digest and model triage

| ID | Story | Acceptance |
|----|-------|------------|
| US-2.1 | At 09:05 I get one digest of everything that didn't ping me. | Sections: card usage, orders, coupons, points, jobs, newsletters, account, other; late items edited in. |
| US-2.2 | LinkedIn job alerts appear once, deduplicated. | Title / company / location / link list. |
| US-2.3 | Mail no rule covers is judged by the model. | notify / digest / ignore with a reason, visible on the dashboard. |
| US-2.4 | I tell it when it got it wrong. | 👍/👎 buttons; feedback stored; shown in `gm eval`. |
| US-2.5 | Model work happens during the day. | gm runs model work 09:00–19:00 only; the model process itself is managed by mlx-air. |
| US-2.6 | I can measure model quality before changing models or prompts. | `gm eval` on the local eval set (≤ 200 mails). |
| US-2.7 | I'm alerted if the system is unhealthy. | Telegram alert after 15 minutes of dead jobs, ingest lag, model down or token invalid; dashboard health panel. |
| US-2.8 | I can retry failed jobs. | Dead-letter page. |
| US-2.9 | I can see which ignored senders are frequent. | Ignored-sender counts; promote one to the newsletter list. |

### Phase 3: Act, with approval

| ID | Story | Acceptance |
|----|-------|------------|
| US-3.1 | I tap **Translate** and get the full mail in my translation language. | Reply in the same chat, at any hour. |
| US-3.2 | I tap **Analyze** and get key points, deadlines, amounts and phishing warnings. | Structured reply. |
| US-3.3 | I reply "tell them Tuesday works" and get a draft to approve. | Draft in the right thread to the original sender; sent only after Approve; re-checked before sending; I can ask for changes by replying to the preview. |
| US-3.4 | `/pause`, `/resume`, `/status`, `/language` work from my chat only. | Other chats ignored. `/pause` holds notifications until `/resume`; the digest and alerts still come. |
| US-3.5 | Approvals I ignore expire. | 24 h. |
| US-3.6 | A message that isn't about a mail gets help, not a guess. | Usage text, no model call. |
| US-3.7 | I can approve from the dashboard too and see what was sent. | `/tasks`: pending approvals, recent tasks, audit log. |

### Phase 4: Calendar

| ID | Story | Acceptance |
|----|-------|------------|
| US-4.1 | I'm told about new invitations and changes to upcoming events. | Calendar source with sync tokens; `calendar.readonly`. |
| US-4.2 | Today's calendar is part of the digest. | Digest section. |

### Backlog

Expense ledger (Sheets), Drive notifications, free-form agent and automations with undo,
Bonsai-27B 1-bit model ([ADR-0011](adr/0011-model-qwen3-8b-now-bonsai-later.md)).

## 7. Functional requirements

### 7.1 Access and identity

- **FR-1** The web UI binds `127.0.0.1` only; remote access through Tailscale.
- **FR-2** Sign-in uses Google OIDC with PKCE; only the verified `GM_ALLOWED_EMAIL` gets a session.
- **FR-3** Connecting Gmail is a separate consent with offline access; the account must equal the owner.
- **FR-4** Minimal scopes per phase ([security spec](spec/security.md#scopes)).
- **FR-5** A rejected refresh token marks the account `needs_reconnect`.

### 7.2 Ingestion

- **FR-6** Gmail changes are pulled from a Pub/Sub subscription gm provisions itself, with a safety
  poll every 15 min (every 60 s while Pub/Sub is down, or in `poll` mode), via the History API; only mail after connecting is processed;
  only configured labels (default `INBOX`).
- **FR-7** Each Gmail message becomes at most one event.
- **FR-8** An expired cursor recovers the last 2 days of INBOX mail.

### 7.3 Triage

- **FR-9** `rules.yaml` is evaluated in order; the first match wins; actions are ignore, digest,
  notify (silent / normal / high / urgent) and triage.
- **FR-10** Rules can extract values with regexes and render a notification `message` from them.
- **FR-11** Every rule carries redacted examples; `gm rules` and CI fail if an example lands elsewhere.
- **FR-12** The file hot-reloads; an invalid file keeps the previous rules.
- **FR-13** (Phase 2) Unmatched mail goes to model triage; until then it is notified with a summary.

### 7.4 Notifications

- **FR-14** Mail notifications are Telegram text messages ≤ 4096 characters with sender, subject or
  rule message, summary (when enabled), suggested actions and a Gmail link, in the owner's UI language.
  Calendar notices use emphasized headings and values, compact local times and actual changes.
- **FR-15** Silent priority sends without sound. Rule-only notifications (no LLM needed) are sent at
  any hour; notifications that need a summary are sent once it exists, in model hours.
- **FR-16** No notification is sent without its summary: while the model is unavailable the work is
  queued (retried every 5 min); real LLM errors leave the job `dead` for inspection.
- **FR-17** Without a bot token, notifications go to the log.
- **FR-18** (Phase 2) Digest at 09:05 (sections by rule); feedback buttons.

### 7.5 Model use

- **FR-19** All model calls go through one client with priority lanes and a concurrency cap.
- **FR-20** Mail is cleaned and capped at ~2,000 tokens; prompts keep a stable prefix with the mail
  last.
- **FR-21** Output is JSON, parsed tolerantly, retried once.
- **FR-22** Model hours 09:00–19:00; work outside them, or while the model is unavailable, is
  deferred, not failed.
- **FR-22a** Outside model hours the model is stopped to free memory. A task I start from Telegram
  or the web starts it (I am told once) and it stops again after 10 idle minutes; mail never starts
  it. Hours, idle minutes, stop grace and the model id are editable on the Settings page, with a switch
  that turns the management off.

### 7.6 Settings

- **FR-23** Per-user settings in the database, edited on the Settings page: summary language,
  translation language, UI language (en / vi / ja). Env vars provide defaults.
- **FR-24** Language values are validated before reaching a prompt.

### 7.7 Administration

- **FR-25** Dashboard: connection, cursor, last sync and error, model hours, queue counts, events.
- **FR-26** Manual sync; `/healthz`; `/metrics` behind a token.
- **FR-27** Secrets in the Keychain; launchd agents; nightly backups (14 kept); JSON logs (7 days).
- **FR-28** Data older than 60 days is deleted (Phase 3: also tasks, approvals and the Telegram
  message map); the audit log of executed actions is kept.

### 7.8 Actions (Phase 3)

- **FR-29** The owner instructs via buttons or replies; other chats are ignored.
- **FR-30** Intents: translate, analyze, reply. Targets come from code, never model output.
- **FR-31** External actions need an approval with an immutable, hashed payload that expires; every
  executed action is audited.
- **FR-32** Owner-initiated tasks run whenever the model answers, also outside model hours; background
  model work keeps to model hours.

## 8. Non-functional requirements

| Area | Requirement |
|------|-------------|
| Privacy | Mail is processed only on the Mac; no cloud model. 60-day retention. Raw eval mail never leaves the Mac. |
| Security | No unapproved external action (hard requirement). Secrets in the Keychain, never logged. See [security spec](spec/security.md). |
| Reliability | At-least-once processing, idempotent handlers, retries 5 s doubling to 15 min, 5 attempts, then `dead`. No lost events across crashes, restarts or sleep. |
| Latency | Mail → event within seconds (Pub/Sub), at most 60 s while Pub/Sub is down. Summary p50 < 30 s on Qwen3-8B 4-bit during model hours. Rule-only pings: as fast as ingestion (seconds with Pub/Sub), day or night. |
| Resources | One resident model ≤ 8 GB (weights + KV cache) during 09:00–19:00, nothing at night; fits beside daily apps on 24 GB. At most 2 concurrent model requests. |
| Cost | Zero recurring cost. |
| Maintainability | Rust stable, one binary, clippy clean, tests against real Postgres with fake HTTP (no real Google/Telegram/LLM). |
| Observability | JSON logs, `/metrics`, dashboard; health alerts in Phase 2. |

## 9. Success metrics

| Metric | Target | Source |
|--------|--------|--------|
| Unapproved external actions | **0** | audit log vs approvals (Phase 3) |
| Pings per day | ≤ 6 on a typical day | `events` with status `notified` |
| Notification precision | ≥ 90% not marked 👎 after 4 weeks | feedback (Phase 2) |
| Missed important mail | ≤ 1 per week | feedback (Phase 2) |
| Mail → ping latency (urgent, rule-only) | < 2 min | event timestamps |
| Lost events | 0 | cursor vs events |
| Rule examples passing | 100% | CI |

## 10. Release plan

| Phase | Theme | Exit criteria |
|-------|-------|---------------|
| 0 ✅ | See what matters (Python) | Shipped in commit `145de43`; replaced by Phase 1. |
| 1 | Rust port, rules v2, operations | Parity with Phase 0; rules from the mail analysis with examples; Keychain, launchd files, backups, CI, metrics; Python removed. |
| 2 | Digest and model triage | Digest at 09:05; model triage with feedback; `gm eval`; health alerts; dead-letter page. |
| 3 | Act, with approval | Translate, analyze, reply from Telegram; approvals and audit; prompt-injection end-to-end test. |
| 4 | Calendar | Invitations and changes; today's calendar in the digest. |

Details: [plan](plan/README.md). Why this order: most daily mail is digest-bound, so the digest delivers value before reply and
actions ([ADR-0015](adr/0015-notification-policy-and-digest.md)).

## 11. Risks and mitigations

| Risk | Impact | Mitigation |
|------|--------|-----------|
| Prompt injection via mail | Data exfiltration, unwanted actions | No tools for models that read mail; targets never from model output; approvals with previews. |
| Rakuten changes its mail format | Card amounts or security mail misclassified | Examples in CI; unmatched mail falls through to triage (notified), not ignored; `gm rules` after edits. |
| OAuth app left in "Testing" | Refresh token dies after 7 days | Setup requires "In production"; `needs_reconnect` on the dashboard. |
| Mac asleep during model hours | Late summaries and digest | Deferred jobs run on wake; urgent rules don't need the model. |
| 8B model gives weak summaries for Japanese | Less useful pings | Translate button (Phase 3); eval set; Bonsai-27B later. |
| RAM pressure | Slow machine | One model, model hours only, max 2 in flight. |
| Long downtime beyond Gmail history | Some mail never processed | 2-day recovery; gap logged. |
| Bot token leak | Bot impersonation | Keychain; never logged; rotate via BotFather. |
| Database loss | History lost | Nightly dumps; Gmail stays the source of truth. |

## 12. Open questions

1. Is Telegram the long-term channel? (A `Notifier` interface keeps a switch cheap.)
2. When Bonsai-27B is supported, is its quality gain worth its memory? (Decide with `gm eval`.)

## 13. Glossary

| Term | Meaning |
|------|---------|
| Event | One Google object (a mail), stored in `events`. |
| Rule / verdict | An entry in `rules.yaml`; the result of evaluating rules for one mail (action, priority, section, extracted values). |
| Digest | The 09:05 message with everything that didn't ping. |
| Model triage | The model choosing notify / digest / ignore for unmatched mail (Phase 2). |
| Lane | Priority class of a model request: interactive, notify, batch. |
| Model hours | 09:00–19:00, when the model is resident. |
| Bus / topic / job | Durable pub/sub on Postgres; a named message type; one delivery to one subscriber. |
| Approval | An immutable, hashed action proposal the owner approves or rejects (Phase 3). |

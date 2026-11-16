# Decision log

The owner made every decision below in a structured interview on 2026-11-16, from the options and
trade-offs presented to them. Each row shows the decision, the choice and the reason. Significant
architecture choices also have an [ADR](adr/README.md); the ADR column links it.

When a decision changes, edit its row, add a dated note under [Changes](#changes), and supersede the
ADR if one exists. Never change a decision silently. Ask the owner first (see
`~/.claude/skills/decision-interview/SKILL.md`).

## Product and scope

| # | Decision | Choice | Reason | ADR |
|---|----------|--------|--------|-----|
| P1 | Users | Single user now; data model and settings keyed per user so more users can be added later | Only the owner today, but "who knows" | [0008](adr/0008-per-user-settings.md) |
| P2 | Mail volume (measured) | ~20 mails/day, of which ~4 are Rakuten and ~3 LinkedIn job alerts; mostly Japanese, then English and Vietnamese | Measured from the owner's mailbox (metadata) | — |
| P3 | Notification style | Important mail notified immediately; everything else goes to a morning digest at **09:05** | Fewer interruptions; the digest fits right after the model loads | [0015](adr/0015-notification-policy-and-digest.md) |
| P4 | Night policy | 19:00–09:00: **only rule-only notifications** (no LLM needed, e.g. the card amount, the Rakuten login notice) go out; **everything that needs the LLM waits for 09:00, urgent included**. No ping is ever sent without its summary | A summary-less ping only duplicates the Gmail app notification | [0015](adr/0015-notification-policy-and-digest.md) |
| P5 | Mobile channel | Telegram, behind a `Notifier` channel interface | Google Chat apps need a Workspace account; the interface allows a switch later | [0006](adr/0006-telegram-as-mobile-channel.md) |
| P6 | Approvals | Every external action (send, calendar change, share) needs explicit approval | Safety invariant | [0003](adr/0003-intent-router-before-agent.md) |
| P7 | Actions model | Intent router + fixed recipes (translate, analyze, reply), no free-form agent yet | Predictable, safe with an 8B model | [0003](adr/0003-intent-router-before-agent.md) |
| P8 | Japanese and foreign mail | Summary in the user's summary language plus a **[Translate]** button for a full translation on demand | Short default output; translation cost only when wanted | — |
| P9 | Retention | Event data and summaries kept **60 days**; the curated local eval set is exempt (≤200 mails) | Privacy versus usefulness | [0017](adr/0017-eval-data-handling.md) |
| P10 | UI languages | Localized web UI and bot strings: **vi / en / ja** | Owner reads all three | [0014](adr/0014-web-ui-askama-htmx-fluent.md) |
| P11 | Settings | Per user, edited in the app; env only provides defaults | More preferences are planned | [0008](adr/0008-per-user-settings.md) |
| P12 | Roadmap | **Rust port (+ rules v2) → digest → reply/actions → calendar** | Most daily mail is digest-bound, so the digest delivers value first | — |
| P13 | Expense ledger | Moved to the **backlog**; no Sheets scope requested | Not selected as a use case | [0007](adr/0007-sheets-for-notes-and-ledger.md) (deferred) |
| P14 | Remote access | gm's own web UI over **Tailscale**; no Vercel or public hosting | Keeps data and ports on the Mac | [0013](adr/0013-ops-launchd-keychain-tailscale.md) |

## Mail triage policy

| # | Mail | Handling | LLM? |
|---|------|----------|------|
| T1 | Rakuten Card 【速報版】 usage flash (`info@mail.rakuten-card.co.jp`) | **Silent ping** with the amount and date, extracted by rule; sent day and night | No |
| T2 | Rakuten Card 確定版 usage (merchant list) | Digest: merchant list | No (rule extraction) |
| T3 | Rakuten Card statement (お支払予定金額のご案内) | Notify **high** with a summary (total, debit date, change deadline) | Yes, daytime |
| T4 | Security mail: Rakuten ID login notice, 【重要】 notices, Google and Apple account security | **Urgent** rules. The Rakuten login notice is rule-only (pings at any hour); 【重要】 and Google/Apple security mail are summarized, so at night or during an LLM outage they wait for the model | Summary only by day |
| T5 | Card terms changes and similar account notices | Digest | Yes (batch) |
| T6 | Rakuten Ichiba shipped (発送 / 出荷) and arrival/delay (入荷予定) from `*@shop.rakuten.co.jp` | **Silent ping** with shop, item and tracking number | Yes, daytime |
| T7 | Rakuten Ichiba order confirmations | Digest | Yes (batch) |
| T8 | Rakuten review requests (`hyouka@rakuten.co.jp`) | Ignore | No |
| T9 | Rakuten marketing: 楽天カード通信 (same address as T1–T3, told apart by display name and unsubscribe link), shop newsletters, Mobile/Magazine/Beauty/Affiliate/楽天24 news | Ignore | No |
| T10 | RaCoupon coupon expiry, weekly/monthly points reports | One digest line each | No (rule extraction) |
| T11 | Newsletters on the named list (Coursera, Blinkist, Japan Dev, Interviewing Japan, GeeksforGeeks, …) | Digest summary (1–2 lines) | Yes (batch) |
| T12 | LinkedIn job alerts | Digest: deduplicated list of title / company / location / link | Yes (batch) |
| T13 | LinkedIn messages from people | Notify normally | Yes |
| T14 | LinkedIn invitations, post notifications, recommendations | Ignore | No |
| T15 | Other Promotions-tab mail (DNSE, SMBC, HotPepper, EX travel, Apple Pay, …) | Ignore (counted on the dashboard so a sender can be promoted to the newsletter list) | No |
| T16 | Everything else unmatched (people, Mercari, Tokyo Gas, SmartEX, gym, dev tools) | **Model triage** chooses notify / digest / ignore with a reason; 👍/👎 feedback | Yes, daytime |

Rules live in `rules.yaml` (see R1). These rows are its initial content.

## Technical

| # | Decision | Choice | Reason | ADR |
|---|----------|--------|--------|-----|
| A1 | Language | **Rust** (stable toolchain, edition 2024) | Owner's choice | [0009](adr/0009-rust-single-binary.md) |
| A2 | Process model | One binary (`gm`) with tokio tasks for web, ingestion, workers and scheduler | Simple to run and update | [0009](adr/0009-rust-single-binary.md) |
| A3 | Libraries | axum, sqlx (Postgres, embedded migrations, **runtime-checked queries** via `sqlx::query`/`query_as`, verified by the test suite against real Postgres), reqwest with thin hand-written Google and Telegram clients, tracing | Few dependencies, full control over scopes and retries; runtime queries keep CI free of `.sqlx/` offline data | [0009](adr/0009-rust-single-binary.md) |
| A4 | Web UI | Server-rendered **askama** templates + **htmx**; Fluent for i18n | Type-checked templates compiled into the binary | [0014](adr/0014-web-ui-askama-htmx-fluent.md) |
| A5 | Queue | Keep the Postgres outbox + `jobs` table (`FOR UPDATE SKIP LOCKED`), at-least-once delivery, idempotent handlers | Durable, no extra service | [0001](adr/0001-postgres-durable-bus.md) |
| A6 | Database | Postgres 17 in Docker (`127.0.0.1:5433`), sqlx migrations | Already in place | — |
| A7 | Gmail ingestion | **Pub/Sub pull** (`GM_GMAIL_INGEST_MODE=pubsub`) with a **safety poll every 15 min**; polling every 60 s while Pub/Sub is down, and as the `poll` mode. Credential: the `pubsub` scope added to the Gmail consent (same encrypted refresh token). gm **creates the topic, subscription and Gmail publisher permission itself** and renews the watch | Mail in seconds without a public webhook; nothing lost if a notification is dropped; no gcloud and no plaintext credentials | [0002](adr/0002-gmail-ingestion-by-pull.md) |
| A8 | Repo | Same repo; Cargo workspace at the root; port Phase 0 to parity, then delete the Python code | One history | [0009](adr/0009-rust-single-binary.md) |
| A9 | Hosting / CI | Private GitHub repo + Actions (fmt, clippy, tests against a Postgres service, `gm rules` check) | Backup + gate for parallel sessions | [0016](adr/0016-operations-baseline.md) |
| A10 | Tests | `#[sqlx::test]` (fresh DB per test) + `wiremock` for Google, Telegram and LLM HTTP; a few end-to-end pipeline tests; never real endpoints | Tests real SQL and HTTP code | [0016](adr/0016-operations-baseline.md) |
| R1 | Rules engine | `rules.yaml`, first match wins, hot reload; matchers: sender, display name, subject regex, labels, List-Unsubscribe; regex extractors; actions ignore / digest / notify(silent, normal, high, urgent) / triage; each rule carries example headers tested in CI | Versioned, reviewable, tested | [0012](adr/0012-rules-engine-v2.md) |

## Operations

| # | Decision | Choice | Reason | ADR |
|---|----------|--------|--------|-----|
| O1 | Service manager | launchd agents for `gm`, mlx-server-runtime and the backup; Postgres via Docker. **No install command**: the agents are registered once (by Claude, after the owner stores the secrets) and then start on login | Native on macOS; a one-time step needs no tooling | [0013](adr/0013-ops-launchd-keychain-tailscale.md) |
| O2 | Secrets | macOS **Keychain** (master key, bot token, OAuth client secret); `.env` only for non-secret config | No plaintext secrets on disk | [0013](adr/0013-ops-launchd-keychain-tailscale.md) |
| O3 | Observability | `tracing` JSON logs to `~/Library/Logs/gm` (rotated), Prometheus `/metrics` behind a token, dashboard health panel, Telegram alert after 15 min unhealthy | Trust an unattended system without a monitoring stack | [0016](adr/0016-operations-baseline.md) |
| O4 | Backups | Nightly `pg_dump` via launchd to `~/Backups/google-management`, keep 14, documented restore; the token key stays in Keychain, not in the dump | Simple and enough | [0016](adr/0016-operations-baseline.md) |
| O5 | Bind address | `127.0.0.1` only; Tailscale for remote | Invariant | [0005](adr/0005-single-process-localhost-deployment.md) |
| O6 | OAuth consent pages | The homepage and privacy policy the consent screen needs before **Publish app** are two static pages in `deploy/oauth-pages/`, hosted on a separate Vercel project (`google-management-bay.vercel.app`, also the authorized domain). Not on the owner's other domains, not inside another Cloud project | Publishing avoids the 7-day refresh-token expiry of "Testing"; keeps gm apart from the owner's other projects | |

## LLM and inference

| # | Decision | Choice | Reason | ADR |
|---|----------|--------|--------|-----|
| L1 | Serving | mlx-server-runtime, called directly; **no LiteLLM / gateway**; gm's `LlmClient` tracks tokens and latency per task and lane | One model, one user; a gateway adds a hop for nothing | [0010](adr/0010-llm-client-lanes-residency.md) |
| L2 | Model | **Qwen3-8B 4-bit** (~5 GB weights, ≤8 GB with KV cache) now; **Bonsai-27B 1-bit later** as a config change once mlx-server-runtime supports it | Fits 24 GB beside daily apps; Bonsai needs runtime work | [0011](adr/0011-model-qwen3-8b-now-bonsai-later.md) |
| L3 | Residency | Model resident **09:00–19:00** only; unloaded at night | Free RAM and power at night | [0010](adr/0010-llm-client-lanes-residency.md) |
| L4 | Priority lanes | `interactive` (user requests, replies) > `notify` (triage + summary of incoming mail) > `batch` (digest items) | User-facing work never waits behind batch work | [0010](adr/0010-llm-client-lanes-residency.md) |
| L5 | Triage tiers | Rules first; only unmatched mail reaches model triage (same main model, short output) | Most mail is decided for free | [0012](adr/0012-rules-engine-v2.md) |
| L6 | Structured output | JSON in the prompt, tolerant parse + **1 retry**; constrained decoding later as an mlx-server-runtime feature | Runtime has no JSON mode | [0010](adr/0010-llm-client-lanes-residency.md) |
| L7 | Thinking | Off (`/no_think`) except reply drafting and analysis | Latency and tokens | [0010](adr/0010-llm-client-lanes-residency.md) |
| L8 | Input size | Clean mail (strip HTML, quotes, signatures, footers), then cap at ~2,000 tokens | Predictable prefill time | [0010](adr/0010-llm-client-lanes-residency.md) |
| L9 | Prompt shape | Shared stable system prefix → task block → user settings → mail last in `<email>` delimiters; prompts versioned in files | Maximizes prefix-cache hits; same injection defenses everywhere | [0010](adr/0010-llm-client-lanes-residency.md) |
| L10 | Digest batch timing | Daytime digest items summarized on arrival in the `batch` lane; the overnight backlog is processed right after the 09:00 load; items not done by 09:05 go out as titles and are edited in later | Spreads load; small morning batch | [0015](adr/0015-notification-policy-and-digest.md) |
| L11 | Evaluation | Redacted header fixtures in git for rule tests; raw curated eval set (≤200 mails) in a gitignored local folder, run with `gm eval` against the local model only | CI coverage without personal data leaving the Mac | [0017](adr/0017-eval-data-handling.md) |
| L12 | Model unavailable | Not running, timeout, 429/502/503/504: the summary job is **queued** (retried every **5 minutes**, no attempt used, waits forever). Other LLM errors retry with backoff and then the job goes **`dead`** (dashboard, `/metrics`); no ping without a summary | The model may be off (no launchd agent yet); a summary-less ping duplicates Gmail | [0015](adr/0015-notification-policy-and-digest.md) |

## Pending, owned elsewhere

- **Bonsai-27B-mlx-1bit** (`prism-ml/Bonsai-27B-mlx-1bit`): switch `GM_LLM_MODEL` once mlx-server-runtime
  supports it. Needed in that repo: the Prism MLX fork (1-bit g128 kernels; upstream MLX 0.31.2 has none)
  and native support for the Qwen3.5 hybrid architecture (3 linear-attention layers per full-attention
  layer). Re-run `gm eval` before switching. See [ADR-0011](adr/0011-model-qwen3-8b-now-bonsai-later.md).
- **Constrained JSON decoding** and **native tool calling** in mlx-server-runtime: tasks for that repo.

## Changes

- 2026-11-16: initial log.
- 2026-11-16: A3/A9 changed from compile-checked `query!` macros with an offline check in CI to
  runtime-checked `sqlx::query` / `query_as`, verified by the test suite; CI runs `gm rules` instead
  of `cargo sqlx prepare --check`. Confirmed by the owner after the port.
- 2026-11-16, confirmed by the owner after the port:
  - A7: Pub/Sub pull is implemented after all (it was poll-only in the first port), with the
    choices above: `pubsub` scope on the Gmail consent, safety poll every 15 min, and gm provisioning
    the topic, subscription and publisher permission itself.
  - O1: no `gm install` command; the agents are registered once.
  - O3: the dashboard health panel and the Telegram health alert are built in Phase 2 (workstream
    2E), next to the digest scheduler. Phase 1 has dashboard counts and `/metrics`.
  - T16: until model triage lands (Phase 2), unmatched mail is notified with a summary, as in Phase 0.
- 2026-11-16, decided by the owner while preparing the service install: the model agent is not
  loaded yet; P4 and T4 changed so urgent mail needing a summary also waits (night and outage),
  rule-only notifications still go out at any hour; new L12 (queue on outage every 5 min, dead on
  real errors, never a summary-less ping).
- 2026-11-16, decided by the owner during Google Cloud setup: new O6 (consent pages on a separate
  Vercel project) over GitHub Pages, an existing domain, or IMAP with an app password.
- 2026-11-16, still to review:
  - Defaults chosen without a question: `GM_LLM_MAX_INFLIGHT=2`, summary output cap 400 tokens,
    retention sweep every 6 h, stale-job requeue after 10 min, 7 days of log files, backup at 03:30,
    launchd labels `com.gm.*`, eval folder `~/.local/share/gm/eval/`.

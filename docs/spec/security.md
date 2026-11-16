# Spec: security and privacy

The system holds long-lived access to the owner's Google account and reads mail written by anyone on
the internet. This spec describes what we protect, from whom, and the controls in each phase.
Trust-boundary diagram: [architecture §10](../architecture.md#10-trust-boundaries).

## Assets

| Asset | Why it matters |
|-------|----------------|
| Google refresh token | Long-lived access to the mailbox (later Calendar). |
| Mail content and metadata | Private correspondence, financial and personal data (card usage, orders). |
| Ability to act as the owner | Sending mail, changing calendars (Phase 3+). |
| Telegram bot token | Lets anyone impersonate the bot or read its updates. |
| Master key, session secret, metrics token | Compromise breaks token encryption or authentication. |
| Eval set | Real mails kept for model evaluation. |

## Threat actors

| Actor | Capability |
|-------|-----------|
| **Mail sender** (most important) | Controls subject, body, sender name and attachment names; can send at will. |
| Other websites in the owner's browser | Can make the browser send requests to `localhost` (CSRF, DNS rebinding). |
| Device on the tailnet | Can reach the web UI through `tailscale serve`; still needs the owner's session. |
| Stranger on Telegram | Can find and message the bot. |
| Malware on the Mac | Out of scope (it can read the Keychain with the user's consent and the database). |

## Threats and controls

| # | Threat | Control | Phase |
|---|--------|---------|-------|
| T1 | **Prompt injection** in mail makes the system act | The model that reads mail has **no tools**; targets come only from headers, config or the owner's words; every external action needs approval with a full preview; mail is delimited and the delimiter cannot be closed (`clean::sanitize_delimiters`). | 1–3 |
| T2 | Injection makes a summary misleading | `system.md` forbids following instructions or repeating links/payment details; the Gmail link is built by code; Analyze flags phishing (Phase 3). Residual risk: the summary is advisory. | 1 |
| T3 | Someone else signs in | OIDC id_token from the token endpoint over TLS (OIDC Core 3.1.3.7): `aud` = client id, `iss` = Google, `exp`, `email_verified`, email = `GM_ALLOWED_EMAIL`. PKCE + random state in a signed, short-lived `gm_oauth` cookie. The connected Google account must equal the signed-in owner. | 1 |
| T4 | CSRF | Cookies `HttpOnly`, `SameSite=Lax`; state changes are POST only; POSTs with a foreign `Origin` → 403. | 1 |
| T5 | DNS rebinding | Host allowlist: `localhost`, `127.0.0.1`, hosts of `GM_BASE_URL` / `GM_PUBLIC_URL`; others → 400. | 1 |
| T6 | Exposure to the internet | Bind loopback only (config refuses anything else); no inbound webhooks; remote access through Tailscale. | 1 |
| T7 | Refresh-token theft from the DB or a backup | AES-256-GCM with a random nonce; key in the Keychain, never in the DB, `.env` or backups. Minimal scopes. | 1 |
| T8 | Secrets in logs, argv or errors | Secrets read from the Keychain at startup; `gm keychain set` lets `security` prompt for the value (never argv); Telegram URLs (which contain the token) are never logged and reqwest errors are stripped of URLs; refresh and access tokens never logged. | 1 |
| T9 | Markup injection in Telegram | Plain text only (no `parse_mode`); link previews disabled. | 1 |
| T10 | Forged session or state cookie | HMAC-SHA256 signed with the Keychain `session-secret`, bound to a purpose (`session` / `oauth`), with expiry; constant-time comparison. | 1 |
| T11 | Metrics leak data | Bearer token compared in constant time; endpoint disabled (404) without a token; counts only. | 1 |
| T12 | A settings value injects text into prompts | Settings are writable only by the signed-in owner; language names must match `^\p{L}[\p{L}\p{M} ()'\-]*$`, 2–40 characters; `ui_language` from a fixed list. | 1 |
| T18 | Pub/Sub messages forged or replayed | A notification only names a mailbox to sync; gm then reads Gmail itself, so a forged message can at most cause an extra sync. Only active accounts are synced; malformed messages are dropped. The subscription lives in the owner's project, readable only with the owner's token. | 1 |
| T13 | A bad `rules.yaml` edit hides security mail | Security rules are first in the file; `gm rules` and CI check every rule's examples; an invalid file keeps the previous rules. | 1 |
| T14 | Stranger controls the bot | Only updates from `GM_TELEGRAM_CHAT_ID` are processed. | 3 |
| T15 | An action executes something other than what was approved | Immutable approval payload with sha256; executor re-checks hash and the live draft; approvals expire. | 3 |
| T16 | Replay / double execution | Approval row locked and must be `pending`; executors idempotent. | 3 |
| T17 | Model invents a recipient | Recipients never come from model output. | 3 |

## Scopes

| Scope | Why | Phase |
|-------|-----|-------|
| `openid`, `email` | Sign-in identity | 1 |
| `gmail.readonly` | Read history and messages; `users.watch` | 1 |
| `pubsub` | Pub/Sub mode only: create gm's topic and subscription, grant Gmail publish rights, pull and acknowledge. Broad (any Cloud project the account can access), accepted to avoid a second, plaintext credential (decision A7) | 1 |
| `gmail.compose` | Create and send drafts, only through an approved executor (`messages.send` is never called) | 3 |
| `calendar.readonly` | Calendar notifications | 4 |

Never requested: `https://mail.google.com/`, `cloud-platform`, `gmail.modify`, Drive, Sheets (the ledger is in the
backlog, [ADR-0007](../adr/0007-sheets-for-notes-and-ledger.md)), contacts, account settings. Adding a
scope means adding it to `google::REQUIRED_SCOPES`; the connect callback then rejects consents that
lack it and the dashboard asks for a reconnect.

## Secrets

Stored in the macOS Keychain under service `google-management`
([ADR-0013](../adr/0013-ops-launchd-keychain-tailscale.md)): `master-key`, `session-secret`,
`google-client-secret`, `telegram-bot-token`, `metrics-token`. `.env` holds only non-secret
configuration. Tests and CI use `GM_SECRET_<NAME>` environment variables with throwaway values.

## Privacy and data handling

- Full mail bodies are never stored; handlers re-fetch them from Gmail.
- Stored per event: sender, subject, date, snippet, labels, attachment names, link, verdict
  (including values a rule extracted, such as a card amount), summary.
- Retention: events, finished jobs and LLM call records are deleted after 60 days.
- Backups (nightly, 14 kept, `~/Backups/google-management/`) contain the same data and the encrypted
  tokens, but not the key.
- Inference is local only; there is no cloud fallback.
- Eval data ([ADR-0017](../adr/0017-eval-data-handling.md)): git holds only redacted header examples
  (no names, real amounts, IPs or order numbers). The raw eval set (≤ 200 mails) lives in
  `~/.local/share/gm/eval/`, is exempt from retention, and is never committed or sent anywhere.

## Residual risks (accepted)

- Malware running as the owner can read the database and ask the Keychain for the master key.
- A misleading but well-formed summary can still mislead; the original mail is one tap away.
- Mail older than Gmail's history window during a long outage is only recovered for the last 2 days.
- The OAuth app is the owner's own unverified Google Cloud project; it must be "In production" or
  refresh tokens expire after 7 days.

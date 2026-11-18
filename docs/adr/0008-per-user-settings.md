# 0008. Settings stored per user and edited in the app; deployment stays single-user

- Status: Accepted
- Date: 2026-11-16

## Context

The summary language was a global environment variable (`GM_SUMMARY_LANGUAGE`). Changing it meant
editing `.env` and restarting, and it would be wrong for anyone but the owner. More user-facing
choices are coming (translation target in Phase 1; tone, signature, quiet hours, VIPs in Phase 3D).
There is one user today, but the owner may add others later.

## Decision

- User-facing behavior is configured **per user, in the app**: a `user_settings` table keyed by the
  user's email, holding one JSONB document with typed preferences.
- New preferences are keys in the settings document.
- Environment variables only provide **defaults** for users without saved settings
  (`GM_DEFAULT_SUMMARY_LANGUAGE`; the old `GM_SUMMARY_LANGUAGE` name is still accepted).
- Code that needs settings resolves the user from the event's account (`AccountStore::owner_of`).
- Values that reach LLM prompts are validated strictly (language names only).
- General preferences are edited on Settings. Summary language can also be set via Telegram
  `/language`. The Mail rules page edits the user's [route overrides](0012-rules-engine-v2.md).

Code: [`user_settings.rs`](../../crates/gm/src/user_settings.rs),
[`google/accounts.rs`](../../crates/gm/src/google/accounts.rs).

## Consequences

- The owner changes the language without touching files or restarting; the next summary uses it.
- Settings code is already multi-user. The rest of the system is **still single-user**. Supporting a
  second user would additionally need:
  1. an allow-list of emails instead of `GM_ALLOWED_EMAIL`;
  2. a Telegram chat id per user (moved from env to `user_settings`) and routing of notifications,
     replies and approvals to the right chat;
  3. dashboard and API queries filtered by user.
- JSONB settings are flexible but not queryable by column; that's fine for a handful of users.

## Alternatives considered

- **Keep environment variables:** simplest, but needs a restart and can't differ per user.
- **One column per setting:** explicit, but every new preference needs a migration.
- **Full multi-user now:** speculative work across auth, Telegram routing and rules with no current
  user to serve.

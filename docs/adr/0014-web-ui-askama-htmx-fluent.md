# 0014. Web UI: askama templates, htmx, Fluent translations (vi / en / ja)

- Status: Accepted
- Date: 2026-11-16

## Context

The web UI is small (dashboard, settings, approvals, later digest and rules views) and must be
localized in Vietnamese, English and Japanese. The owner reads all three.

## Decision

- Server-rendered **askama** templates compiled into the binary (type-checked at build time), with
  **htmx** for partial updates. No JavaScript build step; htmx is vendored as a static file.
- **Fluent** (`.ftl` files under `locales/{en,vi,ja}/`) for every UI and Telegram string. The locale
  comes from the user's `ui_language` setting (default from `Accept-Language`, then `en`).
  Missing keys fall back to English and are reported by a test.
- LLM output language follows the user's summary/translation language settings, separate from the UI
  locale.

## Consequences

- Template edits need a recompile (seconds).
- Adding a language means adding one `.ftl` directory.

## Alternatives considered

- **minijinja:** close to the Python templates and hot-reloadable; errors only at render time.
- **maud:** type-checked Rust macros; markup harder to edit.
- **gettext/.po:** weaker plural and grammar handling than Fluent.

# 0013. launchd services, Keychain secrets, Tailscale for remote access

- Status: Accepted
- Date: 2026-11-16

## Context

gm runs unattended on the owner's Mac next to mlx-server-runtime and Postgres. Phase 0 kept secrets
(Fernet master key, bot token, OAuth client secret) in `.env`. The owner wants the UI reachable from
their phone without public hosting.

## Decision

- **launchd** user agents: `com.gm.server` (`gm serve`, KeepAlive), `com.gm.mlx` (mlx-server-runtime,
  started and stopped by the residency controller with `launchctl kickstart` / `launchctl kill`), and
  `com.gm.backup` (nightly). Postgres keeps running in Docker with `restart: unless-stopped`.
  Plists live in `deploy/launchd/`, registered once with the steps in `deploy/README.md`; there is
  no install command (decision O1).
- **Secrets in the macOS Keychain** (service `google-management`): `master-key` (encrypts refresh
  tokens), `telegram-bot-token`, `google-client-secret`, `session-secret`, `metrics-token`.
  `gm keychain set <name>` stores them. `.env` holds non-secret config only. In tests and CI, the
  same names can come from environment variables (`GM_SECRET_<NAME>`).
- **Remote access:** gm binds `127.0.0.1`; `tailscale serve` exposes it on the owner's tailnet with
  HTTPS. No Vercel, no public webhooks. `GM_PUBLIC_URL` is the tailnet URL; the OAuth redirect stays
  on localhost.

## Consequences

- A copied `.env` or database dump does not reveal tokens.
- Sleep: the Mac must stay awake during model hours (caffeinate or energy settings); missed schedules
  run once on wake.

## Alternatives considered

- **`.env` secrets:** simplest, plaintext on disk.
- **Vercel UI:** a public surface with access to private data; rejected.
- **Docker for everything:** no Metal GPU access for MLX inside containers.

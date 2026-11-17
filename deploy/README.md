# Running gm as a service (macOS)

Decisions: [ADR-0013](../docs/adr/0013-ops-launchd-keychain-tailscale.md),
[ADR-0016](../docs/adr/0016-operations-baseline.md).

## One-time setup

In Google Cloud, enable the **Gmail API**, **Google Calendar API** and **Cloud Pub/Sub API** for the project in
`GM_GOOGLE_PROJECT_ID`; gm creates its topic and subscription itself.

```bash
docker compose up -d                       # Postgres, restarts with Docker
cargo build --release
./target/release/gm gen-key                # run twice: one value for master-key, one for session-secret
./target/release/gm keychain set master-key
./target/release/gm keychain set session-secret
./target/release/gm keychain set google-client-secret
./target/release/gm keychain set telegram-bot-token
./target/release/gm keychain set metrics-token   # optional; enables /metrics
./target/release/gm keychain list
cp .env.example .env                       # non-secret config only
./target/release/gm rules rules.yaml
```

## Install the launchd agents

A one-time registration; afterwards the agents start on every login (there is no install
command). Plists: [`deploy/launchd/`](launchd/).

The model is not one of gm's agents: mlx-air runs it (instance `gm`, launchd
`com.mlx-air.instance.gm`), and gm decides when: it calls `mlx-air stop gm` after model hours and
`mlx-air serve --detach --name gm` in model hours or for an owner request
([ADR-0010](../docs/adr/0010-llm-client-lanes-residency.md); the `GM_MLX_*` and `GM_MODEL_*`
settings in [`.env.example`](../.env.example)). `mlx-air stop` removes the instance's launchd agent
and config, so a start recreates them from `GM_LLM_MODEL` and the port of `GM_LLM_BASE_URL`. The
switch on the dashboard turns this off. While the model is down, summaries wait in the queue and rule-only pings
still go out ([ADR-0015](../docs/adr/0015-notification-policy-and-digest.md)); after 15 minutes in model hours the health check alerts.

```bash
mkdir -p ~/Library/Logs/gm
for name in com.gm.server com.gm.backup; do
  sed -e "s#__REPO__#$PWD#g" -e "s#__HOME__#$HOME#g" deploy/launchd/$name.plist > ~/Library/LaunchAgents/$name.plist
  launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/$name.plist
done
```

- Check what is installed and running: `./target/release/gm status` (launchd agents, running build,
  web, model, migrations, queue; exit code 1 on a failure).
- Restart after an upgrade: `cargo build --release && launchctl kickstart -k gui/$(id -u)/com.gm.server`
- Stop: `launchctl bootout gui/$(id -u)/com.gm.server`
- Logs: `~/Library/Logs/gm/` (JSON, rotated daily, 7 kept).
- The first Keychain read from launchd may show a macOS prompt; choose "Always Allow".

## OAuth consent pages

Publishing the Google OAuth app (so refresh tokens don't expire after 7 days as in "Testing") needs a
homepage and a privacy policy. They are the two static pages in [`oauth-pages/`](oauth-pages/),
hosted on their own Vercel project (`google-management-bay.vercel.app`, also the authorized domain),
apart from the owner's other domains and Cloud projects. They hold no data and gm doesn't depend on
them at runtime.

## Remote access

```bash
tailscale serve --bg 8080
```

Set `GM_PUBLIC_URL` to the tailnet URL. Google sign-in still happens on `http://localhost:8080` on the Mac.

## Backups

`com.gm.backup` runs `scripts/backup.sh` at 03:30 and keeps 14 dumps in `~/Backups/google-management/`.
To restore:

```bash
docker compose exec -T postgres pg_restore -U gm -d gm --clean --if-exists < ~/Backups/google-management/gm-YYYYMMDD-HHMMSS.dump
```

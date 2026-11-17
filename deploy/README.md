# Running gm as a service (macOS)

Decisions: [ADR-0013](../docs/adr/0013-ops-launchd-keychain-tailscale.md),
[ADR-0016](../docs/adr/0016-operations-baseline.md).

## One-time setup

In Google Cloud, enable the **Gmail API** and the **Cloud Pub/Sub API** for the project in
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

A one-time registration; afterwards the agents start on every login (decision O1, no install
command). The model agent (`com.gm.mlx`) is not loaded for now: until it is, summaries simply wait
in the queue and rule-only pings still go out (decision L12).

When you install it later (same loop with `com.gm.mlx`), turn on **Load/unload by gm** on the
dashboard: gm then starts the model at 09:00 and stops it at 19:00 (after in-flight requests, at most
5 min). The agent is not kept alive by launchd; with the switch off it starts at login and runs until
you stop it (`launchctl kill TERM gui/$(id -u)/com.gm.mlx`).

```bash
mkdir -p ~/Library/Logs/gm
for name in com.gm.server com.gm.backup; do
  sed -e "s#__REPO__#$PWD#g" -e "s#__HOME__#$HOME#g" deploy/launchd/$name.plist > ~/Library/LaunchAgents/$name.plist
  launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/$name.plist
done
```

- Restart after an upgrade: `cargo build --release && launchctl kickstart -k gui/$(id -u)/com.gm.server`
- Stop: `launchctl bootout gui/$(id -u)/com.gm.server`
- Logs: `~/Library/Logs/gm/` (JSON, rotated daily, 7 kept).
- The first Keychain read from launchd may show a macOS prompt; choose "Always Allow".

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

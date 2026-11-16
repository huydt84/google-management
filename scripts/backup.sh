#!/usr/bin/env bash
# Nightly database backup (ADR-0016). Runs pg_dump inside the Postgres container, so no local
# client is needed. Refresh tokens in the dump are encrypted; the key stays in the Keychain.
# Restore:  docker compose exec -T postgres pg_restore -U gm -d gm --clean --if-exists < FILE
set -euo pipefail
cd "$(dirname "$0")/.."
DEST="${GM_BACKUP_DIR:-$HOME/Backups/google-management}"
KEEP="${GM_BACKUP_KEEP:-14}"
# launchd starts jobs with a minimal PATH; look in the usual Docker install locations too.
export PATH="$PATH:$HOME/.rd/bin:/usr/local/bin:/opt/homebrew/bin:/Applications/Docker.app/Contents/Resources/bin"
DOCKER="${DOCKER:-$(command -v docker)}"
mkdir -p "$DEST"
out="$DEST/gm-$(date +%Y%m%d-%H%M%S).dump"
"$DOCKER" compose exec -T postgres pg_dump -U gm -d gm -Fc > "$out.tmp"
mv "$out.tmp" "$out"
ls -1t "$DEST"/gm-*.dump | tail -n +"$((KEEP + 1))" | xargs -r rm -f
echo "$(date -Iseconds) backup ok: $out"

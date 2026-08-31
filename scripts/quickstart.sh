#!/usr/bin/env bash
# Opinionated, minimal-resource launch for a single-node Sandhi proxy.
#
# Zero-arg usage: ./scripts/quickstart.sh
#   - builds the release binaries if missing (smaller/faster than debug)
#   - sets up persistent state under ~/.sandhi (admin token, SQLite usage store)
#   - starts sandhi-proxy in the background if not already running
#   - registers every provider listed in ~/.sandhi/providers.json (auto-created with a
#     single local Ollama entry on first run if the file doesn't exist yet)
#
# Point a provider at a different machine by editing providers.json — e.g. Ollama
# running on another host in your fleet:
#   { "provider": "ollama", "label": "dataserver3", "base_url": "http://192.168.1.89:11434" }
# Multiple entries for the same provider (different "label") coexist fine; route different
# workflows to different hosts via the credential_id (e.g. "ollama:dataserver3") a vkey mints against.
#
# Config (env, all optional):
#   SANDHI_HOME       state dir                         default: ~/.sandhi
#   SANDHI_BIND       proxy listen address               default: 127.0.0.1:8787
#   SANDHI_DASHBOARD_PUBLIC  1 = dashboard readable without a token (still LAN/localhost only)
#                                                         default: 1 (single-node dev trust)
#
# Usage: quickstart.sh [status|stop]
set -euo pipefail

SANDHI_HOME="${SANDHI_HOME:-$HOME/.sandhi}"
BIND="${SANDHI_BIND:-127.0.0.1:8787}"
DASHBOARD_PUBLIC="${SANDHI_DASHBOARD_PUBLIC:-1}"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PROXY_BIN="$REPO_ROOT/target/release/sandhi-proxy"
CLI_BIN="$REPO_ROOT/target/release/sandhi"
STORE="$SANDHI_HOME/usage.db"
TOKEN_FILE="$SANDHI_HOME/admin_token"
PID_FILE="$SANDHI_HOME/proxy.pid"
BIND_FILE="$SANDHI_HOME/bind"
LOG_FILE="$SANDHI_HOME/proxy.log"
PROVIDERS_FILE="$SANDHI_HOME/providers.json"

# status/stop need the bind address a prior `start` actually used, not whatever SANDHI_BIND
# happens to be set to on THIS invocation — read it back if the caller didn't override it.
if [ -f "$BIND_FILE" ] && [ -z "${SANDHI_BIND:-}" ]; then
  BIND="$(cat "$BIND_FILE")"
fi

log() { printf '%s\n' "$*"; }

is_running() {
  [ -f "$PID_FILE" ] && kill -0 "$(cat "$PID_FILE")" 2>/dev/null
}

cmd="${1:-start}"

if [ "$cmd" = "stop" ]; then
  if is_running; then
    pid="$(cat "$PID_FILE")"
    kill "$pid" && rm -f "$PID_FILE"
    log "stopped (was pid $pid)"
  else
    log "not running"
  fi
  exit 0
fi

if [ "$cmd" = "status" ]; then
  if is_running; then
    log "running, pid $(cat "$PID_FILE"), bind $BIND, store $STORE"
  else
    log "not running"
  fi
  exit 0
fi

mkdir -p "$SANDHI_HOME"

# --- 1. binaries: release build only — smaller and faster than debug, and this is meant to
# run continuously in the background, not be iterated on. -------------------------------------
if [ ! -x "$PROXY_BIN" ] || [ ! -x "$CLI_BIN" ]; then
  log "building release binaries (first run only, ~1-2 min)..."
  (cd "$REPO_ROOT" && cargo build --release -p sandhi-proxy --bin sandhi-proxy --bin sandhi)
fi

# --- 2. persistent identity: admin token + durable SQLite store, both idempotent -------------
if [ ! -f "$TOKEN_FILE" ]; then
  openssl rand -hex 24 > "$TOKEN_FILE"
  chmod 600 "$TOKEN_FILE"
  log "generated admin token -> $TOKEN_FILE"
fi
ADMIN_TOKEN="$(cat "$TOKEN_FILE")"

# --- 3. start the proxy if not already running ------------------------------------------------
if is_running; then
  log "proxy already running (pid $(cat "$PID_FILE"))"
else
  SANDHI_BIND="$BIND" \
  SANDHI_STORE="$STORE" \
  SANDHI_ADMIN_TOKEN="$ADMIN_TOKEN" \
  SANDHI_DASHBOARD_PUBLIC="$DASHBOARD_PUBLIC" \
    "$PROXY_BIN" > "$LOG_FILE" 2>&1 &
  echo $! > "$PID_FILE"
  echo "$BIND" > "$BIND_FILE"
  # Wait for the listener rather than a fixed sleep — first boot can take a beat.
  for _ in $(seq 1 30); do
    curl -sf -o /dev/null "http://$BIND/dashboard" 2>/dev/null && break
    sleep 0.3
  done
  if ! curl -sf -o /dev/null "http://$BIND/dashboard" 2>/dev/null; then
    log "proxy did not come up — check $LOG_FILE"; exit 1
  fi
  log "started proxy, pid $(cat "$PID_FILE"), listening on http://$BIND"
fi

# --- 4. provider bootstrap: opinionated default (one local, keyless Ollama) on first run,
# otherwise register whatever is listed in providers.json. Idempotent — already-registered
# credential_ids are skipped rather than re-added. --------------------------------------------
if [ ! -f "$PROVIDERS_FILE" ]; then
  cat > "$PROVIDERS_FILE" <<'EOF'
[
  { "provider": "ollama", "label": "default", "base_url": "http://localhost:11434", "secret": "" }
]
EOF
  log "wrote default providers config -> $PROVIDERS_FILE (edit this to add remote hosts)"
fi

existing="$(curl -sf -H "Authorization: Bearer $ADMIN_TOKEN" "http://$BIND/admin/keys" \
  | python3 -c 'import json,sys; print(",".join(k["credential_id"] for k in json.load(sys.stdin).get("keys", [])))' 2>/dev/null || echo "")"

python3 - "$PROVIDERS_FILE" <<'PYEOF' | while IFS=$'\t' read -r provider label base_url secret; do
import json, sys
with open(sys.argv[1]) as f:
    for entry in json.load(f):
        print(entry["provider"], entry.get("label", "default"), entry.get("base_url", ""), entry.get("secret", ""), sep="\t")
PYEOF
  cred_id="$provider:$label"
  if printf '%s' "$existing" | grep -qx "$cred_id" 2>/dev/null || printf '%s\n' "${existing//,/$'\n'}" | grep -qx "$cred_id"; then
    log "credential $cred_id already registered, skipping"
    continue
  fi
  resp="$(printf '%s' "$secret" | curl -sf -X POST "http://$BIND/admin/keys" \
    -H "Authorization: Bearer $ADMIN_TOKEN" -H "Content-Type: application/json" \
    -d "$(python3 -c "import json,sys; print(json.dumps({'provider':sys.argv[1],'label':sys.argv[2],'base_url':sys.argv[3] or None,'secret':sys.stdin.read()}))" "$provider" "$label" "$base_url")")"
  log "registered $cred_id -> $base_url"
done

log ""
log "Sandhi is up:"
log "  dashboard:    http://$BIND/dashboard"
log "  admin token:  $TOKEN_FILE (paste into the dashboard's token field to unlock actions)"
log "  providers:    $PROVIDERS_FILE (edit + re-run this script to add more, including remote hosts)"
log "  mint a key:   $CLI_BIN --admin-token \"\$(cat $TOKEN_FILE)\" keys share ollama:default --subject myagent"
log "  stop:         $0 stop"

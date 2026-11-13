#!/usr/bin/env bash
# Local single-node starter: no client authentication or identity-provider dependency.
# Optional protected profile: SANDHI_AUTH_MODE=tokens ./scripts/quickstart.sh
# Provider credentials/state remain under the private SANDHI_HOME, never in this script.
set -euo pipefail
umask 077

SANDHI_HOME="${SANDHI_HOME:-$HOME/.sandhi}"
BIND="${SANDHI_BIND:-127.0.0.1:8787}"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PROXY_BIN="$REPO_ROOT/target/release/sandhi-proxy"
CLI_BIN="$REPO_ROOT/target/release/sandhi"
STORE="$SANDHI_HOME/usage.db"
TOKEN_FILE="$SANDHI_HOME/admin_token"
MODE_FILE="$SANDHI_HOME/auth_mode"
PID_FILE="$SANDHI_HOME/proxy.pid"
BIND_FILE="$SANDHI_HOME/bind"
LOG_FILE="$SANDHI_HOME/proxy.log"
PROVIDERS_FILE="$SANDHI_HOME/providers.json"
if [ -f "$BIND_FILE" ] && [ -z "${SANDHI_BIND:-}" ]; then BIND="$(cat "$BIND_FILE")"; fi
log() { printf '%s\n' "$*"; }
is_running() { [ -f "$PID_FILE" ] && kill -0 "$(cat "$PID_FILE")" 2>/dev/null; }
cmd="${1:-start}"
if [ "$cmd" = "stop" ]; then
  if is_running; then kill "$(cat "$PID_FILE")"; rm -f "$PID_FILE"; log "stopped"; else log "not running"; fi
  exit 0
fi
if [ "$cmd" = "status" ]; then
  if is_running; then log "running, pid $(cat "$PID_FILE"), bind $BIND, store $STORE"; else log "not running"; fi
  exit 0
fi
if [ "$cmd" != "start" ]; then log "usage: $0 [start|status|stop]"; exit 1; fi
# Preserve an existing protected quickstart installation. A fresh home defaults to none.
if [ "${SANDHI_AUTH_MODE+x}" = x ]; then MODE="$SANDHI_AUTH_MODE"
elif [ "${SANDHI_OIDC_CONFIG+x}" = x ]; then MODE=oidc
elif [ -f "$MODE_FILE" ]; then MODE="$(cat "$MODE_FILE")"
elif [ -f "$TOKEN_FILE" ]; then MODE=tokens
else MODE=none
fi
case "$MODE" in
  none|tokens) ;;
  oidc) log "For OIDC, configure the issuer and use docs/operator/oidc-sso.md; this local bootstrap supports none or tokens."; exit 1 ;;
  *) log "SANDHI_AUTH_MODE must be none, tokens or oidc."; exit 1 ;;
esac
if [ "$MODE" = none ] && [ "${SANDHI_OIDC_CONFIG+x}" = x ]; then
  log "OIDC configuration conflicts with explicit no-auth mode; choose the intended mode."; exit 1
fi
mkdir -p "$SANDHI_HOME"
EXPECTED_VERSION="$(python3 -c 'import sys,tomllib; print(tomllib.load(open(sys.argv[1], "rb"))["workspace"]["package"]["version"])' "$REPO_ROOT/Cargo.toml")"
if [ ! -x "$PROXY_BIN" ] || [ ! -x "$CLI_BIN" ] \
  || [ "$("$PROXY_BIN" --version 2>/dev/null || true)" != "sandhi-proxy $EXPECTED_VERSION" ] \
  || [ "$("$CLI_BIN" --version 2>/dev/null || true)" != "sandhi $EXPECTED_VERSION" ]; then
  log "building release binaries..."
  (cd "$REPO_ROOT" && cargo build --release --locked -p sandhi-proxy --features sentinelpass-ipc,policy-onnx --bin sandhi-proxy --bin sandhi)
fi
# Token auth is optional. An explicitly supplied admin token can protect the control plane
# even in none mode. Otherwise no secret is generated or needed by the fresh starter.
if [ "$MODE" = tokens ] && [ "${SANDHI_ADMIN_TOKEN+x}" != x ]; then
  if [ ! -f "$TOKEN_FILE" ]; then openssl rand -hex 24 > "$TOKEN_FILE"; chmod 600 "$TOKEN_FILE"; fi
  SANDHI_ADMIN_TOKEN="$(cat "$TOKEN_FILE")"; export SANDHI_ADMIN_TOKEN
fi
if [ "${SANDHI_ADMIN_TOKEN+x}" = x ] && [ -z "$SANDHI_ADMIN_TOKEN" ]; then
  log "An explicitly configured admin token must not be empty."; exit 1
fi
if is_running; then log "proxy already running (restart to change authentication mode)"
else
  # Keep the selected credential backend; auth mode does not move or replace secrets.
  SANDHI_AUTH_MODE="$MODE" SANDHI_BIND="$BIND" SANDHI_STORE="$STORE" \
  SANDHI_DASHBOARD_PUBLIC="${SANDHI_DASHBOARD_PUBLIC:-0}" \
    "$PROXY_BIN" > "$LOG_FILE" 2>&1 &
  echo $! > "$PID_FILE"; echo "$BIND" > "$BIND_FILE"
  for _ in $(seq 1 50); do
    curl -sf -o /dev/null "http://$BIND/readyz" 2>/dev/null && break
    sleep 0.3
  done
  if ! curl -sf -o /dev/null "http://$BIND/readyz" 2>/dev/null; then
    log "proxy did not become ready; check $LOG_FILE"; exit 1
  fi
  printf '%s\n' "$MODE" > "$MODE_FILE"
fi
if [ ! -f "$PROVIDERS_FILE" ]; then
  cat > "$PROVIDERS_FILE" <<'PROVIDERS'
[
  { "provider": "ollama", "label": "default", "base_url": "http://localhost:11434", "secret": "" }
]
PROVIDERS
  log "wrote $PROVIDERS_FILE; edit it to configure other backends"
fi
# Secrets are read inside this process and HTTP request bodies, never interpolated into
# command-line arguments or printed. Disable ambient HTTP proxies for this owned admin call.
python3 - "$PROVIDERS_FILE" "$BIND" "$MODE" "$EXPECTED_VERSION" <<'PYBOOT'
import json, os, sys, urllib.request
path, bind, expected_mode, expected_version = sys.argv[1:]
base = "http://" + bind
class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None
opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())
headers = {"Content-Type": "application/json"}
if "SANDHI_ADMIN_TOKEN" in os.environ:
    headers["Authorization"] = "Bearer " + os.environ["SANDHI_ADMIN_TOKEN"]
def request(method, route, value=None):
    body = None if value is None else json.dumps(value).encode()
    with opener.open(urllib.request.Request(base + route, data=body, headers=headers, method=method), timeout=15) as response:
        return json.load(response)
try:
    if request("GET", "/version")["package_version"] != expected_version:
        raise ValueError("running gateway version differs; stop the old writer before upgrading")
    if request("GET", "/auth/session")["mode"] != expected_mode:
        raise ValueError("running gateway mode differs; restart before changing authentication")
    existing = {item["credential_id"] for item in request("GET", "/admin/keys").get("keys", [])}
    with open(path) as source:
        entries = json.load(source)
    for item in entries:
        provider, label = item["provider"], item.get("label", "default")
        credential_id = provider + ":" + label
        if credential_id not in existing:
            request("POST", "/admin/keys", {"provider": provider, "label": label,
                "base_url": item.get("base_url") or None, "secret": item.get("secret", "")})
            print("registered " + credential_id)
except (OSError, ValueError, KeyError, TypeError):
    raise SystemExit("Gateway bootstrap failed; check mode, private provider configuration and gateway logs.") from None
PYBOOT
log ""
log "Sandhi is ready at http://$BIND/dashboard"
log "  client authentication: $MODE"
if [ "${SANDHI_ADMIN_TOKEN+x}" = x ]; then
  log "  operator actions use the configured admin token"
else
  log "  no authentication: all clients share the starter identity and budget"
fi
log "  providers: $PROVIDERS_FILE"
log "  configure a default route or SANDHI_STARTER_UPSTREAM when using multiple backends"
log "  stop: $0 stop"

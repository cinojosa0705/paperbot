#!/bin/sh
# Install/refresh the paperbot launchd agents (engine, telegram responder, caffeinate) from ~/.paperbot/env.
# They restart within seconds if they die and start again at login. Re-run after changing the env file.
set -eu
REPO="$(cd "$(dirname "$0")/.." && pwd)"
ENV_FILE="${PAPERBOT_ENV:-$HOME/.paperbot/env}"
AGENTS="$HOME/Library/LaunchAgents"
mkdir -p "$AGENTS" "$HOME/.paperbot"
# shellcheck disable=SC1090
. "$ENV_FILE"
STATE="${PAPERBOT_STATE:-$HOME/.paperbot/state.json}"
LEDGER="${PAPERBOT_LEDGER:-$HOME/.paperbot/ledger.jsonl}"
[ -f "$STATE" ] || { echo "no $STATE — run \`paperbot register\` first, or move an existing state.json/ledger.jsonl/params.json there"; exit 1; }
env_xml() {
  for k in PAPERBOT_API PAPERBOT_TG_TOKEN PAPERBOT_TG_CHAT PAPERBOT_TG_USERS PAPERBOT_RPC PAPERBOT_RPC_LOG_RANGE RUST_LOG; do
    v="$(eval "printf '%s' \"\${$k:-}\"")"; [ -n "$v" ] && printf '    <key>%s</key><string>%s</string>\n' "$k" "$v"
  done
  printf '    <key>RUST_LOG</key><string>%s</string>\n' "${RUST_LOG:-info}"
}
agent() { # label, log, program args (xml)
  cat > "$AGENTS/$1.plist" <<PL
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>$1</string>
  <key>ProgramArguments</key><array>
$3
  </array>
  <key>WorkingDirectory</key><string>$REPO</string>
  <key>EnvironmentVariables</key><dict>
$(env_xml)
  </dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ThrottleInterval</key><integer>10</integer>
  <key>StandardOutPath</key><string>$HOME/.paperbot/$2.log</string>
  <key>StandardErrorPath</key><string>$HOME/.paperbot/$2.log</string>
</dict></plist>
PL
  chmod 600 "$AGENTS/$1.plist"
  launchctl bootout "gui/$(id -u)/$1" 2>/dev/null || true
  launchctl bootstrap "gui/$(id -u)" "$AGENTS/$1.plist"
  launchctl kickstart -k "gui/$(id -u)/$1" 2>/dev/null || true
}
BIN="$REPO/target/release/paperbot"
agent com.paperbot.engine run "    <string>$BIN</string><string>--state</string><string>$STATE</string><string>--ledger</string><string>$LEDGER</string><string>run</string>"
agent com.paperbot.tg tg-serve "    <string>$BIN</string><string>--state</string><string>$STATE</string><string>--ledger</string><string>$LEDGER</string><string>tg-serve</string>"
agent com.paperbot.caffeinate caffeinate "    <string>/usr/bin/caffeinate</string><string>-i</string><string>-s</string>"
launchctl list | grep com.paperbot || true

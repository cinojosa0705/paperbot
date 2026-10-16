#!/bin/sh
# The thing to run when the Telegram group goes quiet: is the farm alive, and why not.
REPO="$(cd "$(dirname "$0")/.." && pwd)"
STATE="${PAPERBOT_STATE:-$HOME/.paperbot/state.json}"
DIR="$(dirname "$STATE")"
now=$(date +%s)
echo "== launchd"
launchctl list 2>/dev/null | grep paperbot || echo "  no paperbot agents loaded"
echo "== heartbeat"
if [ -f "$DIR/heartbeat" ]; then
  hb=$(cat "$DIR/heartbeat"); echo "  $((now - hb))s ago"
else
  echo "  no heartbeat file (engine never ran here)"
fi
echo "== params"
grep -E '"(market|move_pct|buffer_pct|paused|pause_reason|liquidate)"' "$DIR/params.json" 2>/dev/null | sed 's/^ */  /; s/,$//' 
echo "== state"
python3 - "$STATE" <<'EOF' 2>/dev/null || echo "  unreadable"
import json,sys,time
s=json.load(open(sys.argv[1]))
f=s.get("in_flight")
print(f"  cycles done {s.get('cycles_done')} · peak ${int(s.get('peak_equity',0))/1e18:.2f} · partial rows {s.get('partial_cycles',[])}")
print("  pair:", f"{f['coin']} {f['leverage']}x ids {f['position_ids']} open {int(time.time()-f['opened_at'])//60} min" if f else "none")
EOF
echo "== network"
for h in exchange.papertrade.xyz api.hyperliquid.xyz api.telegram.org; do
  if curl -s -o /dev/null -m 5 "https://$h/" ; then echo "  $h ok"; else echo "  $h UNREACHABLE"; fi
done
echo "== last log lines"
tail -5 "$HOME/.paperbot/run.log" 2>/dev/null | sed 's/\x1b\[[0-9;]*m//g' | cut -c1-160

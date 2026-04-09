#!/bin/bash
# Claude Code statusline hook for ccom
# Reads statusline JSON from stdin, extracts rate_limits + cost, writes to ~/.claude/ccom-rate-limits.json
#
# Install: add to Claude Code settings.json:
#   "statusLine": { "command": "/path/to/ccom-statusline.sh" }

python3 -c "
import sys, json, os
from datetime import datetime, timezone
try:
    data = json.load(sys.stdin)
    rl = data.get('rate_limits', {})
    cost = data.get('cost', {})
    outfile = os.path.expanduser('~/.claude/ccom-rate-limits.json')
    out = {'updated_at': datetime.now(timezone.utc).isoformat()}
    for key in ('five_hour', 'seven_day'):
        w = rl.get(key)
        if w:
            out[key] = {
                'used_percentage': w.get('used_percentage'),
                'resets_at': w.get('resets_at'),
            }
    if cost:
        out['cost'] = {
            'total_cost_usd': cost.get('total_cost_usd'),
        }
    with open(outfile, 'w') as f:
        json.dump(out, f)
    # Output statusline text
    parts = []
    fh = rl.get('five_hour', {})
    if fh and fh.get('used_percentage') is not None:
        parts.append(f\"5h:{fh['used_percentage']:.0f}%\")
    sd = rl.get('seven_day', {})
    if sd and sd.get('used_percentage') is not None:
        parts.append(f\"7d:{sd['used_percentage']:.0f}%\")
    if parts:
        print(' | '.join(parts))
except Exception:
    pass
"

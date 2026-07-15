#!/usr/bin/env bash
# Runs when the battery recovers above low_threshold_pct + hysteresis_pct (default 25%).
# Mirror of battery-low.sh: bring back whatever that stopped.
set -euo pipefail

logger -t waveshare-ups "battery recovered (${UPS_BATTERY_PCT}%) - restarting services"

# systemctl start plex
# systemctl start some-heavy-thing

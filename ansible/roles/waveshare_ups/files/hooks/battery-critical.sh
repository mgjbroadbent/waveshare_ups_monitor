#!/usr/bin/env bash
# Runs when the battery drops below hooks.critical_threshold_pct (default 5%).
# Fires after battery-low.sh, so services are already stopped by this point.
#
# The daemon deliberately never powers the machine off itself -- that policy lives here.
set -euo pipefail

logger -t waveshare-ups "battery critical (${UPS_BATTERY_PCT}%) - shutting down"

# Guard against shutting down if mains came back in the meantime.
if [[ "${UPS_EXTERNAL_POWER}" == "1" ]]; then
  logger -t waveshare-ups "external power present again - not shutting down"
  exit 0
fi

sync
systemctl poweroff

#!/usr/bin/env bash
# Runs when the battery drops below hooks.low_threshold_pct (default 20%).
# Stop non-essential services here so the remaining charge lasts longer.
#
# Environment provided by the daemon:
#   UPS_EVENT           battery_low
#   UPS_BATTERY_PCT     e.g. 18.42
#   UPS_BUS_VOLTAGE     volts, as measured on the load side
#   UPS_CURRENT_A       amps; positive = charging, negative = discharging
#   UPS_POWER_W         watts
#   UPS_CHARGING        1 or 0
#   UPS_EXTERNAL_POWER  1 or 0
set -euo pipefail

logger -t waveshare-ups "battery low (${UPS_BATTERY_PCT}%) - stopping non-essential services"

# systemctl stop plex
# systemctl stop some-heavy-thing

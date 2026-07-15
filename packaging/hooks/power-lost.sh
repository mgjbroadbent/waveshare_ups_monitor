#!/usr/bin/env bash
# Runs when mains power goes away (the UPS starts discharging).
# Fires regardless of battery level, so this is the place for early warning.
set -euo pipefail

logger -t waveshare-ups "mains lost - running on battery (${UPS_BATTERY_PCT}%)"

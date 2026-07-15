#!/usr/bin/env bash
# Runs when mains power comes back.
set -euo pipefail

logger -t waveshare-ups "mains restored (${UPS_BATTERY_PCT}%)"

#!/usr/bin/env bash
# Notification authorization from the ad-hoc bundles. Needs a human at an
# unlocked screen: macOS shows "abyss spike would like to send you
# notifications" and the script waits for the click.
#   spikes/notify.sh [sandboxed|plain] [wait-seconds]
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
which="${1:-sandboxed}"; wait="${2:-90}"
out="$(mktemp -d)"
echo "Click Allow on the notification prompt within $wait s. Output: $out/stdout.txt"
open -n -W --stdout "$out/stdout.txt" --stderr "$out/stderr.txt" "build/Spike-$which.app" --args notify "$wait" || true
cat "$out/stdout.txt"
# A second run, after the answer is stored, shows the settled state and a banner.
echo "--- second run (settled state)"
open -n -W --stdout "$out/stdout2.txt" --stderr "$out/stderr2.txt" "build/Spike-$which.app" --args notify 20 || true
cat "$out/stdout2.txt"

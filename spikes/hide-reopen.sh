#!/usr/bin/env bash
# GPUI App::hide / on_reopen / activate scenario against build/Spike-gpui.app.
#   spikes/hide-reopen.sh <scratch-dir>
# Run it with the screen unlocked to also see frames painted (a locked screen
# occludes the window, and GPUI stops its display link for an occluded window).
set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
D="${1:?scratch dir}"
rm -rf "$D"; mkdir -p "$D"
open -n --stdout "$D/stdout.txt" --stderr "$D/stderr.txt" build/Spike-gpui.app --args --cmd-dir "$D"
sleep 4
PID="$(pgrep -f 'Spike-gpui.app/Contents/MacOS/abyss-gpui-spike' | head -1)"
show() { echo "[$1] $(./cmd.sh "$D" state 0.8 | tail -1)"; build/winlist "$PID" | sed 's/^/      /'; }
echo "== A. App::hide, then reopen like a Dock click (open <bundle> sends the reopen Apple event)"
show "launched"
./cmd.sh "$D" hide 1 | sed 's/^/      /'
show "after hide"
sleep 3
show "3 s later, still hidden"
open build/Spike-gpui.app; sleep 2
echo "      log since reopen:"; grep -n "on_reopen" "$D/log.txt" | sed 's/^/      /'
show "after reopen"
echo "== B. App::hide, then cx.activate(true) in-process (the notification-click path)"
./cmd.sh "$D" hide 1 | sed 's/^/      /'
show "after hide"
./cmd.sh "$D" activate 1.5 | sed 's/^/      /'
show "after activate"
echo "== C. hide, then reopen again, to see whether on_reopen fires each time"
./cmd.sh "$D" hide 1 >/dev/null
open build/Spike-gpui.app; sleep 2
show "after second reopen"
grep -c "on_reopen" "$D/log.txt" | sed 's/^/      on_reopen callbacks in total: /'
echo "== stderr tail"; tail -5 "$D/stderr.txt" | sed 's/^/      /'

#!/usr/bin/env bash
# Sends one command to a running gpui spike and prints the log lines it adds.
#   spikes/cmd.sh <cmd-dir> <state|hide|activate|ax|axdump|quit>
set -euo pipefail
dir="$1"; cmd="$2"
before="$(wc -l <"$dir/log.txt" | tr -d ' ')"
printf '%s' "$cmd" >"$dir/cmd.tmp" && mv "$dir/cmd.tmp" "$dir/cmd"
sleep "${3:-1}"
tail -n +"$((before + 1))" "$dir/log.txt"

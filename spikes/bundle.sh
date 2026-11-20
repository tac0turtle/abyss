#!/usr/bin/env bash
# Builds the spike bundles under spikes/build/. Ad-hoc signed throughout.
#
#   spikes/bundle.sh
#
# Bundles:
#   Spike-sandboxed.app   id dev.abyss.spike        sandbox + network.client
#   Spike-sandboxed-nonet.app  id dev.abyss.spike.nonet  sandbox only (negative control for the TCP check)
#   Spike-plain.app       id dev.abyss.spike.plain  no entitlements (control)
#   Spike-gpui.app        id dev.abyss.spike.gpui   no entitlements (GPUI hide/reopen, a11y)
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
here="$PWD"
cargo build -p abyss-spike -p abyss-gpui-spike

make_bundle() { # name id exe binary entitlements-or-empty
    local name="$1" id="$2" exe="$3" binary="$4" ent="${5:-}"
    local app="$here/build/$name.app"
    rm -rf "$app"
    mkdir -p "$app/Contents/MacOS"
    cp "$binary" "$app/Contents/MacOS/$exe"
    sed -e "s/@NAME@/$name/g" -e "s/@ID@/$id/g" -e "s/@EXE@/$exe/g" \
        assets/Info.plist.in >"$app/Contents/Info.plist"
    plutil -lint "$app/Contents/Info.plist" >/dev/null
    if [[ -n "$ent" ]]; then
        codesign --force --sign - --identifier "$id" --entitlements "$ent" "$app"
    else
        codesign --force --sign - --identifier "$id" "$app"
    fi
    codesign --verify --deep --strict "$app"
    echo "bundled $app"
}

make_bundle Spike-sandboxed dev.abyss.spike abyss-spike target/debug/abyss-spike assets/spike.entitlements
make_bundle Spike-sandboxed-nonet dev.abyss.spike.nonet abyss-spike target/debug/abyss-spike assets/spike-nonet.entitlements
make_bundle Spike-plain dev.abyss.spike.plain abyss-spike target/debug/abyss-spike ""
make_bundle Spike-gpui dev.abyss.spike.gpui abyss-gpui-spike target/debug/abyss-gpui-spike ""

#!/usr/bin/env bash
# Builds target/bundle/abyss.app: release binary, icon, Info.plist, ad-hoc signature.
#
#   scripts/bundle.sh
#   ABYSS_BUNDLE_PRECOMPILED_SHADERS=1 scripts/bundle.sh
#
# The default build compiles Metal shaders at runtime and needs no Metal
# toolchain. With ABYSS_BUNDLE_PRECOMPILED_SHADERS=1 the build uses
# --no-default-features, as CI does, and needs Xcode's Metal toolchain.
# The `fake` feature is never enabled here.
#
# ABYSS_SIGNING_IDENTITY can select an installed code-signing certificate.
# A stable certificate and bundle identifier let Keychain trust survive rebuilds.
# Without it, ad-hoc signing only seals resources; its identity changes on rebuild.
# Notarization and the App Sandbox are a later stage.
set -euo pipefail

# Oldest macOS the bundle declares (LSMinimumSystemVersion). The Rust
# toolchain's own floor for Apple targets is 11.0. 13.0 is a conservative
# choice: CI builds on macOS 15 and nothing older has been tried, and GPUI
# renders with Metal. The script fails if the binary's own deployment target
# is newer than this value.
min_macos="13.0"

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

if [[ "$(uname -s)" != "Darwin" ]]; then
    echo "bundle.sh: macOS only" >&2
    exit 1
fi

build_args=(build --release -p abyss)
if [[ "${ABYSS_BUNDLE_PRECOMPILED_SHADERS:-0}" == "1" ]]; then
    build_args+=(--no-default-features)
fi
cargo "${build_args[@]}"

# plutil reads JSON, so no other JSON tool is needed. The package id ends in
# "#abyss@<version>" or "#<version>".
target_dir="$(cargo metadata --format-version 1 --no-deps | plutil -extract target_directory raw -o - -)"
version="$(cargo pkgid -p abyss)"
version="${version##*[#@]}"
if [[ -z "$target_dir" || -z "$version" ]]; then
    echo "bundle.sh: could not read target directory or version from cargo metadata" >&2
    exit 1
fi

binary="$target_dir/release/abyss"
bundle="$target_dir/bundle/abyss.app"
[[ -x "$binary" ]] || { echo "bundle.sh: missing $binary" >&2; exit 1; }

# Refuse to declare an older minimum than the binary was linked for.
binary_min="$(otool -l "$binary" | awk '/LC_BUILD_VERSION/ {f=1} f && $1 == "minos" {print $2; exit}')"
if [[ -n "$binary_min" ]]; then
    newest="$(printf '%s\n%s\n' "$min_macos" "$binary_min" | sort -V | tail -n 1)"
    if [[ "$newest" != "$min_macos" ]]; then
        echo "bundle.sh: binary needs macOS $binary_min, Info.plist declares $min_macos" >&2
        exit 1
    fi
fi

rm -rf "$bundle"
mkdir -p "$bundle/Contents/MacOS" "$bundle/Contents/Resources"

cp "$binary" "$bundle/Contents/MacOS/abyss"
cp assets/icon/abyss.icns "$bundle/Contents/Resources/abyss.icns"
sed -e "s/@VERSION@/$version/g" -e "s/@MIN_MACOS@/$min_macos/g" \
    assets/macos/Info.plist >"$bundle/Contents/Info.plist"
plutil -lint "$bundle/Contents/Info.plist"

signing_identity="${ABYSS_SIGNING_IDENTITY:--}"
codesign --force --identifier dev.abyss --sign "$signing_identity" "$bundle"
codesign --verify --deep --strict "$bundle"

echo "bundled $bundle (version $version, binary minos ${binary_min:-unknown})"

#!/usr/bin/env sh
# Installs libvkslang.so and its implicit layer manifest for the current user
# (or system-wide with PREFIX=/usr and root rights).
set -eu

PREFIX="${PREFIX:-$HOME/.local}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
LIB="$ROOT/target/release/libvkslang.so"
UI="$ROOT/target/release/vkslang-ui"

# Always rebuild: installing a stale target/ binary with an up-to-date manifest
# is very hard to notice afterwards. Set VKSLANG_NO_BUILD=1 to install exactly
# what is in target/release (e.g. binaries downloaded from CI).
if [ "${VKSLANG_NO_BUILD:-0}" = "1" ]; then
    for f in "$LIB" "$UI"; do
        [ -f "$f" ] || { echo "missing $f (VKSLANG_NO_BUILD=1 skips the build)" >&2; exit 1; }
    done
elif command -v cargo > /dev/null; then
    if [ "$(id -u)" = "0" ]; then
        echo "refusing to build as root (it would leave root-owned files in target/)." >&2
        echo "build as your user first, then: sudo VKSLANG_NO_BUILD=1 PREFIX=/usr $0" >&2
        exit 1
    fi
    (cd "$ROOT" && cargo build --release --workspace)
else
    echo "cargo not found: install Rust, or drop libvkslang.so and vkslang-ui into" >&2
    echo "$ROOT/target/release and re-run with VKSLANG_NO_BUILD=1" >&2
    exit 1
fi

install -Dm755 "$LIB" "$PREFIX/lib/vkslang/libvkslang.so"
install -Dm755 "$UI" "$PREFIX/bin/vkslang-ui"
mkdir -p "$PREFIX/share/vulkan/implicit_layer.d"
# The manifest points at the absolute library path so no LD_LIBRARY_PATH is needed.
sed "s|\"library_path\": \"libvkslang.so\"|\"library_path\": \"$PREFIX/lib/vkslang/libvkslang.so\"|" \
    "$ROOT/layer/vkslang.json" > "$PREFIX/share/vulkan/implicit_layer.d/vkslang.json"

# The sample configuration belongs to a user, not to root: skip it for a
# system-wide install so no root-owned file lands in a home directory.
if [ "$(id -u)" != "0" ]; then
    CONF_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/vkSlang"
    if [ ! -f "$CONF_DIR/vkSlang.conf" ]; then
        install -Dm644 "$ROOT/config/vkSlang.conf" "$CONF_DIR/vkSlang.conf"
    fi
fi

echo "Installed. Try: ENABLE_VKSLANG=1 VKSLANG_PRESET=/path/to/preset.slangp vkcube & vkslang-ui"

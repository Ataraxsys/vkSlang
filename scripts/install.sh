#!/usr/bin/env sh
# Installs libvkslang.so and its implicit layer manifest for the current user
# (or system-wide with PREFIX=/usr and root rights).
set -eu

PREFIX="${PREFIX:-$HOME/.local}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
LIB="$ROOT/target/release/libvkslang.so"
UI="$ROOT/target/release/vkslang-ui"

{ [ -f "$LIB" ] && [ -f "$UI" ]; } || (cd "$ROOT" && cargo build --release --workspace)

install -Dm755 "$LIB" "$PREFIX/lib/vkslang/libvkslang.so"
install -Dm755 "$UI" "$PREFIX/bin/vkslang-ui"
mkdir -p "$PREFIX/share/vulkan/implicit_layer.d"
# The manifest points at the absolute library path so no LD_LIBRARY_PATH is needed.
sed "s|\"library_path\": \"libvkslang.so\"|\"library_path\": \"$PREFIX/lib/vkslang/libvkslang.so\"|" \
    "$ROOT/layer/vkslang.json" > "$PREFIX/share/vulkan/implicit_layer.d/vkslang.json"

CONF_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/vkSlang"
if [ ! -f "$CONF_DIR/vkSlang.conf" ]; then
    install -Dm644 "$ROOT/config/vkSlang.conf" "$CONF_DIR/vkSlang.conf"
fi

echo "Installed. Try: ENABLE_VKSLANG=1 VKSLANG_PRESET=/path/to/preset.slangp vkcube & vkslang-ui"

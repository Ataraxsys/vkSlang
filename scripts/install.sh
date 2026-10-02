#!/usr/bin/env sh
# Installs libvkslang.so, its implicit layer manifest and vkslang-ui for the
# current user (or system-wide with PREFIX=/usr and root rights).
#
#   ./scripts/install.sh                      build, then install into ~/.local
#   sudo VKSLANG_NO_BUILD=1 PREFIX=/usr ./scripts/install.sh
#                                             install what is already built
#   ./scripts/install.sh --uninstall          remove what the first form installed
#   sudo PREFIX=/usr ./scripts/install.sh --uninstall
set -eu

PREFIX="${PREFIX:-$HOME/.local}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
LIB="$ROOT/target/release/libvkslang.so"
UI="$ROOT/target/release/vkslang-ui"
MANIFEST="$PREFIX/share/vulkan/implicit_layer.d/vkslang.json"

if [ "${1:-}" = "--uninstall" ]; then
    # Only what this script installs: the configuration and the profiles in
    # ~/.config/vkSlang belong to the user and stay.
    rm -f "$MANIFEST" "$PREFIX/bin/vkslang-ui" "$PREFIX/lib/vkslang/libvkslang.so"
    rmdir "$PREFIX/lib/vkslang" 2>/dev/null || true
    echo "Removed vkSlang from $PREFIX."
    other="$HOME/.local"
    [ "$PREFIX" = "$other" ] && other=/usr
    if [ -f "$other/share/vulkan/implicit_layer.d/vkslang.json" ]; then
        echo "Another copy is still installed under $other: the layer still loads from there."
    fi
    exit 0
elif [ -n "${1:-}" ]; then
    echo "usage: $0 [--uninstall]" >&2
    exit 1
fi

# Always rebuild: installing a stale target/ binary with an up-to-date manifest
# is very hard to notice afterwards. Set VKSLANG_NO_BUILD=1 to install exactly
# what is in target/release (e.g. binaries downloaded from a release).
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
mkdir -p "$(dirname "$MANIFEST")"
# The manifest points at the absolute library path so no LD_LIBRARY_PATH is needed.
sed "s|\"library_path\": \"libvkslang.so\"|\"library_path\": \"$PREFIX/lib/vkslang/libvkslang.so\"|" \
    "$ROOT/layer/vkslang.json" > "$MANIFEST"

# The sample configuration belongs to a user, not to root: skip it for a
# system-wide install so no root-owned file lands in a home directory.
if [ "$(id -u)" != "0" ]; then
    CONF_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/vkSlang"
    if [ ! -f "$CONF_DIR/vkSlang.conf" ]; then
        install -Dm644 "$ROOT/config/vkSlang.conf" "$CONF_DIR/vkSlang.conf"
    fi
fi

echo "Installed into $PREFIX."
# A copy in ~/.local wins over one in /usr: say so, it is easy to forget.
if [ "$PREFIX" != "$HOME/.local" ] && [ -f "$HOME/.local/share/vulkan/implicit_layer.d/vkslang.json" ]; then
    echo "Note: $HOME/.local also has vkSlang, and that copy takes priority."
fi
echo "Try: ENABLE_VKSLANG=1 VKSLANG_PRESET=/path/to/preset.slangp vkcube & vkslang-ui"

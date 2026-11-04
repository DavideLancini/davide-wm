#!/bin/sh
# Builds the release binary; every running davide-wm then replaces itself
# with the new build (live upgrade), keeping all windows open.
set -e
cd "$(dirname "$0")"
cargo build --release
for sock in "$XDG_RUNTIME_DIR"/davide-wm-wayland-*.sock; do
    [ -S "$sock" ] || continue
    DAVIDE_WM_SOCKET="$sock" ./target/release/davide-wm msg upgrade || echo "no answer on $sock"
done

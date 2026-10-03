#!/bin/sh
# Build the example apps and the plugins: WASM component → <dir>/package/backend.wasm,
# and a .pkg archive in target/apps/ (apps) or target/plugins/ (plugins).
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root/apps"
cargo build --release --target wasm32-wasip2
mkdir -p "$root/target/apps"
for pkg in */package; do
    app=${pkg%/package}
    [ -f "$app/Cargo.toml" ] || continue   # Go and Python apps are built below
    cp "target/wasm32-wasip2/release/$app.wasm" "$pkg/backend.wasm"
    files="app.toml backend.wasm"
    [ -d "$pkg/ui" ] && files="$files ui"
    # shellcheck disable=SC2086
    tar -C "$pkg" -cf "$root/target/apps/$app.pkg" $files
    echo "built: apps/$pkg, target/apps/$app.pkg"
done

# Plugins (plugins/): the same packaging, archives in target/plugins/.
(
    cd "$root/plugins"
    cargo build --release --target wasm32-wasip2
    mkdir -p "$root/target/plugins"
    for pkg in */package; do
        plugin=${pkg%/package}
        cp "target/wasm32-wasip2/release/$plugin.wasm" "$pkg/backend.wasm"
        files="app.toml backend.wasm"
        [ -d "$pkg/ui" ] && files="$files ui"
        # shellcheck disable=SC2086
        tar -C "$pkg" -cf "$root/target/plugins/$plugin.pkg" $files
        echo "built: plugins/$pkg, target/plugins/$plugin.pkg"
    done
)

# Go (TinyGo, -target=wasip2): requires tinygo and wasm-tools.
if command -v tinygo >/dev/null && command -v wasm-tools >/dev/null; then
    for mod in "$root"/apps/*/go.mod; do
        dir=$(dirname "$mod")
        app=$(basename "$dir")
        (cd "$dir" && tinygo build -target=wasip2 -o package/backend.wasm .)
        tar -C "$dir/package" -cf "$root/target/apps/$app.pkg" app.toml backend.wasm
        echo "built: apps/$app/package, target/apps/$app.pkg (TinyGo)"
    done
else
    echo "skipped: Go examples (require tinygo and wasm-tools)"
fi


# Python (componentize-py: CPython inside the component).
if command -v componentize-py >/dev/null; then
    for main in "$root"/apps/*/app.py; do
        dir=$(dirname "$main")
        app=$(basename "$dir")
        # The app's own world (world.wit) lists only the imports it uses.
        pkg=$(sed -n 's/^package \([^;@]*\).*/\1/p' "$dir/world.wit")
        world=$(sed -n 's/^world \([a-z0-9-]*\).*/\1/p' "$dir/world.wit")
        componentize-py -d "$root/wit" -d "$dir/world.wit" -w "$pkg/$world" \
            componentize -p "$dir" app -o "$dir/package/backend.wasm"
        tar -C "$dir/package" -cf "$root/target/apps/$app.pkg" app.toml backend.wasm ui
        echo "built: apps/$app/package, target/apps/$app.pkg (Python)"
    done
else
    echo "skipped: Python examples (require componentize-py: pipx install componentize-py)"
fi

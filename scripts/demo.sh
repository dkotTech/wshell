#!/bin/sh
# Demo: shelld + the launcher plugin + Renderer + example apps, then prints the links
# and ways to connect.
# Everything lives in target/demo (data, configs, logs); Ctrl+C stops it all.
#
#   scripts/demo.sh             build, start, install apps (WPE WebKit renderer)
#   scripts/demo.sh --servo     the Renderer on Servo instead of WPE WebKit
#   scripts/demo.sh --no-build  skip building (use what is in target/)
#   scripts/demo.sh --clean     start from empty data (apps, storage, logs)
#
# Ports: SHELL_PORT (8470), DASH_PORT (8471), RENDER_PORT (8090), METRICS_PORT (9470).
# Apps: DEMO_APPS="ping todo-py" (default: every target/apps/*.pkg except stress).
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
demo="$root/target/demo"
run="${XDG_RUNTIME_DIR:-/tmp}/wshell-demo"
shell_port=${SHELL_PORT:-8470}
dash_port=${DASH_PORT:-8471}
render_port=${RENDER_PORT:-8090}
metrics_port=${METRICS_PORT:-9470}
engine=webkit
build=1
clean=0
for arg in "$@"; do
    case "$arg" in
        --servo) engine=servo ;;
        --no-build) build=0 ;;
        --clean) clean=1 ;;
        -h|--help) sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown argument: $arg (see --help)" >&2; exit 2 ;;
    esac
done

say() { printf '\033[1m==> %s\033[0m\n' "$*"; }
die() { printf '\033[31merror:\033[0m %s\n' "$*" >&2; exit 1; }

for port in "$shell_port" "$dash_port" "$render_port" "$metrics_port"; do
    if ss -ltnH "sport = :$port" 2>/dev/null | grep -q .; then
        die "port $port is busy (another shelld/render?); set SHELL_PORT / DASH_PORT / RENDER_PORT / METRICS_PORT"
    fi
done
[ -S "$run/control.sock" ] && die "$run/control.sock exists: is another demo running?"

[ "$clean" = 1 ] && rm -rf "$demo"
mkdir -p "$demo"

# --- Build ---
if [ "$build" = 1 ]; then
    say "building shelld, shellctl"
    cargo build -q --manifest-path "$root/Cargo.toml" -p shelld -p shellctl
    say "building render-rs ($engine)"
    if [ "$engine" = servo ]; then
        cargo build -q --manifest-path "$root/Cargo.toml" -p render-rs --no-default-features --features servo
    else
        cargo build -q --manifest-path "$root/Cargo.toml" -p render-rs
    fi
    say "building example apps (log: target/demo/build-apps.log)"
    if ! "$root/scripts/build-apps.sh" > "$demo/build-apps.log" 2>&1; then
        tail -20 "$demo/build-apps.log"
        die "building apps failed, see $demo/build-apps.log"
    fi
    grep '^skipped:' "$demo/build-apps.log" | sed 's/^/   /' || true
fi
bin="$root/target/debug"
for b in shelld shellctl render-rs; do
    [ -x "$bin/$b" ] || die "$bin/$b is missing: run without --no-build"
done

# --- Configs ---
token=$(od -An -N16 -tx1 /dev/urandom | tr -d ' \n')
cat > "$demo/shell.toml" <<CFG
[shell]
data_dir = "$demo/data"
runtime_dir = "$run"
log_level = "info"

[http]
port = $shell_port

[dashboard]
port = $dash_port

[metrics]
enabled = true
listen = "127.0.0.1:$metrics_port"

# The Renderer gets one-time links to the launcher plugin, and nothing else.
[control.sockets.render]
allow = ["url:org.wshell.launcher"]
CFG
cat > "$demo/render.toml" <<CFG
[api]
listen = "127.0.0.1:$render_port"
token = "$token"

[shell]
launcher = "org.wshell.launcher"
socket = "$run/render.sock"
CFG

# --- Start ---
pids=""
cleanup() {
    trap - EXIT INT TERM
    echo
    say "stopping"
    # shelld stops its apps on SIGTERM; the Renderer has nothing to save.
    for pid in $pids; do kill "$pid" 2>/dev/null || true; done
    for pid in $pids; do wait "$pid" 2>/dev/null || true; done
    exit 0
}
trap cleanup EXIT INT TERM

say "starting shelld (UI :$shell_port, dashboard :$dash_port)"
if command -v systemd-run >/dev/null; then
    # A user scope with a delegated cgroup: CPU/memory limits work as on a device.
    systemd-run --user --scope -q -p Delegate=yes --unit="wshell-demo-$$" \
        "$bin/shelld" -c "$demo/shell.toml" > "$demo/shelld.log" 2>&1 &
else
    "$bin/shelld" -c "$demo/shell.toml" > "$demo/shelld.log" 2>&1 &
fi
pids="$!"
i=0
until [ -S "$run/render.sock" ]; do
    i=$((i + 1)); [ $i -gt 100 ] && die "shelld did not start, see $demo/shelld.log"
    kill -0 "$pids" 2>/dev/null || die "shelld exited, see $demo/shelld.log"
    sleep 0.1
done

ctl() { "$bin/shellctl" --socket "$run/control.sock" "$@"; }

say "installing the launcher plugin and apps"
plugin="$root/target/plugins/launcher.pkg"
[ -f "$plugin" ] || die "$plugin is missing: run without --no-build"
if [ -n "${DEMO_APPS:-}" ]; then
    pkgs=$(for a in $DEMO_APPS; do echo "$root/target/apps/$a.pkg"; done)
else
    pkgs=$(ls "$root"/target/apps/*.pkg 2>/dev/null | grep -v '/stress\.pkg$' || true)
fi
[ -n "$pkgs" ] || die "no packages in target/apps: run without --no-build"
servers=""
for pkg in $plugin $pkgs; do
    [ -f "$pkg" ] || die "$pkg is missing"
    if ! out=$(ctl install --yes --grant-all "$pkg" 2>&1); then
        echo "   $(basename "$pkg"): $(echo "$out" | tail -1)"
        continue
    fi
    echo "   $(echo "$out" | tail -1)"
    # Programs with main (kind = "command") serve their own UI on net.listen ports: start them.
    manifest=$(tar -xOf "$pkg" app.toml)
    if echo "$manifest" | grep -q '^kind = "command"'; then
        id=$(echo "$manifest" | sed -n 's/^id = "\([^"]*\)".*/\1/p' | head -1)
        port=$(echo "$manifest" | sed -n 's/^ports = \[\([0-9]*\).*/\1/p' | head -1)
        if ctl start "$id" > /dev/null 2>&1; then
            servers="$servers $id:$port"
        else
            echo "   $id: failed to start, see: shellctl logs $id"
        fi
    fi
done

say "starting the Renderer ($engine, API 127.0.0.1:$render_port)"
"$bin/render-rs" -c "$demo/render.toml" > "$demo/render.log" 2>&1 &
pids="$! $pids"
i=0
until curl -fs -o /dev/null -H "Authorization: Bearer $token" "http://127.0.0.1:$render_port/api/renders"; do
    i=$((i + 1)); [ $i -gt 100 ] && die "the Renderer did not start, see $demo/render.log"
    sleep 0.1
done

# --- Links ---
link() { ctl "$@" 2>/dev/null | grep -o 'http[^ ]*' || true; }
b() { printf '\033[1m%s\033[0m\n' "$*"; }
api="http://127.0.0.1:$render_port"

echo
b "Device screen (render 1: the launcher plugin, 256x144)"
echo "  viewer:    $api/view/1?token=$token"
echo "  all:       $api/?token=$token"
echo "  terminal:  $bin/render-rs -c $demo/render.toml tui --render 1"
echo "  frame:     curl -H 'Authorization: Bearer $token' $api/api/renders/1/frame -o frame.png"
echo "  MCU raw:   ws://127.0.0.1:$render_port/api/renders/1/ws?format=rgb565&fps=10&token=$token"
echo
b "Terminal (kitty / WezTerm / Ghostty): own render sized to the window"
echo "  $bin/render-rs -c $demo/render.toml tui"
echo "  arrows + Enter: navigate · F12: launcher · Ctrl+Q: quit"
echo
# The TUI is plain terminal output (kitty graphics escapes), so it works over SSH as on a
# device: shelld and the Renderer stay here, only the terminal is remote.
b "Over SSH, as on a device (kitty; not inside tmux/screen)"
echo "  kitten ssh -t -o IdentitiesOnly=yes localhost $bin/render-rs -c $demo/render.toml tui"
if ! ss -ltn 'sport = :22' 2>/dev/null | grep -q LISTEN; then
    echo "  no sshd on :22 here: sudo systemctl start sshd"
fi
echo
b "Browser (one-time links: one use, 5 minutes)"
echo "  dashboard: $(link dashboard)"
ctl list | tail -n +2 | while read -r id _; do
    url=$(link url "$id")
    [ -n "$url" ] && printf '  %-10s %s\n' "${id##*.}:" "$url"
done
if [ -n "$servers" ]; then
    echo
    b "Programs with main: their own HTTP servers (net.listen)"
    for s in $servers; do
        name=${s%%:*}
        printf '  %-14s http://127.0.0.1:%s/\n' "${name##*.}:" "${s##*:}"
    done
fi
echo
b "Metrics (Prometheus text format; scrape it from a Prometheus/agent on the device)"
echo "  curl http://127.0.0.1:$metrics_port/metrics"
echo
b "Management"
echo "  $bin/shellctl --socket $run/control.sock list"
echo "  $bin/shellctl --socket $run/control.sock url <app-id>     # new one-time link"
echo "  $bin/shellctl --socket $run/control.sock logs -f <app-id>"
echo "  logs: $demo/shelld.log, $demo/render.log"
echo
ctl list
echo
say "running; Ctrl+C stops shelld, the Renderer and the apps"
wait

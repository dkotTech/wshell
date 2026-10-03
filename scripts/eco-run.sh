#!/bin/sh
# Run shelld in the energy-efficient mode: a release build and the `strict` power profile
# from the spec (§8), in a user scope with a delegated cgroup, data in target/eco.
# Extra arguments are passed to shelld (e.g. `-c more.toml` on top of this config).
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
cargo build --release --manifest-path "$root/Cargo.toml" -p shelld -p shellctl
mkdir -p "$root/target/eco"
cat > "$root/target/eco/eco.toml" <<CFG
[shell]
data_dir = "$root/target/eco/data"
log_level = "info"
compile_threads = 1               # installs are slower but never load more than one core

[profile]
name = "strict"
cpu_max_percent = 10              # 10% of a core per app (cgroup cpu.max)
memory_mb = 16                    # WASM memory per app; an app may request less, not more
runtime_overhead_mb = 16
pids_max = 8
fuel_per_call = 500_000_000       # a single call is cut off sooner
bridge_events_per_s = 5

[exec]
max_concurrent = 1

[lifecycle]
idle_stop_after_s = 60            # stop apps idle for a minute (idle = "keep" is still honored)
memory_high_mb = 64               # memory budget of all apps: evict by priority above it
CFG
exec systemd-run --user --scope -q -p Delegate=yes --unit="wshell-eco-$$" \
    "$root/target/release/shelld" -c "$root/target/eco/eco.toml" "$@"

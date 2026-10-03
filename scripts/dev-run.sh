#!/bin/sh
# Run shelld for development: a user scope with a delegated cgroup,
# data in target/dev. Extra arguments are passed to shelld.
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
cargo build --manifest-path "$root/Cargo.toml" -p shelld -p shellctl
mkdir -p "$root/target/dev"
cat > "$root/target/dev/dev.toml" <<CFG
[shell]
data_dir = "$root/target/dev/data"
log_level = "info,shelld=debug"
CFG
exec systemd-run --user --scope -q -p Delegate=yes --unit="wshell-dev-$$" \
    "$root/target/debug/shelld" -c "$root/target/dev/dev.toml" "$@"

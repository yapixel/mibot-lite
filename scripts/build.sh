#!/bin/bash
# Build the Rust mibot-lite binary with its version baked in.
set -euo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
out=${1:-$root/mibot-lite}
version=${MIBOT_VERSION:-$(git -C "$root" describe --tags --always --dirty 2>/dev/null || echo dev)}
target=${RUST_TARGET:-}

args=(build --manifest-path "$root/rust/Cargo.toml" --release --locked -p mibot-lite)
if [[ -n "$target" ]]; then
  args+=(--target "$target")
  bin="$root/rust/target/$target/release/mibot-lite"
else
  bin="$root/rust/target/release/mibot-lite"
fi

MIBOT_VERSION=$version cargo "${args[@]}"
mkdir -p "$(dirname "$out")"
cp "$bin" "$out"
chmod 755 "$out"
printf 'built %s (version %s)\n' "$out" "$version"

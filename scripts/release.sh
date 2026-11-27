#!/bin/bash
# Build the release artifacts .update expects: one binary per platform and a
# checksums.txt. `.update run` refuses a release with no checksum file, so
# publishing without this script produces a release nothing will install.
set -euo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
version=${1:-${MIBOT_VERSION:-}}
[[ -n "$version" ]] || { echo "Usage: bash scripts/release.sh <version>" >&2; exit 2; }
out="$root/dist"
rm -rf "$out"
mkdir -p "$out"

targets=(
  "linux-amd64:x86_64-unknown-linux-musl"
  "linux-arm64:aarch64-unknown-linux-musl"
)

host_arch=$(uname -m)
for entry in "${targets[@]}"; do
  name=${entry%%:*}
  rust_target=${entry#*:}
  case "$rust_target" in
    x86_64-unknown-linux-musl)
      if [[ -z "${CC_x86_64_unknown_linux_musl:-}" ]]; then
        if [[ "$host_arch" == "x86_64" ]]; then
          export CC_x86_64_unknown_linux_musl=musl-gcc
        else
          export CC_x86_64_unknown_linux_musl=x86_64-linux-musl-gcc
        fi
      fi
      ;;
    aarch64-unknown-linux-musl)
      if [[ -z "${CC_aarch64_unknown_linux_musl:-}" ]]; then
        if [[ "$host_arch" == "aarch64" || "$host_arch" == "arm64" ]]; then
          export CC_aarch64_unknown_linux_musl=musl-gcc
        else
          export CC_aarch64_unknown_linux_musl=aarch64-linux-musl-gcc
        fi
      fi
      ;;
  esac
  RUST_TARGET=$rust_target MIBOT_VERSION=$version bash "$root/scripts/build.sh" "$out/mibot-lite-$name"
done

cd "$out"
if command -v sha256sum > /dev/null; then
  sha256sum mibot-lite-* > checksums.txt
else
  shasum -a 256 mibot-lite-* > checksums.txt
fi
cat checksums.txt
printf 'Upload every file in %s as assets of release %s\n' "$out" "$version"

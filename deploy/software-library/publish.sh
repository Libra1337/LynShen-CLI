#!/usr/bin/env bash
# Publishes a signed release to the LynShen software library.
#
#   deploy/software-library/publish.sh <product> <version> <files-dir>
#
# Needs SOFTWARE_SIGNING_KEY (path to the key from `software-release keygen`)
# and SOFTWARE_SSH (user@host of the restricted publisher account). The
# server runs receive-software.py, which checks everything again.
set -euo pipefail
product=${1:?product} version=${2:?version} files=${3:?files dir}
: "${SOFTWARE_SIGNING_KEY:?path to the signing key}" "${SOFTWARE_SSH:?user@host}"
root=$(cd "$(dirname "$0")/../.." && pwd)
out=$(mktemp -d)
trap 'rm -rf "$out"' EXIT
cargo run -q --release --manifest-path "$root/Cargo.toml" -p lynshen-software-release -- \
	sign "$SOFTWARE_SIGNING_KEY" "$product" "$version" "$files" "$out"
# COPYFILE_DISABLE: macOS tar would add ._* metadata files the server refuses.
COPYFILE_DISABLE=1 tar -czf - -C "$out" . | ssh "$SOFTWARE_SSH" "publish-software $product"
curl -fsS "https://software.lynshen.org/$product/latest.json" >/dev/null
echo "https://software.lynshen.org/$product/latest.json"

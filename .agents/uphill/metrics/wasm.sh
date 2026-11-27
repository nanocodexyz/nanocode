#!/bin/sh
set -eu
root=$(pwd)
cache="${XDG_CACHE_HOME:-$HOME/.cache}/nanocode-uphill"
work="$cache/wasm/$(printf %s "$root" | cksum | cut -d" " -f1)"
ts_ref="${NANOCODE_TS_REF:-master}"
mkdir -p "$cache" "$work"
if [ ! -d "$cache/nanocode-ts.git" ]; then
  git clone --quiet --bare --filter=blob:none https://github.com/nanocodexyz/nanocode-ts "$cache/nanocode-ts.git" >&2
fi
git -C "$cache/nanocode-ts.git" fetch --quiet origin '+refs/heads/*:refs/heads/*' >&2 || echo 'wasm.sh: fetch failed, using the cached nanocode-ts' >&2
rev=$(git -C "$cache/nanocode-ts.git" rev-parse "$ts_ref")
rm -rf "$work/nanocode-ts"
mkdir -p "$work/nanocode-ts"
git -C "$cache/nanocode-ts.git" archive "$rev" | tar -x -C "$work/nanocode-ts"
rm -f "$work/nanocode-ts/rust-toolchain.toml"
ln -sfn "$root" "$work/nanocodex"
cd "$work/nanocode-ts"
CARGO_TARGET_DIR="$work/target" cargo build --quiet --lib --target wasm32-unknown-unknown --profile wasm >&2
wasm="$work/target/wasm32-unknown-unknown/wasm/nanocode_wasm.wasm"
raw=$(wc -c < "$wasm" | tr -d ' ')
gz=$(gzip -9 -c "$wasm" | wc -c | tr -d ' ')
short=$(echo "$rev" | cut -c1-9)
echo "wasm-size ${raw}B note \"nanocode-ts $short\"" >> "$UPHILL_METRICS"
echo "wasm-gzip ${gz}B" >> "$UPHILL_METRICS"

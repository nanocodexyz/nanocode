#!/usr/bin/env bash
set -u
here=$(cd "$(dirname "$0")" && pwd)
bin=${NANOCODE_SIM:-${CARGO_TARGET_DIR:-$here/../../target}/release/nanocode-sim}
exec "$bin" --memory "$@"

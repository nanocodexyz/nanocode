#!/usr/bin/env bash
set -euo pipefail
root=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
mkdir -p "$root/output"
work=$(mktemp -d "$root/output/memory-mutations.XXXXXX")
git clone --quiet --shared "$root" "$work/core"
cd "$work/core"
export SHIPPY_REMOTE_BUILD=0
export CARGO_TARGET_DIR=${MEMORY_MUTATIONS_TARGET:-$work/target}
check=(--memory --memory-gate --only claude/reload --turns 30 --retained-k 3.2)
cargo run --release --offline --locked -p nanocode-sim -- "${check[@]}" > "$work/baseline.log" 2>&1
printf 'baseline_exit=0\n' > "$work/exits.txt"
python3 - <<'PY'
from pathlib import Path
p = Path('crates/backends/claude/src/agent/history.rs')
s = p.read_text()
old = 'let record = EncodedPayload::from_json(Arc::clone(self.json()?));'
assert s.count(old) == 1
p.write_text(s.replace(old, 'let record = EncodedPayload::from_json(Arc::from(&**self.json()?));'))
PY
set +e
cargo run --release --offline --locked -p nanocode-sim -- "${check[@]}" > "$work/mutant.log" 2>&1
status=$?
printf 'mutant_exit=%s\n' "$status" >> "$work/exits.txt"
set -e
if [ "$status" -eq 0 ]; then
  printf 'FAIL: memory mutation escaped: %s\n' "$work"
  exit 1
fi
if ! grep -q 'MEMORY RETAINED' "$work/mutant.log"; then
  printf 'FAIL: mutation did not reach the memory invariant: %s\n' "$work"
  exit 1
fi
printf 'PASS: duplicated history record caught; evidence: %s\n' "$work"

#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat >&2 <<'EOF'
usage: scripts/pin-rebuild.sh [--rev <nanocode sha>] [--build-only] [--python-too]

Pins nanocode-ts master's sdk-revision.json to a nanocode revision (default: origin/master HEAD),
builds and type-checks the package, pushes the pin to nanocode-ts master and publishes the
build-<short> release with nanocode-ts's own scripts/release.mjs. The last line printed is:

  build-<short> <nanocode sha> <wasm fingerprint> <tarball URL>

  --rev SHA      pin this nanocode revision instead of origin/master
  --build-only   build and check only: no commit, push or release
  --python-too   also pin python-revision.json (default keeps nanocode-ts master's python pin)

Environment: PIN_DIR (clones, default ~/.cache/nanocode-pin), PIN_LOG (log file),
PIN_LLVM (LLVM bin dir with clang 22, default /usr/lib/llvm-22/bin).
Stock defaults only: nothing here sets an eval or product flag, and nothing deploys.
EOF
  exit 2
}

REV= BUILD_ONLY=0 PYTHON_TOO=0
while [ $# -gt 0 ]; do
  case "$1" in
    --rev) REV=$2; shift ;;
    --build-only) BUILD_ONLY=1 ;;
    --python-too) PYTHON_TOO=1 ;;
    *) usage ;;
  esac
  shift
done

W=${PIN_DIR:-$HOME/.cache/nanocode-pin}
NC=$W/nanocode
TS=$W/nanocode-ts
mkdir -p "$W"
LOG=${PIN_LOG:-$W/pin-rebuild-$(date -u +%Y%m%dT%H%M%SZ).log}
LLVM=${PIN_LLVM:-/usr/lib/llvm-22/bin}
export NANOCODE_CC=$LLVM/clang PATH=$LLVM:$PATH

say() { echo "[pin $(date -u +%H:%M:%S)] $*" | tee -a "$LOG" >&2; }
step() { say "$*"; "$@" >>"$LOG" 2>&1 || { say "FAILED: $* (see $LOG)"; tail -40 "$LOG" >&2; exit 1; }; }

[ -x "$NANOCODE_CC" ] || { say "missing $NANOCODE_CC: python-capi needs clang 22 (apt.llvm.org clang-22 lld-22 llvm-22)"; exit 1; }
[ -d "$NC/.git" ] || step git clone -q https://github.com/nanocodexyz/nanocode.git "$NC"
[ -d "$TS/.git" ] || step git clone -q https://github.com/nanocodexyz/nanocode-ts.git "$TS"

step git -C "$NC" fetch -q origin master
[ -n "$REV" ] || REV=$(git -C "$NC" rev-parse origin/master)
REV=$(git -C "$NC" rev-parse "$REV^{commit}")
step git -C "$NC" checkout -q --detach "$REV"
say "nanocode $REV ($(git -C "$NC" log -1 --format=%s "$REV"))"

step git -C "$TS" fetch -q origin master
if [ -n "$(git -C "$TS" status --porcelain --untracked-files=no)" ]; then
  say "$TS has uncommitted changes; refusing to touch them"; exit 1
fi
if git -C "$TS" rev-parse -q --verify master >/dev/null && [ -n "$(git -C "$TS" log --oneline origin/master..master)" ]; then
  say "local nanocode-ts master has unpushed commits; push or drop them first"; exit 1
fi
step git -C "$TS" checkout -q -B master origin/master

pin() { python3 - "$1" "$REV" <<'PY'
import json, sys
path, rev = sys.argv[1], sys.argv[2]
data = json.load(open(path))
data["revision"] = rev
data["branch"] = "master"
open(path, "w").write(json.dumps(data, indent=2) + "\n")
PY
}
rev_of() { python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["revision"])' "$TS/$1"; }

FILES=(sdk-revision.json)
[ "$PYTHON_TOO" = 1 ] && FILES+=(python-revision.json)
CHANGED=0
for f in "${FILES[@]}"; do
  if [ "$(rev_of "$f")" != "$REV" ]; then pin "$TS/$f"; CHANGED=1; fi
done
say "pins: sdk $(rev_of sdk-revision.json) python $(rev_of python-revision.json)"

cd "$TS"
step npm ci --ignore-scripts
step node scripts/build.mjs
step npm run prepare
step npm run check:types
PRINT=$(node scripts/wasm.mjs)
say "wasm fingerprint $PRINT"

if [ "$BUILD_ONLY" = 1 ]; then
  git checkout -q -- "${FILES[@]}"
  say "BUILD-ONLY OK: nanocode $REV on nanocode-ts $(git rev-parse --short HEAD), wasm $PRINT"
  exit 0
fi

if [ "$CHANGED" = 1 ]; then
  step git add "${FILES[@]}"
  if [ "$PYTHON_TOO" = 1 ]; then MSG="chore(wasm): pin nanocode ${REV:0:9} for the sdk and python"; else MSG="chore(wasm): pin nanocode ${REV:0:9}"; fi
  step git commit -q -m "$MSG"
  if ! git push -q origin HEAD:master >>"$LOG" 2>&1; then
    say "push rejected: nanocode-ts master moved during the build; rerun (it rebuilds on the new master)"
    git reset -q --keep origin/master
    exit 1
  fi
  say "pushed $(git rev-parse --short HEAD) $MSG"
fi

SHORT=$(git rev-parse --short=7 HEAD)
if gh release view "build-$SHORT" --repo nanocodexyz/nanocode-ts >/dev/null 2>&1; then
  say "release build-$SHORT already exists"
else
  step node scripts/release.mjs
fi
URL="https://github.com/nanocodexyz/nanocode-ts/releases/download/build-$SHORT/nanocode-ts-$SHORT.tgz"
say "RELEASE build-$SHORT: nanocode-ts $(git rev-parse HEAD) against nanocode $REV, wasm $PRINT"
echo "build-$SHORT $REV $PRINT $URL"

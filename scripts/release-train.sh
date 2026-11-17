#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat >&2 <<'EOF'
usage: scripts/release-train.sh <command> [args]

The release train of docs/RELEASE-CHAIN.md: one core build moved through every surface in order.

  plan                          print the chain with each surface's current pin
  build [--rev SHA]             scripts/pin-rebuild.sh: pin, build and release nanocode-ts build-<short>
  bump <surface> <short>        point a surface's pin at nanocode-ts build-<short> and commit (no push)
  deploy <surface>              build, check and deploy a bumped surface the way its repository does, then push
  verify <surface> <short> [url]
                                deployed wasm sha256 == build-<short> tarball, and the surface's overrides
  sweep <prev-short> <new-short> [targets]
                                nanoeval systems run --suite systems-v1 on every target, build vs build
  train [--rev SHA] [--cli] [--release-worker]
                                build, then bump, deploy and verify site, cf-demo and vercel-demo in order,
                                optionally the CLI rolling release and the nanoeval release Worker, then sweep

Surfaces: cf-demo, vercel-demo, site, release-worker (the CLI ships with scripts/release-cli.sh;
gateway and swarms are deployed by their owners, see the hand-off in docs/RELEASE-CHAIN.md).
Environment: TRAIN_DIR (surface checkouts, default ~/.cache/nanocode-train), WRANGLER (default 'npx wrangler'),
VERCEL_SCOPE (default tempoxyz), NANOEVAL_SUITES (systems suite checkout), NANOEVAL_RUNS_IDLE=1 (required to
deploy the release Worker: confirms no measurement run is using it). Deploys need Cloudflare and Vercel logins
on the machine that runs them.
EOF
  exit 2
}

ROOT=$(cd "$(dirname "$0")/.." && pwd)
D=${TRAIN_DIR:-$HOME/.cache/nanocode-train}
WRANGLER=${WRANGLER:-npx wrangler}
SCOPE=${VERCEL_SCOPE:-tempoxyz}
TS_REPO=nanocodexyz/nanocode-ts
mkdir -p "$D"
say() { echo "[train $(date -u +%H:%M:%S)] $*" >&2; }
die() { say "$*"; exit 1; }

repo_of() {
  case $1 in
    cf-demo) echo nanocodexyz/durable-cloudflare ;;
    vercel-demo) echo nanocodexyz/durable-vercel ;;
    site) echo nanocodexyz/nanocode-site ;;
    release-worker) echo nanocodexyz/nanoeval ;;
    *) die "unknown surface $1" ;;
  esac
}
branch_of() { case $1 in release-worker) echo main ;; *) echo master ;; esac; }
url_of() {
  case $1 in
    cf-demo) echo https://cloudflare.nanocode.xyz ;;
    vercel-demo) echo https://nanocode-vercel-durable.vercel.app ;;
    site) echo https://nanocode.xyz ;;
    release-worker) echo https://nanoeval-release.tempo-dev.workers.dev ;;
  esac
}
checkout() {
  local dir=$D/$1 repo branch
  repo=$(repo_of "$1"); branch=$(branch_of "$1")
  [ -d "$dir/.git" ] || gh repo clone "$repo" "$dir" -- -q >&2
  [ -z "$(git -C "$dir" status --porcelain --untracked-files=no)" ] || die "$dir has uncommitted changes"
  git -C "$dir" fetch -q origin "$branch"
  git -C "$dir" checkout -q -B "$branch" "origin/$branch"
  echo "$dir"
}
tarball() { echo "https://github.com/$TS_REPO/releases/download/build-$1/nanocode-ts-$1.tgz"; }
sha() { sha256sum "$1" | cut -d' ' -f1; }
tarball_wasm_sha() {
  local t=$D/tarballs/$1
  if [ ! -f "$t/package/dist/nanocode_wasm.wasm" ]; then
    mkdir -p "$t"
    curl -fsSL "$(tarball "$1")" -o "$t/pkg.tgz"
    tar -xzf "$t/pkg.tgz" -C "$t"
  fi
  sha "$t/package/dist/nanocode_wasm.wasm"
}
full_commit() { gh api "repos/$TS_REPO/commits/$1" -q .sha; }
latest_build() { gh release list --repo "$TS_REPO" --limit 20 --json tagName -q '[.[]|select(.tagName|startswith("build-"))][0].tagName' | sed 's/^build-//'; }
pin_of() {
  gh api "repos/$(repo_of "$1")/contents/package.json?ref=$(branch_of "$1")" -q .content | base64 -d |
    python3 -c 'import json,sys; d=json.load(sys.stdin); print({**d.get("dependencies",{}),**d.get("devDependencies",{})}.get("nanocode-ts"))'
}

plan() {
  local n
  echo "1 core           nanocode master $(gh api repos/nanocodexyz/nanocode/commits/master -q .sha | cut -c1-9)"
  echo "2 build          nanocode-ts latest build-$(latest_build)"
  echo "3 cli            release nanocode-cli built from $(gh release view nanocode-cli --repo nanocodexyz/nanocode --json targetCommitish -q .targetCommitish | cut -c1-9)"
  echo "4 site           wasm-revision $(gh api "repos/$(repo_of site)/contents/wasm-revision.json" -q .content | base64 -d | python3 -c 'import json,sys; print(json.load(sys.stdin)["revision"][:9])') $(url_of site)"
  n=5; for s in cf-demo vercel-demo release-worker; do printf "%s %-14s %s %s\n" "$n" "$s" "$(pin_of "$s")" "$(url_of "$s")"; n=$((n + 1)); done
  echo "8 gateway        owners deploy (its DEPLOY.md): same build-<short> tarball, staging, then production"
  echo "9 swarms         owners deploy after the gateway: same tarball, npm run deploy"
}

bump() {
  local s=$1 b=$2 dir
  dir=$(checkout "$s")
  case $s in
    cf-demo | vercel-demo | release-worker)
      (cd "$dir" && npm install --no-audit --no-fund "$(tarball "$b")" >&2)
      git -C "$dir" add package.json package-lock.json
      ;;
    site)
      python3 - "$dir/wasm-revision.json" "$(full_commit "$b")" <<'PY'
import json, sys
path, rev = sys.argv[1], sys.argv[2]
data = json.load(open(path))
data["revision"] = rev
open(path, "w").write(json.dumps(data, indent=2) + "\n")
PY
      git -C "$dir" add wasm-revision.json
      ;;
  esac
  if git -C "$dir" diff --cached --quiet; then say "$s already pins build-$b"; return; fi
  git -C "$dir" commit -q -m "chore(deps): nanocode-ts build-$b"
  say "$s bumped to build-$b in $dir (commit $(git -C "$dir" rev-parse --short HEAD), not pushed)"
}

deploy() {
  local s=$1 dir=$D/$1 ne short
  [ -d "$dir/.git" ] || die "run bump $s first"
  cd "$dir"
  case $s in
    cf-demo)
      npm run build >&2
      npm run dry-run >&2
      $WRANGLER deployments list --name nanocode-cloudflare-durable 2>/dev/null | tail -12 > "$D/$s.before" || true
      npm run deploy:demo >&2
      ;;
    vercel-demo)
      npm run check >&2
      npx vercel@latest ls nanocode-vercel-durable --prod --scope "$SCOPE" 2>/dev/null | head -5 > "$D/$s.before" || true
      npx vercel@latest deploy --prod --scope "$SCOPE" --yes >&2
      ;;
    site)
      pnpm install --frozen-lockfile >&2
      pnpm check:types >&2
      pnpm build >&2
      $WRANGLER deployments list --name nanocode-site 2>/dev/null | tail -12 > "$D/$s.before" || true
      $WRANGLER deploy >&2
      ;;
    release-worker)
      [ "${NANOEVAL_RUNS_IDLE:-}" = 1 ] || die "set NANOEVAL_RUNS_IDLE=1 once no measurement run is using the release Worker"
      ne=$(git rev-parse --short=7 HEAD)
      short=$(node -e 'const v=require("./package.json").dependencies["nanocode-ts"]; console.log(v.match(/build-([0-9a-f]+)/)[1])')
      $WRANGLER deployments list --name nanoeval-release 2>/dev/null | tail -12 > "$D/$s.before" || true
      $WRANGLER deploy --dry-run --outdir output/deploy >&2
      $WRANGLER deploy --name nanoeval-release --var "COMMIT:pin-$ne-ncts-$short" >&2
      ;;
    *) die "deploy: unknown surface $s" ;;
  esac
  git push -q origin "HEAD:$(branch_of "$s")"
  say "$s deployed and pushed; previous deployments saved in $D/$s.before for rollback"
}

verify() {
  local s=$1 b=$2 url=${3:-} want got dir=$D/$1 f
  want=$(tarball_wasm_sha "$b")
  case $s in
    vercel-demo)
      url=${url:-$(url_of "$s")}
      curl -fsSL "$url/nanocode_wasm.wasm" -o "$D/$s.wasm"
      got=$(sha "$D/$s.wasm")
      ;;
    cf-demo | release-worker)
      f=$(find "$dir/output/deploy" -name '*nanocode_wasm*.wasm' | head -1)
      [ -n "$f" ] || die "no bundled nanocode_wasm.wasm under $dir/output/deploy: run deploy first"
      got=$(sha "$f")
      ;;
    site)
      f=$dir/.cache/wasm/nanocode-wasm/dist/nanocode_wasm.wasm
      [ -f "$f" ] || die "no site wasm build at $f: run deploy first"
      got=$(sha "$f")
      ;;
    *) die "verify: unknown surface $s" ;;
  esac
  if [ "$got" = "$want" ]; then say "$s wasm sha256 $got == build-$b tarball: PASS"; else say "$s wasm sha256 $got != build-$b tarball $want: FAIL"; return 1; fi
  case $s in
    cf-demo) grep -o -- '--var [A-Z_]*:[^ ]*' "$dir/package.json" | sed 's/^/  deploy override: /' >&2 || true ;;
  esac
}

sweep() {
  local prev=$1 new=$2 targets=${3:-cli-local,release-worker,cf-demo,vercel-demo,site} ne=$D/nanoeval-sweep
  [ -d "$ne/.git" ] || gh repo clone nanocodexyz/nanoeval "$ne" -- -q >&2
  git -C "$ne" fetch -q origin main
  git -C "$ne" checkout -q -B main origin/main
  (cd "$ne" && npm ci --no-audit --no-fund >&2 && node cli/nanoeval.ts systems run --suite systems-v1 --targets "$targets" --build "prev=build-$prev" --build "new=build-$new")
}

train() {
  local rev= cli=0 rw=0 line b prev
  while [ $# -gt 0 ]; do
    case $1 in --rev) rev=$2; shift ;; --cli) cli=1 ;; --release-worker) rw=1 ;; *) usage ;; esac
    shift
  done
  prev=$(latest_build)
  line=$("$ROOT/scripts/pin-rebuild.sh" ${rev:+--rev "$rev"} | tail -1)
  b=$(echo "$line" | cut -d' ' -f1 | sed 's/^build-//')
  say "core build $line"
  if [ "$cli" = 1 ]; then
    (cd "$ROOT" && git fetch -q origin master && git checkout -q --detach "$(echo "$line" | cut -d' ' -f2)" &&
      scripts/release-cli.sh build && scripts/release-cli.sh pack && scripts/release-cli.sh upload && scripts/release-cli.sh publish)
  fi
  for s in site cf-demo vercel-demo; do bump "$s" "$b"; deploy "$s"; verify "$s" "$b"; done
  if [ "$rw" = 1 ]; then bump release-worker "$b"; deploy release-worker; verify release-worker "$b"; fi
  say "gateway and swarms: hand build-$b to their owners (docs/RELEASE-CHAIN.md, Hand-off)"
  sweep "$prev" "$b"
}

[ $# -gt 0 ] || usage
cmd=$1
shift
case $cmd in
  plan) plan ;;
  build) "$ROOT/scripts/pin-rebuild.sh" "$@" ;;
  bump) [ $# = 2 ] || usage; bump "$@" ;;
  deploy) [ $# = 1 ] || usage; deploy "$@" ;;
  verify) [ $# -ge 2 ] || usage; verify "$@" ;;
  sweep) [ $# -ge 2 ] || usage; sweep "$@" ;;
  train) train "$@" ;;
  *) usage ;;
esac

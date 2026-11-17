#!/bin/sh
set -eu
root="$(cd "$(dirname "$0")/.." && pwd)"
out="${NANOCODE_RELEASE_OUT:-$root/target/release-cli}"
cache="$root/target/release-cache"
repo=nanocodexyz/nanocode
url="${NANOCODE_RELEASE:-https://github.com/$repo/releases/download/nanocode-cli}"
all="darwin-aarch64 darwin-x86_64 linux-arm64 linux-amd64 windows-x86_64 windows-aarch64"
cd "$root"
[ "${1:-}" != inside ] || git config --global --add safe.directory "$root"
short="$(git rev-parse --short=12 HEAD)"
digest() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi
}
key() {
  [ -z "${NANOCODE_NO_CACHE:-}" ] || return 0
  python3 scripts/input-key.py "$@" 2>/dev/null || true
}
remote() {
  file="$out/remote/$1"
  if [ ! -f "$file" ]; then
    mkdir -p "$out/remote"
    curl -fsSL "$url/$1" -o "$file" 2>/dev/null || : > "$file"
  fi
  printf '%s\n' "$file"
}
save() {
  [ -n "$1" ] || return 0
  mkdir -p "$cache"
  cp "$2" "$cache/$1"
}
restore() {
  [ -n "$1" ] || return 1
  if [ -f "$cache/$1" ]; then
    cp "$cache/$1" "$2"
    echo "release-cli.sh: reused the local build of $(basename "$2")" >&2
    return 0
  fi
  line="$(awk -v k="$1" '$1 == k { print $2, $3; exit }' "$(remote INPUTS)")"
  [ -n "$line" ] || return 1
  asset="${line%% *}"
  member="${line#* }"
  expected="$(awk -v a="$asset" '$2 == a { print $1; exit }' "$(remote SHA256SUMS)")"
  [ -n "$expected" ] || return 1
  fetched="$out/remote/$asset"
  [ -f "$fetched" ] || curl -fsSL "$url/$asset" -o "$fetched" || return 1
  [ "$(digest "$fetched")" = "$expected" ] || return 1
  case $asset in
    *.xz) xz -dc "$fetched" > "$2" || return 1 ;;
    *.gz) gunzip -c "$fetched" > "$2" || return 1 ;;
    *.zip) unzip -p "$fetched" "$member" > "$2" || return 1 ;;
  esac
  chmod +x "$2"
  save "$1" "$2"
  echo "release-cli.sh: reused $asset for $(basename "$2")" >&2
}
cli() {
  triple=$1
  exe=$2
  work="$out/work/$triple"
  rm -rf "$work"
  mkdir -p "$work"
  python="$work/nanocode-python$exe"
  pkey="$(key nanocode-python-cli "$triple")"
  if ! restore "$pkey" "$python"; then
    built="$(NANOCODE_BOLT= scripts/build-cli.sh nanocode-python-cli nanocode-python "$triple")"
    cp "$built" "$python"
    save "$pkey" "$python"
  fi
  bolt="${NANOCODE_BOLT:-}"
  nkey=
  [ "$bolt" = 1 ] || nkey="$(key nanocode-cli "$triple" "python=$(digest "$python")")"
  if ! restore "$nkey" "$work/nanocode$exe"; then
    built="$(NANOCODE_PYTHON="$python" scripts/build-cli.sh nanocode-cli nanocode "$triple")"
    cp "$built" "$work/nanocode$exe"
    save "$nkey" "$work/nanocode$exe"
  fi
}
lane() {
  name=$1
  dest="$out/lanes/$name"
  case $name in linux-*) ;; *) rm -rf "$dest" && mkdir -p "$dest" ;; esac
  case $name in
    darwin-aarch64 | darwin-x86_64)
      triple="${name#darwin-}-apple-darwin"
      export CARGO_TARGET_DIR="$root/target/lane-$name"
      cli "$triple" ""
      cp "$work/nanocode" "$dest/nanocode-$triple"
      hash="$(digest "$python")"
      cp "$python" "$dest/nanocode-python-$hash"
      ckey="$(key nanocode-cua-driver "$triple")"
      if ! restore "$ckey" "$dest/nanocode-cua-driver-$triple"; then
        built="$(scripts/build-cli.sh nanocode-cua-driver nanocode-cua-driver "$triple")"
        cp "$built" "$dest/nanocode-cua-driver-$triple"
        save "$ckey" "$dest/nanocode-cua-driver-$triple"
      fi
      printf '%s %s\n' "$pkey" "nanocode-python-$hash" "$nkey" "nanocode-$triple" "$ckey" "nanocode-cua-driver-$triple" > "$dest/.keys"
      ;;
    linux-arm64 | linux-amd64)
      arch="${name#linux-}"
      host="$arch"
      case $(uname -m) in x86_64 | amd64) host=amd64 ;; esac
      image="nanocode-cli-builder:$host"
      docker build -q --platform "linux/$host" -t "$image" -f scripts/cli-builder.Dockerfile scripts >&2
      docker run --rm --platform "linux/$host" -v "$root:/src" -v "nanocode-cargo-$host:/usr/local/cargo/registry" -w /src \
        -e CARGO_TARGET_DIR="/src/target/linux-$arch" -e RUSTC_WRAPPER= -e RUSTC_WORKSPACE_WRAPPER= -e CARGO_INCREMENTAL=0 \
        -e NANOCODE_BOLT="${NANOCODE_BOLT:-}" -e NANOCODE_NO_CACHE="${NANOCODE_NO_CACHE:-}" -e NANOCODE_RELEASE="$url" \
        "$image" scripts/release-cli.sh inside "$name" >&2
      [ "$out" = "$root/target/release-cli" ] || cp -R "$root/target/release-cli/lanes/$name/." "$dest/"
      ;;
    windows-x86_64 | windows-aarch64)
      triple="${name#windows-}-pc-windows-msvc"
      export CARGO_TARGET_DIR="$root/target/lane-$name"
      cli "$triple" .exe
      stage="windows/nanocode-$triple"
      mkdir -p "$dest/$stage"
      cp "$work/nanocode.exe" "$python" "$dest/$stage/"
      printf '%s %s\n' "$pkey" "$stage/nanocode-python.exe" "$nkey" "$stage/nanocode.exe" > "$dest/.keys"
      ;;
    *)
      echo "release-cli.sh: unknown lane $name; lanes are $all" >&2
      exit 1
      ;;
  esac
}
inside() {
  name=$1
  dest="$out/lanes/$name"
  rm -rf "$dest"
  mkdir -p "$dest"
  case $name in linux-arm64) triple=aarch64-unknown-linux-gnu ;; linux-amd64) triple=x86_64-unknown-linux-gnu ;; esac
  cli "$triple" ""
  hash="$(digest "$python")"
  cp "$python" "$dest/nanocode-python-$hash"
  built="$CARGO_TARGET_DIR/cli/$triple"
  if [ "${NANOCODE_BOLT:-}" = 1 ] && [ -f "$built/nanocode-agent" ]; then
    cp "$built/nanocode-agent" "$dest/nanocode-$triple"
    cp "$built/nanocode-exec" "$dest/nanocode-exec-$triple"
    cp "$built/nanocode" "$dest/nanocode-front-$triple"
  else
    cp "$work/nanocode" "$dest/nanocode-$triple"
  fi
  printf '%s %s\n' "$pkey" "nanocode-python-$hash" "$nkey" "nanocode-$triple" > "$dest/.keys"
  owner="$(stat -c %u:%g "$root")"
  chown -R "$owner" "$out" "$cache" "$CARGO_TARGET_DIR" 2>/dev/null || true
}
build() {
  [ $# -gt 0 ] || set -- $all
  mkdir -p "$out/logs"
  env -u CARGO_NET_OFFLINE cargo fetch --locked >&2
  pids=
  for name in "$@"; do
    (
      start=$(date +%s)
      if "$0" lane "$name" > "$out/logs/$name.log" 2>&1; then
        echo "release-cli.sh: $name built in $(( $(date +%s) - start ))s" >&2
      else
        echo "release-cli.sh: $name failed after $(( $(date +%s) - start ))s; last lines of $out/logs/$name.log:" >&2
        tail -n 20 "$out/logs/$name.log" >&2
        exit 1
      fi
    ) &
    pids="$pids $!"
  done
  status=0
  for pid in $pids; do wait "$pid" || status=1; done
  return $status
}
asset() {
  case $1 in
    nanocode-python-*) echo "$1.xz" ;;
    windows/*) stage="${1#windows/}"; echo "${stage%%/*}.$short.zip ${1##*/}" ;;
    nanocode-cua-driver-* | nanocode-front-*) echo "$1.$short.gz" ;;
    *) echo "$1.$short.xz" ;;
  esac
}
pack_lane() {
  name=$1
  src="$out/lanes/$name"
  dist="$out/dist/$name"
  [ -d "$src" ] || { echo "release-cli.sh: lane $name isn't built in $out/lanes" >&2; exit 1; }
  rm -rf "$dist"
  mkdir -p "$dist"
  pids=
  for file in "$src"/nanocode-*; do
    [ -f "$file" ] || continue
    base="${file##*/}"
    case $base in
      nanocode-python-*) xz -9e -c "$file" > "$dist/$base.xz" & pids="$pids $!" ;;
      nanocode-cua-driver-* | nanocode-front-*) gzip -9 -c "$file" > "$dist/$base.$short.gz" & pids="$pids $!" ;;
      *)
        gzip -9 -c "$file" > "$dist/$base.$short.gz" & pids="$pids $!"
        xz -9e -c "$file" > "$dist/$base.$short.xz" & pids="$pids $!"
        ;;
    esac
  done
  for stage in "$src"/windows/nanocode-*; do
    [ -d "$stage" ] || continue
    (cd "$stage" && if command -v zip >/dev/null 2>&1; then zip -9 -q -X "$dist/${stage##*/}.$short.zip" nanocode.exe nanocode-python.exe; else python3 -m zipfile -c "$dist/${stage##*/}.$short.zip" nanocode.exe nanocode-python.exe; fi) &
    pids="$pids $!"
  done
  status=0
  for pid in $pids; do wait "$pid" || status=1; done
  [ $status = 0 ] || { echo "release-cli.sh: compressing $name failed" >&2; exit 1; }
  (cd "$dist" && for file in ./*.gz ./*.xz ./*.zip; do [ -f "$file" ] && printf '%s  %s\n' "$(digest "$file")" "${file#./}"; done; true) > "$out/dist/parts-$short-$name.sums"
  if [ -f "$src/.keys" ]; then
    while read -r input file; do
      [ -n "$input" ] && [ -n "$file" ] && printf '%s %s\n' "$input" "$(asset "$file")"
    done < "$src/.keys" > "$out/dist/parts-$short-$name.inputs"
  else
    : > "$out/dist/parts-$short-$name.inputs"
  fi
}
pack() {
  [ $# -gt 0 ] || set -- $all
  pids=
  for name in "$@"; do pack_lane "$name" & pids="$pids $!"; done
  status=0
  for pid in $pids; do wait "$pid" || status=1; done
  return $status
}
upload() {
  [ $# -gt 0 ] || set -- $all
  for name in "$@"; do
    gh release upload nanocode-cli --repo "$repo" --clobber "$out/dist/$name"/*
    gh release upload nanocode-cli --repo "$repo" --clobber "$out/dist/parts-$short-$name.sums" "$out/dist/parts-$short-$name.inputs"
    echo "release-cli.sh: uploaded $name for $short" >&2
  done
}
publish() {
  staged="$out/publish"
  rm -rf "$staged"
  mkdir -p "$staged"
  for name in $all; do
    for part in sums inputs; do
      gh release download nanocode-cli --repo "$repo" --pattern "parts-$short-$name.$part" --dir "$staged" 2>/dev/null \
        || { echo "release-cli.sh: $name isn't uploaded for $short yet; run release-cli.sh ship $name on its host" >&2; exit 1; }
    done
  done
  cat "$staged"/parts-*.sums > "$staged/SHA256SUMS"
  cat "$staged"/parts-*.inputs > "$staged/INPUTS"
  gh release view nanocode-cli --repo "$repo" --json assets -q '.assets[].name' > "$staged/assets"
  missing="$(cut -d' ' -f3 "$staged/SHA256SUMS" | grep -vxF -f "$staged/assets" || true)"
  [ -z "$missing" ] || { echo "release-cli.sh: the release lacks $missing" >&2; exit 1; }
  gh release download nanocode-cli --repo "$repo" --pattern SHA256SUMS --output "$staged/previous" 2>/dev/null || : > "$staged/previous"
  git push -f origin "$(git rev-parse HEAD):refs/tags/nanocode-cli"
  cp install "$staged/install.sh"
  cp install.ps1 "$staged/install.ps1"
  gh release upload nanocode-cli --repo "$repo" --clobber "$staged/SHA256SUMS" "$staged/INPUTS" "$staged/install.sh" "$staged/install.ps1"
  keep="$staged/keep"
  { cut -d' ' -f3 "$staged/SHA256SUMS"; cut -d' ' -f3 "$staged/previous"; printf '%s\n' SHA256SUMS INPUTS install.sh install.ps1; } > "$keep"
  grep -vxF -f "$keep" "$staged/assets" | grep -v '^nanocode-python-' | while read -r stale; do
    gh release delete-asset nanocode-cli "$stale" --repo "$repo" -y >/dev/null
  done
  echo "release-cli.sh: published $short" >&2
}
command="${1:-all}"
[ $# -gt 0 ] && shift
case $command in
  lane) lane "$1" ;;
  inside) inside "$1" ;;
  build) build "$@" ;;
  pack) pack "$@" ;;
  upload) upload "$@" ;;
  ship) build "$@" && pack "$@" && upload "$@" ;;
  publish) publish ;;
  all) build && pack ;;
  --publish) build && pack && upload && publish ;;
  *) echo "usage: release-cli.sh [build|pack|upload|ship] [lane...] | publish | --publish; lanes: $all" >&2; exit 1 ;;
esac

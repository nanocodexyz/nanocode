#!/bin/sh
set -eu
root="$(cd "$(dirname "$0")/.." && pwd)"
out="${NANOCODE_RELEASE_OUT:-$root/target/release-cli}"
all="darwin-aarch64 darwin-x86_64 linux-arm64 linux-amd64 windows-x86_64 windows-aarch64"
cd "$root"
digest() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi
}
lane() {
  name=$1
  dest="$out/lanes/$name"
  rm -rf "$dest"
  mkdir -p "$dest"
  case $name in
    darwin-aarch64 | darwin-x86_64)
      triple="${name#darwin-}-apple-darwin"
      export CARGO_TARGET_DIR="$root/target/lane-$name"
      built="$(scripts/build-cli.sh nanocode-cli nanocode "$triple")"
      cp "$built" "$dest/nanocode-$triple"
      python="$(dirname "$built")/nanocode-python"
      cp "$python" "$dest/nanocode-python-$(digest "$python")"
      cp "$(scripts/build-cli.sh nanocode-cua-driver nanocode-cua-driver "$triple")" "$dest/nanocode-cua-driver-$triple"
      ;;
    linux-arm64 | linux-amd64)
      arch="${name#linux-}"
      case $arch in arm64) triple=aarch64-unknown-linux-gnu ;; amd64) triple=x86_64-unknown-linux-gnu ;; esac
      image="nanocode-cli-builder:$arch"
      docker build -q --platform "linux/$arch" -t "$image" -f scripts/cli-builder.Dockerfile scripts >&2
      docker run --rm --platform "linux/$arch" -v "$root:/src" -v "nanocode-cargo-$arch:/usr/local/cargo/registry" -w /src \
        -e CARGO_TARGET_DIR="/src/target/linux-$arch" -e RUSTC_WRAPPER= -e RUSTC_WORKSPACE_WRAPPER= -e CARGO_INCREMENTAL=0 \
        "$image" scripts/build-cli.sh nanocode-cli nanocode >&2
      built="target/linux-$arch/cli/$triple"
      if [ "$arch" = amd64 ]; then
        cp "$built/nanocode-agent" "$dest/nanocode-$triple"
        cp "$built/nanocode-exec" "$dest/nanocode-exec-$triple"
        cp "$built/nanocode" "$dest/nanocode-front-$triple"
      else
        cp "$built/nanocode" "$dest/nanocode-$triple"
      fi
      cp "$built/nanocode-python" "$dest/nanocode-python-$(digest "$built/nanocode-python")"
      ;;
    windows-x86_64 | windows-aarch64)
      triple="${name#windows-}-pc-windows-msvc"
      export CARGO_TARGET_DIR="$root/target/lane-$name"
      built="$(scripts/build-cli.sh nanocode-cli nanocode "$triple")"
      mkdir -p "$dest/windows/nanocode-$triple"
      cp "$built" "$(dirname "$built")/nanocode-python.exe" "$dest/windows/nanocode-$triple/"
      ;;
    *)
      echo "release-cli.sh: unknown lane $name; lanes are $all" >&2
      exit 1
      ;;
  esac
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
pack() {
  for name in $all; do
    [ -d "$out/lanes/$name" ] || { echo "release-cli.sh: lane $name is missing from $out/lanes; build it here or copy it in" >&2; exit 1; }
  done
  dist="$out/dist"
  rm -rf "$dist"
  mkdir -p "$dist"
  for name in $all; do cp -R "$out/lanes/$name/." "$dist/"; done
  cd "$dist"
  pids=
  for binary in nanocode-*; do
    [ -f "$binary" ] || continue
    case $binary in nanocode-python-*) ;; *) gzip -9 -c "$binary" > "$binary.gz" & pids="$pids $!" ;; esac
    case $binary in nanocode-cua-driver-* | nanocode-front-*) ;; *) xz -9e -c "$binary" > "$binary.xz" & pids="$pids $!" ;; esac
  done
  for stage in windows/nanocode-*; do
    (cd "$stage" && zip -9 -q -X "../../${stage#windows/}.zip" nanocode.exe nanocode-python.exe) &
    pids="$pids $!"
  done
  status=0
  for pid in $pids; do wait "$pid" || status=1; done
  [ $status = 0 ] || { echo "release-cli.sh: compression failed" >&2; exit 1; }
  for file in ./*.gz ./*.xz ./*.zip; do printf '%s  %s\n' "$(digest "$file")" "${file#./}"; done > SHA256SUMS
  cd "$root"
}
publish() {
  dist="$out/dist"
  [ -s "$dist/SHA256SUMS" ] || { echo "release-cli.sh: nothing packed in $dist" >&2; exit 1; }
  git push -f origin "$(git rev-parse HEAD):refs/tags/nanocode-cli"
  cp install "$dist/install.sh"
  cp install.ps1 "$dist/install.ps1"
  cd "$dist"
  gh release upload nanocode-cli --repo nanocodexyz/nanocode --clobber ./*.gz ./*.xz ./*.zip SHA256SUMS install.sh install.ps1
}
command="${1:-all}"
[ $# -gt 0 ] && shift
case $command in
  lane) lane "$1" ;;
  build) build "$@" ;;
  pack) pack ;;
  publish) publish ;;
  all) build && pack ;;
  --publish) build && pack && publish ;;
  *) echo "usage: release-cli.sh [build [lane...] | pack | publish | --publish]; lanes: $all" >&2; exit 1 ;;
esac

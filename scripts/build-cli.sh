#!/bin/sh
set -eu
package=$1
bin=$2
root="$(cd "$(dirname "$0")/.." && pwd)"
target="${3:-$(rustc -vV | sed -n 's/^host: //p')}"
profile="${PROFILE:-dist}"
dir="${CARGO_TARGET_DIR:-$root/target}"
out="$dir/cli/$target"
mkdir -p "$out"
cd "$root"
env -u CARGO_NET_OFFLINE cargo fetch --locked >&2
native() {
  if command -v cygpath >/dev/null 2>&1; then cygpath -m "$1"; else printf '%s\n' "$1"; fi
}
offline() {
  if command -v ip >/dev/null 2>&1 && unshare -rn ip link set lo up 2>/dev/null; then
    unshare -rn sh -c 'ip link set lo up && user=$0 group=$1 && shift && exec unshare --user --map-user="$user" --map-group="$group" "$@"' "$(id -u)" "$(id -g)" "$@"
  elif command -v ip >/dev/null 2>&1 && sudo -n unshare --net true 2>/dev/null; then
    sudo -n unshare --net sh -c 'ip link set lo up && user=$0 group=$1 path=$2 && shift 2 && exec setpriv --reuid="$user" --regid="$group" --clear-groups env PATH="$path" "$@"' "$(id -u)" "$(id -g)" "$PATH" "$@"
  else
    echo "build-cli.sh: no network namespace for training (needs iproute2 and unprivileged user namespaces or sudo), training with the network" >&2
    "$@"
  fi
}
digest() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi
}
remap="--remap-path-prefix=$(native "$(pwd -P)")=/src --remap-path-prefix=$(native "${CARGO_HOME:-$HOME/.cargo}")=/cargo --remap-path-prefix=$(rustc --print sysroot)/lib/rustlib/src/rust=/rustc/$(rustc -vV | sed -n 's/^commit-hash: //p')"
exe=
case $target in *-windows-msvc) exe=.exe ;; esac
if [ "$package" = nanocode-cli ] && [ -n "${NANOCODE_PYTHON:-}" ]; then
  cp "$NANOCODE_PYTHON" "$out/nanocode-python$exe"
  case $target in
    *-windows-msvc) ;;
    *)
      NANOCODE_PYTHON_SHA256="$(digest "$out/nanocode-python")"
      export NANOCODE_PYTHON_SHA256
      ;;
  esac
fi
plain() {
  if [ "$package" = nanocode-cli ] && [ -z "${NANOCODE_PYTHON:-}" ]; then
    RUSTFLAGS="$remap" cargo build --locked --offline --profile "$profile" --target "$target" -p nanocode-python-cli --bin nanocode-python >&2
    cp "$dir/$target/$profile/nanocode-python" "$out/nanocode-python"
    NANOCODE_PYTHON_SHA256="$(digest "$out/nanocode-python")"
    export NANOCODE_PYTHON_SHA256
  fi
  RUSTFLAGS="$remap" cargo build --locked --offline --profile "$profile" --target "$target" -p "$package" --bin "$bin" >&2
  cp "$dir/$target/$profile/$bin" "$out/$bin"
}
case $target in
  *-windows-msvc)
    exe=.exe
    case $(rustc -vV | sed -n 's/^host: //p') in
      *-windows-*) ;;
      *)
        [ -n "${NANOCODE_XWIN:-}" ] || { echo "build-cli.sh: building $target off Windows needs NANOCODE_XWIN, a directory from xwin splat" >&2; exit 1; }
        arch="${target%%-*}"
        INCLUDE="$NANOCODE_XWIN/crt/include;$NANOCODE_XWIN/sdk/include/ucrt;$NANOCODE_XWIN/sdk/include/um;$NANOCODE_XWIN/sdk/include/shared"
        export INCLUDE
        export "CARGO_TARGET_$(printf '%s' "$target" | tr a-z- A-Z_)_LINKER=${LLD_LINK:-lld-link}"
        remap="$remap -Lnative=$NANOCODE_XWIN/crt/lib/$arch -Lnative=$NANOCODE_XWIN/sdk/lib/um/$arch -Lnative=$NANOCODE_XWIN/sdk/lib/ucrt/$arch"
        ;;
    esac
    if [ "$package" = nanocode-cli ] && [ -z "${NANOCODE_PYTHON:-}" ]; then
      RUSTFLAGS="$remap" cargo build --locked --offline --profile "$profile" --target "$target" -p nanocode-python-cli --bin nanocode-python >&2
      cp "$dir/$target/$profile/nanocode-python.exe" "$out/nanocode-python.exe"
    fi
    RUSTFLAGS="$remap" cargo build --locked --offline --profile "$profile" --target "$target" -p "$package" --bin "$bin" >&2
    cp "$dir/$target/$profile/$bin.exe" "$out/$bin.exe"
    ;;
  *-linux-gnu.*)
    if [ "$package" = nanocode-cli ] && [ -z "${NANOCODE_PYTHON:-}" ]; then
      RUSTFLAGS="$remap" cargo zigbuild --locked --offline --profile "$profile" --target "$target" -p nanocode-python-cli --bin nanocode-python >&2
      cp "$dir/${target%%.*}/$profile/nanocode-python" "$out/nanocode-python"
      NANOCODE_PYTHON_SHA256="$(digest "$out/nanocode-python")"
      export NANOCODE_PYTHON_SHA256
    fi
    RUSTFLAGS="$remap" cargo zigbuild --locked --offline --profile "$profile" --target "$target" -p "$package" --bin "$bin" >&2
    cp "$dir/${target%%.*}/$profile/$bin" "$out/$bin"
    ;;
  *-linux-*)
    if [ "${NANOCODE_BOLT:-}" != 1 ]; then
      plain
      echo "$out/$bin$exe"
      exit 0
    fi
    bolt="$(realpath "${LLVM_BOLT:-$(command -v llvm-bolt)}")" || { echo "build-cli.sh: needs llvm-bolt on PATH or LLVM_BOLT" >&2; exit 1; }
    flags="-Crelocation-model=static -Clink-arg=-Wl,--emit-relocs -Clink-arg=-Wl,-T,$root/scripts/cli-eh-frame.ld $remap"
    icf=all
    "$bolt" --help-hidden | grep -q -- '--icf=<value>' || icf=1
    case $target in
      x86_64-unknown-linux-gnu) flags="$flags -Clink-arg=-Wl,--icf=all" ;;
      *) flags="$flags -Clink-arg=-fuse-ld=lld" ;;
    esac
    nostrip="CARGO_PROFILE_$(printf '%s' "$profile" | tr a-z- A-Z_)_STRIP=none"
    if [ "$package" = nanocode-cli ] && [ -z "${NANOCODE_PYTHON:-}" ]; then
      env "$nostrip" RUSTFLAGS="$flags" cargo build --locked --offline --profile "$profile" --target "$target" -p nanocode-python-cli --bin nanocode-python >&2
      strip -o "$out/nanocode-python" "$dir/$target/$profile/nanocode-python"
      NANOCODE_PYTHON_SHA256="$(digest "$out/nanocode-python")"
      export NANOCODE_PYTHON_SHA256
    fi
    optimize() {
      name=$1
      env "$nostrip" RUSTFLAGS="$flags" \
        cargo rustc --locked --offline --profile "$profile" --target "$target" -p "$package" --bin "$name" -- -C save-temps -C link-arg=-Wl,-Map="$out/$name.map" --print link-args > "$out/$name.link.new"
      if [ -s "$out/$name.link.new" ]; then mv "$out/$name.link.new" "$out/$name.link"; else rm "$out/$name.link.new"; fi
      built="$dir/$target/$profile/$name"
      train="$dir/bolt/$target/$name"
      rm -rf "$train"
      mkdir -p "$train"
      "$bolt" "$built" -instrument -instrumentation-file="$train/profile" -instrumentation-file-append-pid -o "$train/instrumented" >&2
      offline python3 "$root/scripts/train-cli.py" "$train/instrumented" "$3" >&2
      mkdir "$train/startup"
      mv "$train"/profile.*.fdata "$train/startup/"
      offline python3 "$root/scripts/train-cli.py" "$train/instrumented" oneshot >&2
      mkdir "$train/oneshot"
      mv "$train"/profile.*.fdata "$train/oneshot/"
      offline python3 "$root/scripts/train-cli.py" "$train/instrumented" tools >&2
      mkdir "$train/tools"
      mv "$train"/profile.*.fdata "$train/tools/"
      offline python3 "$root/scripts/train-cli.py" "$train/instrumented" codemode >&2
      mkdir "$train/codemode"
      mv "$train"/profile.*.fdata "$train/codemode/"
      python3 "$root/scripts/rodata-cli.py" "$built" "$out/$name.map" "$out/$name.link" "$train/linked" "$train/startup" "$train/oneshot" "$train/tools" "$train/codemode" >&2
      if [ "$3" = startup ]; then
        offline python3 "$root/scripts/train-cli.py" "$train/instrumented" >&2
        mkdir "$train/session"
        mv "$train"/profile.*.fdata "$train/session/"
      fi
      "$(dirname "$bolt")/merge-fdata" "$train"/*/*.fdata "$train"/oneshot/*.fdata "$train"/tools/*.fdata "$train"/codemode/*.fdata > "$train/profile.fdata"
      layout="-reorder-blocks=ext-tsp -split-functions -split-all-cold -split-threshold=256 -split-eh -icf=$icf -use-old-text -no-huge-pages"
      "$bolt" "$train/linked" -o "$train/cdsort" -data="$train/profile.fdata" $layout -reorder-functions=cdsort -generate-function-order="$train/cdsort.order" >&2
      rm "$train/cdsort"
      python3 "$root/scripts/order-cli.py" "$train/cdsort.order" "$train/startup" "$train/oneshot" "$train/tools" "$train/codemode" > "$train/startup.order"
      input="$train/linked"
      order="$train/startup.order"
      if [ "$package" = nanocode-cli ] && [ "$target" = x86_64-unknown-linux-gnu ]; then
        python3 "$root/scripts/static-cli.py" "$out/$name.link" "$train/linked.ld" "$root/scripts/cli-libc.order" "$train/static" >&2
        cat "$root/scripts/cli-libc.order" "$train/startup.order" > "$train/static.order"
        input="$train/static"
        order="$train/static.order"
      fi
      if ! "$bolt" "$input" -o "$train/optimized" -data="$train/profile.fdata" $layout -reorder-functions=user -function-order="$order" > "$train/optimized.log" 2>&1; then
        [ "$input" = "$train/static" ] || { cat "$train/optimized.log" >&2; exit 1; }
        tail -n 3 "$train/optimized.log" >&2
        echo "build-cli.sh: BOLT can't rewrite the static $name; optimizing the dynamic link instead" >&2
        input="$train/linked"
        order="$train/startup.order"
        "$bolt" "$input" -o "$train/optimized" -data="$train/profile.fdata" $layout -reorder-functions=user -function-order="$order" > "$train/optimized.log" 2>&1
      fi
      cat "$train/optimized.log" >&2
      if [ "$input" = "$train/static" ] && grep -q 'original .text too small' "$train/optimized.log"; then
        reserve=$(sed -n 's/.*using 0x\([0-9a-f]*\) alignment\. \([0-9]*\) bytes needed, have \([0-9]*\) bytes available.*/\1 \2 \3/p' "$train/optimized.log" | head -n 1 | { read -r align needed have; align=$((0x$align)); echo $(( (needed - have + align - 1) / align * align )); })
        python3 "$root/scripts/static-cli.py" "$out/$name.link" "$train/linked.ld" "$root/scripts/cli-libc.order" "$train/static" "$reserve" >&2
        if ! "$bolt" "$input" -o "$train/optimized" -data="$train/profile.fdata" $layout -reorder-functions=user -function-order="$order" > "$train/optimized.log" 2>&1; then
          tail -n 3 "$train/optimized.log" >&2
          echo "build-cli.sh: BOLT can't rewrite the reserved static $name; using -lite on the unreserved one" >&2
          python3 "$root/scripts/static-cli.py" "$out/$name.link" "$train/linked.ld" "$root/scripts/cli-libc.order" "$train/static" >&2
          echo "original .text too small" > "$train/optimized.log"
        else
          cat "$train/optimized.log" >&2
        fi
      fi
      if grep -q 'original .text too small' "$train/optimized.log"; then
        "$bolt" "$input" -o "$train/optimized" -data="$train/profile.fdata" $layout -lite -reorder-functions=user -function-order="$order" >&2
      fi
      "$(dirname "$bolt")/llvm-strip" --remove-section=.note.bolt_info -o "$out/$2" "$train/optimized"
      python3 "$root/scripts/trim-cli.py" "$out/$2"
    }
    agent="$bin"
    if [ "$package" = nanocode-cli ] && [ "$target" = x86_64-unknown-linux-gnu ]; then agent=nanocode-agent; fi
    optimize "$bin" "$agent" startup
    if [ "$agent" != "$bin" ]; then
      optimize nanocode-exec nanocode-exec oneshot
      CARGO_TARGET_DIR="$dir" cargo build --locked --offline --release --target "$target" --manifest-path bin/nanocode-front/Cargo.toml >&2
      cp "$dir/$target/release/nanocode-front" "$out/$bin"
    fi
    ;;
  *)
    plain
    ;;
esac
echo "$out/$bin$exe"

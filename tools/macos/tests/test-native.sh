#!/bin/bash
# test-native.sh runs one test thread unless asked otherwise (#548: parallel GPU tests hung an M1
# Pro's GPU), keeps a caller's RUST_TEST_THREADS, and refuses to start without the Metal Shader
# Converter (#379). Run: /bin/bash tools/macos/tests/test-native.sh
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
script="$(dirname "$here")/test-native.sh"
root=$(mktemp -d "${TMPDIR:-/tmp}/victory-test-native.XXXXXX")
cleanup() { rm -rf "$root"; }
trap cleanup EXIT

# A stub toolchain: `rustup` prints a sysroot of our own, and that sysroot's `cargo` records what
# it was given. The script puts the sysroot's bin first, so the stub is what runs - the real cargo
# would build and run the suite.
sysroot="$root/sysroot"
mkdir -p "$root/bin" "$sysroot/bin" "$root/msc"
cat > "$root/bin/rustup" <<EOF
#!/bin/sh
echo "$sysroot"
EOF
cat > "$sysroot/bin/cargo" <<EOF
#!/bin/sh
{ printf '%s' "\${RUST_TEST_THREADS:-unset}"; printf ' %s' "\$@"; echo; } > "$root/seen"
EOF
chmod +x "$root/bin/rustup" "$sysroot/bin/cargo"
# The converter the script insists on, as a file of its own.
: > "$root/msc/libmetalirconverter.dylib"

# Rustup, cargo and the converter through the environment the script is given.
run() { # run [arguments...]
  PATH="$root/bin:$PATH" VICTORY_MSC_DYLIB="$root/msc/libmetalirconverter.dylib" "$script" "$@"
}

# Default: one thread, and the caller's arguments reach cargo.
rm -f "$root/seen"
run -p victory-gpu --lib > /dev/null
read -r -a seen < "$root/seen"
[ "${seen[0]}" = "1" ] || { echo "FAIL: default threads '${seen[0]}', want 1" >&2; exit 1; }
[ "${seen[*]:1}" = "test -p victory-gpu --lib" ] \
  || { echo "FAIL: cargo got '${seen[*]:1}'" >&2; exit 1; }

# A caller's own RUST_TEST_THREADS is kept.
rm -f "$root/seen"
RUST_TEST_THREADS=4 run --lib > /dev/null
read -r -a seen < "$root/seen"
[ "${seen[0]}" = "4" ] || { echo "FAIL: asked for 4 threads, got '${seen[0]}'" >&2; exit 1; }

# Without the converter it stops before cargo, naming the download (#379).
rm -f "$root/seen"
if PATH="$root/bin:$PATH" VICTORY_MSC_DYLIB="$root/missing.dylib" "$script" --lib > /dev/null 2>&1; then
  echo "FAIL: ran without the converter" >&2; exit 1
fi
[ -e "$root/seen" ] && { echo "FAIL: cargo ran without the converter" >&2; exit 1; }
echo "test-native: ok"

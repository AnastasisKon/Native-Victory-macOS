#!/bin/sh
# The verification gate for native VIctory on a Mac: every check a change
# must pass before it goes into a pull request.
#
# usage: tools/macos/verify.sh [quick|full]
#
#   quick  the test suites: msc, suite-hle, suite-shader, suite-gpu
#   full   quick + policy + clippy-win (default)
#
# Gates: msc (Apple's Metal Shader Converter present), suite-hle and
# suite-shader (CONTRIBUTING.md's two), suite-gpu (victory-gpu natively),
# then in full: policy (cargo xtask policy) and clippy-win (the CI clippy
# gate for the Windows target).
#
# Prints one PASS/FAIL line per gate and exits non-zero if any gate failed,
# so a caller (CI, a hook, a human) can gate a pull request on it. The msc
# gate fails fast, before any suite: without the converter the gpu and
# shader tests panic on the missing dylib, which looks like a GPU bug (#379).
set -u
cd "$(dirname "$0")/../.." || exit 1

GATE="${1:-full}"
RUNNER="rustup run 1.98.1"
fail=0

echo "== native VIctory verification gate ($GATE) =="

MSC="${VICTORY_MSC_DYLIB:-/usr/local/lib/libmetalirconverter.dylib}"
printf '%-14s ... ' "msc"
if [ -f "$MSC" ]; then
    echo "PASS ($MSC)"
else
    echo "FAIL"
    echo "    Metal Shader Converter not found at $MSC: the gpu and shader tests need it."
    echo "    Download \"Metal shader converter\" from https://developer.apple.com/metal/shader-converter/"
    echo "    and run its installer (free with an Apple ID), or set VICTORY_MSC_DYLIB to its dylib."
    exit 1
fi

run_gate() {
    name="$1"; shift
    printf '%-14s ... ' "$name"
    log=$(mktemp)
    if "$@" >"$log" 2>&1; then
        # Summarize: last "test result:" lines carry the counts.
        counts=$(grep -h "^test result:" "$log" | awk '{p+=$4; f+=$6} END {printf "%d passed, %d failed", p, f}')
        echo "PASS (${counts:-ok})"
    else
        echo "FAIL"
        grep -E "^(error|failures:|---- )" "$log" | head -10 | sed 's/^/    /'
        tail -3 "$log" | sed 's/^/    /'
        fail=1
    fi
    rm -f "$log"
}

run_gate "suite-hle" sh -c \
    "$RUNNER cargo test --target x86_64-apple-darwin -p victory-hle -p victory-loader -p victory"

run_gate "suite-shader" sh -c \
    "$RUNNER cargo test -p victory-shader -p victory-hle"

run_gate "suite-gpu" sh -c \
    "$RUNNER cargo test -p victory-gpu"

if [ "$GATE" = "full" ]; then
    run_gate "policy" sh -c \
        "$RUNNER cargo xtask policy"
    run_gate "clippy-win" sh -c \
        "$RUNNER cargo clippy --workspace --all-targets --locked --target x86_64-pc-windows-gnullvm -- -D warnings"
fi

echo "== gate $([ $fail -eq 0 ] && echo PASSED || echo FAILED) =="
exit $fail

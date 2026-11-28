#!/bin/bash
# Game-free regressions for check-game-data.sh: fixtures instead of an install.
# Run: /bin/bash tools/macos/tests/check-game-data.sh
set -euo pipefail
scripts=$(cd "$(dirname "$0")/.." && pwd)
root=$(mktemp -d "${TMPDIR:-/tmp}/victory data.XXXXXX")
root=$(cd "$root" && pwd -P)
trap 'rm -rf "$root"' EXIT

fail() { echo "FAIL: $*" >&2; cat "$root/output" >&2; exit 1; }
contains() { grep -F -- "$1" "$root/output" >/dev/null || fail "missing line: $1"; }

rc=0
run_script() {
  rc=0
  /bin/bash "$scripts/check-game-data.sh" "$@" > "$root/output" 2>&1 || rc=$?
}

# 4 MiB of data: never a hole at a 4 KiB sample.
whole() {
  dd if=/dev/urandom of="$1" bs=1m count=4 2>/dev/null
}

# What a preallocating downloader leaves: a full length, a real first block and a
# real last block, and zeros in between (the shape shaders.rpf had, issue #632).
holey() {
  local file="$1" blocks="$2"
  dd if=/dev/zero of="$file" bs=4096 count="$blocks" 2>/dev/null
  dd if=/dev/urandom of="$file" bs=4096 count=1 conv=notrunc 2>/dev/null
  dd if=/dev/urandom of="$file" bs=4096 count=1 seek=$(( blocks - 1 )) conv=notrunc 2>/dev/null
}

test_a_complete_folder_is_quiet() {
  mkdir -p "$root/complete"
  whole "$root/complete/common.rpf"
  whole "$root/complete/data.rpf"
  run_script "$root/complete"
  [[ $rc == 0 ]] || fail "a complete folder exited $rc"
  contains "common.rpf"
  contains "none a hole"
  if grep -Fq MISSING "$root/output"; then fail "a complete folder was reported as missing data"; fi
}

test_a_hole_is_named() {
  mkdir -p "$root/partial"
  holey "$root/partial/shaders.rpf" 512
  whole "$root/partial/textures.rpf"
  run_script "$root/partial"
  [[ $rc == 1 ]] || fail "a holey archive exited $rc, not 1"
  contains "shaders.rpf"
  contains "MISSING DATA"
  contains "textures.rpf"
  contains "Finish the"
}

test_a_folder_with_only_its_header_and_tail_reads_as_a_hole() {
  # The trap's shape: a header and the last piece are valid, so sizes, the magic
  # and the tail all look right, and the middle is zeros.
  mkdir -p "$root/header-and-tail"
  holey "$root/header-and-tail/shaders.rpf" 64
  run_script --samples 8 "$root/header-and-tail"
  [[ $rc == 1 ]] || fail "header-and-tail exited $rc, not 1"
  contains "MISSING DATA"
}

test_a_small_archive_is_read_block_by_block() {
  mkdir -p "$root/small"
  dd if=/dev/urandom of="$root/small/appdata0.rpf" bs=4096 count=3 2>/dev/null
  run_script --samples 16 "$root/small"
  [[ $rc == 0 ]] || fail "a small complete archive exited $rc"
  contains "3 samples, none a hole"
}

test_the_sample_count_is_honoured() {
  mkdir -p "$root/count"
  holey "$root/count/packs.rpf" 1024
  run_script --samples 4 "$root/count"
  [[ $rc == 1 ]] || fail "exited $rc"
  # Three of the four samples at step 256 are holes; the header and tail samples run.
  contains "4 samples"
}

test_a_folder_without_archives_is_a_usage_error() {
  mkdir -p "$root/empty"
  printf 'not a game\n' > "$root/empty/game_scarlett_master.exe"
  run_script "$root/empty"
  [[ $rc == 2 ]] || fail "a folder with no archive exited $rc, not 2"
  contains "no .rpf archive"
}

test_a_missing_folder_is_a_usage_error() {
  run_script "$root/not-here"
  [[ $rc == 2 ]] || fail "a missing folder exited $rc, not 2"
  contains "not a folder"
}

test_bad_arguments_are_usage_errors() {
  run_script
  [[ $rc == 2 ]] || fail "no argument exited $rc, not 2"
  run_script --samples 0 "$root"
  [[ $rc == 2 ]] || fail "--samples 0 exited $rc, not 2"
  run_script --samples=abc "$root"
  [[ $rc == 2 ]] || fail "--samples=abc exited $rc, not 2"
  run_script --nonsense "$root"
  [[ $rc == 2 ]] || fail "an unknown option exited $rc, not 2"
}

test_several_folders_in_one_call() {
  mkdir -p "$root/one" "$root/two with spaces"
  whole "$root/one/data.rpf"
  holey "$root/two with spaces/levels.rpf" 256
  run_script "$root/one" "$root/two with spaces"
  [[ $rc == 1 ]] || fail "one holey folder among two exited $rc, not 1"
  contains "data.rpf"
  contains "levels.rpf"
}

test_help_exits_zero() {
  run_script --help
  [[ $rc == 0 ]] || fail "--help exited $rc"
  contains "check-game-data.sh"
}

for test in a_complete_folder_is_quiet a_hole_is_named \
  a_folder_with_only_its_header_and_tail_reads_as_a_hole \
  a_small_archive_is_read_block_by_block the_sample_count_is_honoured \
  a_folder_without_archives_is_a_usage_error a_missing_folder_is_a_usage_error \
  bad_arguments_are_usage_errors several_folders_in_one_call help_exits_zero; do
  "test_$test"
  echo "PASS: $test"
done

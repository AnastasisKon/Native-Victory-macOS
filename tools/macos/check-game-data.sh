#!/bin/bash
# Says whether a game folder's archives hold the data they claim to, before a run
# is spent on them.
# Usage: check-game-data.sh [--samples N] <game folder>
#
# A downloader that preallocates leaves every archive at its full length, with the
# parts it has not downloaded reading as zeros, so the title reads a valid header,
# a valid size and nothing in between. Its boot shader database is then written
# from that and asserts (docs/native-host.md, the int3 at image+0x1330a44), which
# costs a run and says nothing about the host. This samples the archives and says
# which ones are still holes.
#
# Exit: 0 every archive holds data, 1 an archive is mostly holes, 2 no archive to
# check or the arguments are not usable.
set -uo pipefail
export LC_ALL=C

samples=32
folders=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    --samples)
      [[ $# -ge 2 && "$2" =~ ^[1-9][0-9]*$ ]] || { echo '--samples needs a positive count' >&2; exit 2; }
      samples="$2"; shift 2 ;;
    --samples=*)
      value="${1#--samples=}"
      [[ "$value" =~ ^[1-9][0-9]*$ ]] || { echo '--samples needs a positive count' >&2; exit 2; }
      samples="$value"; shift ;;
    -h|--help) sed -n '2,15p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    -*) echo "unknown option: $1" >&2; exit 2 ;;
    *) folders+=("$1"); shift ;;
  esac
done
[[ ${#folders[@]} -gt 0 ]] || { sed -n '3,4p' "$0" | sed 's/^# \{0,1\}//' >&2; exit 2; }

# Human size of a byte count.
size_of() {
  awk -v b="$1" 'BEGIN {
    split("B KiB MiB GiB TiB", unit, " "); i = 1
    while (b >= 1024 && i < 5) { b /= 1024; i++ }
    printf (i == 1 ? "%.0f %s" : "%.1f %s"), b, unit[i]
  }'
}

# One 4 KiB block of `$1` at block index `$2` is a hole when every byte is zero.
is_hole() {
  local left
  left=$(dd if="$1" bs=4096 skip="$2" count=1 2>/dev/null | tr -d '\0' | wc -c)
  [[ $((left)) -eq 0 ]]
}

# Prints one line per archive and returns 0 when all of them hold data.
check_file() {
  local file="$1" base size blocks step taken=0 holes=0 block
  base=$(basename "$file")
  size=$(stat -f %z "$file" 2>/dev/null) || { printf '%-28s cannot be read\n' "$base"; return 1; }
  blocks=$(( (size + 4095) / 4096 ))
  if [[ $blocks -le 0 ]]; then
    printf '%-28s empty\n' "$base"
    return 1
  fi
  if [[ $blocks -le $samples ]]; then
    step=1
  else
    step=$(( blocks / samples ))
    # Spread the samples to the last block, which is where a sequential
    # download's tail piece lands first.
    step=$(( step > 1 ? step : 1 ))
  fi
  for ((block = 0; block < blocks; block += step)); do
    taken=$(( taken + 1 ))
    if is_hole "$file" "$block"; then holes=$(( holes + 1 )); fi
    [[ $taken -ge $samples ]] && break
  done
  located=$(awk -v h="$holes" -v t="$taken" 'BEGIN { printf "%d", (100 * h) / t }')
  if [[ $holes -eq 0 ]]; then
    printf '%-28s %10s  %2d samples, none a hole  ok\n' "$base" "$(size_of "$size")" "$taken"
    return 0
  fi
  printf '%-28s %10s  %2d samples, %2d holes (%s%% of what was read)  MISSING DATA\n' \
    "$base" "$(size_of "$size")" "$taken" "$holes" "$located"
  return 1
}

status=0
for folder in "${folders[@]}"; do
  [[ -d "$folder" ]] || { echo "not a folder: $folder" >&2; exit 2; }
  found=0
  for file in "$folder"/*.rpf; do
    [[ -e "$file" ]] || continue
    found=$(( found + 1 ))
    check_file "$file" || status=1
  done
  if [[ $found -eq 0 ]]; then
    echo "no .rpf archive in $folder" >&2
    exit 2
  fi
done
if [[ $status -ne 0 ]]; then
  echo
  echo 'Some archives are holes: a downloader has not filled them in yet, and the'
  echo 'title asserts in its boot shader database when it reads them. Finish the'
  echo 'download (or re-check it) before spending a run.'
fi
exit "$status"

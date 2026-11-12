#!/usr/bin/env bash
# Runs the larger instances in Deep/Configs.lean. Builds each model checker,
# runs it for at most DEEP_TIMEOUT seconds, and fails if any finds a violation.
# A check that runs out of time is reported as incomplete, not as a failure.
set -euo pipefail
cd "$(dirname "$0")"
timeout_s="${DEEP_TIMEOUT:-2400}"
out_dir="${DEEP_OUT:-.lake/deep}"
mkdir -p "$out_dir"

mapfile -t configs < <(grep -E '^#mc_config' Deep/Configs.lean)
mapfile -t binaries < <(lake env lean Deep/Configs.lean | sed -n 's/^Model checker built: //p')
if [ "${#binaries[@]}" -ne "${#configs[@]}" ]; then
  echo "expected ${#configs[@]} model checkers, built ${#binaries[@]}" >&2
  exit 1
fi

failed=0
for i in "${!binaries[@]}"; do
  name="${configs[$i]#\#mc_config }"
  result="$out_dir/check-$i.json"
  echo "::group::$name"
  start=$(date +%s)
  # The binary exits as soon as its stdin closes, so hold stdin open; it
  # prints its result on stdout and its progress on stderr. The arguments
  # split the search into one task per core.
  set +e
  timeout "$timeout_s" "${binaries[$i]}" "$(nproc)" 20 \
    < <(sleep $((timeout_s + 60))) > "$result" 2>/dev/null
  code=$?
  set -e
  took=$(( $(date +%s) - start ))
  if [ "$code" -eq 124 ]; then
    verdict="⏱ incomplete: stopped after ${timeout_s}s"
  elif grep -q '"no_violation_found"' "$result"; then
    verdict="✅ no violation ($(grep -o '"explored_states": *[0-9]*' "$result" | grep -o '[0-9]*$') states, ${took}s)"
  else
    verdict="❌ violation or error (exit $code), see $result"
    cat "$result"
    failed=1
  fi
  echo "::endgroup::"
  echo "$name: $verdict" | tee -a "$out_dir/summary.txt"
done
exit "$failed"

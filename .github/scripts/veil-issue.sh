#!/usr/bin/env bash
# Files (or updates) a GitHub issue when the nightly Veil model check fails.
#
# Usage: veil-issue.sh <artifacts-dir>
# Needs GH_TOKEN, GH_REPO and RUN_URL in the environment. Keeps one open
# issue labelled `veil`: a new failure comments on it rather than opening
# another.
set -euo pipefail
artifacts="$1"
label=veil
body=$(mktemp)

{
  echo "The nightly Veil model check failed: $RUN_URL"
  echo
  echo "A violation means one of the models in \`verify/\` reached a state that"
  echo "breaks a property. Check first whether the code changed in a way the"
  echo "model does not reflect; if not, the trace below is a schedule of steps"
  echo "that leads the protocol into the bad state. If no trace is shown, the"
  echo "job failed for another reason (a build error, say): see the run."
  # Model-check job: each violation is an error line followed by its trace,
  # indented.
  for log in "$artifacts"/*/model-check.log; do
    [ -f "$log" ] || continue
    awk '/Violation/ {on=1; print; next} on && /^[ \t]/ {print; next} {on=0}' "$log" > "$body.mc"
    if [ -s "$body.mc" ]; then
      echo
      echo "### \`lake build\`"
      echo '```'
      head -c 30000 "$body.mc"
      echo '```'
    fi
  done
  # Deep job: summary, then the result of each failed check.
  for summary in "$artifacts"/*/summary.txt; do
    [ -f "$summary" ] || continue
    echo
    echo "### \`deep.sh\`"
    echo '```'
    cat "$summary"
    echo '```'
    for result in "$(dirname "$summary")"/check-*.json; do
      [ -f "$result" ] || continue
      grep -q '"no_violation_found"' "$result" && continue
      echo "<details><summary>$(basename "$result")</summary>"
      echo
      echo '```json'
      head -c 20000 "$result"
      echo '```'
      echo "</details>"
    done
  done
} > "$body"

gh label create "$label" --color B60205 \
  --description "Found by the Veil model check" --force >/dev/null
open=$(gh issue list --label "$label" --state open --json number --jq '.[0].number // empty')
if [ -n "$open" ]; then
  gh issue comment "$open" --body-file "$body"
else
  gh issue create --title "Veil model check failed" --label "$label" --body-file "$body"
fi

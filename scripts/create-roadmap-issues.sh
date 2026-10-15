#!/usr/bin/env bash
# Creates one GitHub Issue per planned ROADMAP item (RM-xxx) with a milestone label.
# Idempotent: skips items that already have an issue whose title starts with the RM id.
# Requires: gh auth login
set -euo pipefail
cd "$(dirname "$0")/.."
REPO="kajisho5/onchip"

for label in M1 M2 M3 M4 M5; do
  gh label create "milestone:$label" --repo "$REPO" --color "0E8A16" --force >/dev/null
done
gh label create "interop-report" --repo "$REPO" --color "1D76DB" --force >/dev/null

existing=$(gh issue list --repo "$REPO" --state all --limit 500 --json title --jq '.[].title')
current=""
while IFS= read -r line; do
  if [[ "$line" =~ ^##\ (M[0-9]+) ]]; then current="${BASH_REMATCH[1]}"; continue; fi
  if [[ "$line" =~ ^-\ \[\ \]\ (RM-[0-9]+)\ (.*)$ ]]; then
    id="${BASH_REMATCH[1]}"; title="${BASH_REMATCH[2]}"
    if grep -q "^$id" <<<"$existing"; then echo "skip $id"; continue; fi
    lbl="milestone:$current"; [[ "$current" == M5+ ]] && lbl="milestone:M5"
    gh issue create --repo "$REPO" --title "$id $title" --label "$lbl" \
      --body "Tracked from ROADMAP.md ($current). See docs/DESIGN.md." >/dev/null
    echo "created $id"
  fi
done < ROADMAP.md

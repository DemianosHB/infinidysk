#!/usr/bin/env bash
# stamp-release-notes.sh --repo <owner/repo> [--out <file>]
#
# Writes the cumulative release notes of every v2* tag in this clone, newest tag first, as
# markdown: one "## [version](release-url) (date)" section per tag with the PRs merged into
# rust during that tag's interval (previous v2* tag .. this tag). The newest tag also gets
# PRs merged after it was created, matching the old single-tag stamp.
#
# Reads the merged PRs as a JSON array on stdin, exactly as
#   gh pr list --state merged --base rust --limit 1000 --json number,title,url,mergedAt
# prints it. Tags come from the git repository in the current directory (checkout with
# fetch-tags), so the script itself needs no network access. Tag timestamps are read as
# unix epochs because jq's fromdateiso8601 only parses the Z-suffixed form gh emits.

set -euo pipefail

repo=""
out=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --repo) repo="${2:?--repo needs a value}"; shift 2 ;;
    --out) out="${2:?--out needs a value}"; shift 2 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
if [[ -z "$repo" ]]; then
  echo "usage: stamp-release-notes.sh --repo <owner/repo> [--out <file>]" >&2
  exit 2
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

cat > "$tmp/prs.json"
git tag -l 'v2*' --sort=creatordate \
  --format='{"tag":"%(refname:short)","ts":%(creatordate:unix),"day":"%(creatordate:short)"}' \
  > "$tmp/tags.jsonl"

notes="$(jq -s -r -j --arg repo "$repo" '
  .[0] as $prs |
  .[1:] as $tags |
  ($tags | length) as $n |
  [ range(0; $n) as $i |
    ($tags[$i]) as $t |
    (if $i == 0 then 0 else $tags[$i - 1].ts end) as $since |
    (if $i == $n - 1 then 253402300799 else $t.ts end) as $until |
    {
      tag: $t.tag,
      version: ($t.tag | sub("^v"; "")),
      day: $t.day,
      changes: ([ $prs[]
                  | select((.mergedAt | fromdateiso8601) > $since and (.mergedAt | fromdateiso8601) <= $until) ]
                | sort_by(.mergedAt)
                | map("* \(.title) ([#\(.number)](\(.url)))"))
    }
  ]
  | reverse
  | .[]
  | "## [\(.version)](https://github.com/\($repo)/releases/tag/\(.tag)) (\(.day))\n\n### Changes\n\n"
    + ((.changes | join("\n")) + "\n\n")
' "$tmp/prs.json" "$tmp/tags.jsonl")"

if [[ -n "$out" ]]; then
  printf '%s\n' "$notes" > "$out"
else
  printf '%s\n' "$notes"
fi

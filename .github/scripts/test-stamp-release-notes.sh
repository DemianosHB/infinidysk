#!/usr/bin/env bash

set -euo pipefail

export TZ=UTC

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
STAMP="$ROOT/.github/scripts/stamp-release-notes.sh"
REPO="DemianosHB/infinidysk"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

# A scratch clone with three v2* tags; the annotated tag dates drive the intervals.
gitdir="$TMP/repo"
mkdir "$gitdir"
cd "$gitdir"
git init -q
git config user.email test@example.com
git config user.name Test
git config commit.gpgsign false
git config tag.gpgsign false
git commit -q --allow-empty -m init
GIT_COMMITTER_DATE="2026-11-20T10:00:00+00:00" git commit -q --allow-empty -m c1
GIT_COMMITTER_DATE="2026-11-20T12:00:00+00:00" git tag -a -m t1 v2.0.0-beta.11-Demianos1
GIT_COMMITTER_DATE="2026-11-24T10:00:00+00:00" git commit -q --allow-empty -m c2
GIT_COMMITTER_DATE="2026-11-25T12:00:00+00:00" git tag -a -m t2 v2-beta.03
GIT_COMMITTER_DATE="2026-11-26T10:00:00+00:00" git commit -q --allow-empty -m c3
GIT_COMMITTER_DATE="2026-11-27T12:00:00+00:00" git tag -a -m t3 v2-beta.04

# Deliberately unsorted mergedAt (Z-suffixed like gh pr list); #12 predates the first tag,
# #60 postdates the newest.
cat > "$TMP/prs.json" <<'EOF'
[
  {"number": 59, "title": "chore(docs): redesign rust-branch README", "url": "https://github.com/DemianosHB/infinidysk/pull/59", "mergedAt": "2026-11-27T09:00:00Z"},
  {"number": 47, "title": "fix(ui): let fetch set the form content-type", "url": "https://github.com/DemianosHB/infinidysk/pull/47", "mergedAt": "2026-11-24T08:00:00Z"},
  {"number": 55, "title": "feat(api): second change of beta.04", "url": "https://github.com/DemianosHB/infinidysk/pull/55", "mergedAt": "2026-11-26T20:00:00Z"},
  {"number": 12, "title": "chore: before the first tag", "url": "https://github.com/DemianosHB/infinidysk/pull/12", "mergedAt": "2026-11-01T00:00:00Z"},
  {"number": 60, "title": "feat(api): merged after the newest tag", "url": "https://github.com/DemianosHB/infinidysk/pull/60", "mergedAt": "2026-11-28T00:00:00Z"},
  {"number": 54, "title": "feat(api): first change of beta.04", "url": "https://github.com/DemianosHB/infinidysk/pull/54", "mergedAt": "2026-11-25T13:00:00Z"}
]
EOF

cat > "$TMP/expected.md" <<EOF
## [2-beta.04](https://github.com/$REPO/releases/tag/v2-beta.04) (2026-11-27)

### Changes

* feat(api): first change of beta.04 ([#54](https://github.com/$REPO/pull/54))
* feat(api): second change of beta.04 ([#55](https://github.com/$REPO/pull/55))
* chore(docs): redesign rust-branch README ([#59](https://github.com/$REPO/pull/59))
* feat(api): merged after the newest tag ([#60](https://github.com/$REPO/pull/60))

## [2-beta.03](https://github.com/$REPO/releases/tag/v2-beta.03) (2026-11-25)

### Changes

* fix(ui): let fetch set the form content-type ([#47](https://github.com/$REPO/pull/47))

## [2.0.0-beta.11-Demianos1](https://github.com/$REPO/releases/tag/v2.0.0-beta.11-Demianos1) (2026-11-20)

### Changes

* chore: before the first tag ([#12](https://github.com/$REPO/pull/12))
EOF

bash "$STAMP" --repo "$REPO" < "$TMP/prs.json" > "$TMP/actual.md"
diff -u "$TMP/expected.md" "$TMP/actual.md"

# --out writes the same notes to the file instead of stdout.
bash "$STAMP" --repo "$REPO" --out "$TMP/stamped.md" < "$TMP/prs.json"
diff -u "$TMP/expected.md" "$TMP/stamped.md"

# The stamp is deterministic: a rerun produces identical output.
bash "$STAMP" --repo "$REPO" < "$TMP/prs.json" > "$TMP/actual2.md"
diff -u "$TMP/actual.md" "$TMP/actual2.md"

echo "stamp-release-notes.sh: all fixtures passed"

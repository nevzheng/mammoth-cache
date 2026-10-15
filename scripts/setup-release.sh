#!/usr/bin/env bash
# One-time release setup for the repository owner. See docs/design/0009-releases.md.
#
# 1. Creates the fine-grained token release-plz needs and stores it as RELEASE_PLZ_TOKEN.
# 2. Turns on immutable releases, so a published release's assets and tag can't be changed.
# 3. Adds a tag ruleset on v*, so a version tag can't be moved or deleted (admins can bypass).
#
# Each step is skipped if it is already done, so rerun it when the token expires.
#
# Usage: scripts/setup-release.sh [owner/repo]
set -euo pipefail

repo="${1:-nevzheng/mammoth-cache}"
owner="${repo%%/*}"
name="${repo#*/}"

echo "== RELEASE_PLZ_TOKEN"
if gh secret list -R "$repo" | grep -q '^RELEASE_PLZ_TOKEN[[:space:]]' && [ "${RENEW:-}" != 1 ]; then
  echo "Already set. To replace an expiring token, run: RENEW=1 $0 $repo"
else
  url="https://github.com/settings/personal-access-tokens/new?name=${name}-release-plz&target_name=${owner}&expires_in=366&contents=write&pull_requests=write"
  echo "Opening the token form, prefilled with Contents and Pull requests read/write:"
  echo "  $url"
  echo "Under 'Repository access' pick 'Only select repositories' -> $name,"
  echo "then click 'Generate token', copy it, and paste it at the prompt below."
  open "$url" 2>/dev/null || xdg-open "$url" 2>/dev/null || true
  gh secret set RELEASE_PLZ_TOKEN -R "$repo"
  echo "Stored. The token expires in a year; rerun with RENEW=1 then."
fi

echo "== Immutable releases"
gh api -X PUT "repos/$repo/immutable-releases" >/dev/null
echo "On."

echo "== Tag ruleset"
ruleset="Release tags"
if gh api "repos/$repo/rulesets" --jq '.[].name' | grep -qx "$ruleset"; then
  echo "'$ruleset' already exists."
else
  # Repository admins (role 5) can bypass, to fix a mistake by hand.
  gh api -X POST "repos/$repo/rulesets" --input - >/dev/null <<EOF
{
  "name": "$ruleset",
  "target": "tag",
  "enforcement": "active",
  "conditions": { "ref_name": { "include": ["refs/tags/v*"], "exclude": [] } },
  "rules": [{ "type": "update" }, { "type": "deletion" }, { "type": "non_fast_forward" }],
  "bypass_actors": [{ "actor_id": 5, "actor_type": "RepositoryRole", "bypass_mode": "always" }]
}
EOF
  echo "Added: v* tags can be created but not moved or deleted."
fi

echo
echo "Done. One step is left for after the first release image is pushed: make the package public"
echo "at https://github.com/users/$owner/packages/container/$name/settings (there is no API for it)."

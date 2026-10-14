#!/usr/bin/env bash
# One-time release setup: create the fine-grained token release-plz needs and store it as the
# RELEASE_PLZ_TOKEN repository secret. See docs/design/0009-releases.md.
#
# Usage: scripts/setup-release-token.sh [owner/repo]
set -euo pipefail

repo="${1:-nevzheng/mammoth-cache}"
owner="${repo%%/*}"
url="https://github.com/settings/personal-access-tokens/new?name=${repo#*/}-release-plz&target_name=${owner}&expires_in=366&contents=write&pull_requests=write"

echo "Opening the token form, prefilled with Contents and Pull requests read/write:"
echo "  $url"
echo "Under 'Repository access' pick 'Only select repositories' -> ${repo#*/},"
echo "then click 'Generate token', copy it, and paste it at the prompt below."
open "$url" 2>/dev/null || xdg-open "$url" 2>/dev/null || true

gh secret set RELEASE_PLZ_TOKEN -R "$repo"
echo "Stored RELEASE_PLZ_TOKEN on $repo. The token expires in a year; rerun this script then."

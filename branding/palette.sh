#!/usr/bin/env sh
# Regenerates branding/palette.css from branding/palette.json. Requires jq.
#
# palette.css defines three sets of custom properties:
#   --mc-light-<token>, --mc-dark-<token>  fixed per theme, for sites with their own theme switcher
#   --mc-<token>                           follows the OS light/dark preference
set -eu
cd "$(dirname "$0")"

# vars <mode> <prefix> <indent>
vars() {
  jq -r --arg mode "$1" --arg prefix "$2" --arg indent "$3" \
    '.[$mode] | to_entries[] | "\($indent)--\($prefix)\(.key): \(.value);"' palette.json
}

{
  echo "/* Generated from palette.json by palette.sh. Do not edit by hand. */"
  echo ""
  echo ":root {"
  vars light mc-light- "  "
  echo ""
  vars dark mc-dark- "  "
  echo ""
  vars light mc- "  "
  echo "}"
  echo ""
  echo "@media (prefers-color-scheme: dark) {"
  echo "  :root {"
  vars dark mc- "    "
  echo "  }"
  echo "}"
} > palette.css

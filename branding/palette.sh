#!/usr/bin/env sh
# Regenerates branding/palette.css from branding/palette.json. Requires jq.
set -eu
cd "$(dirname "$0")"

vars() {
  jq -r --arg mode "$1" '.[$mode] | to_entries[] | "  --mc-\(.key): \(.value);"' palette.json
}

{
  echo "/* Generated from palette.json by palette.sh. Do not edit by hand. */"
  echo ""
  echo ":root {"
  vars light
  echo "}"
  echo ""
  echo "@media (prefers-color-scheme: dark) {"
  echo "  :root {"
  vars dark | sed 's/^/  /'
  echo "  }"
  echo "}"
  echo ""
  echo "/* Explicit light/dark selectors for sites with a theme switcher. */"
  echo "[data-mc-theme=\"light\"] {"
  vars light
  echo "}"
  echo ""
  echo "[data-mc-theme=\"dark\"] {"
  vars dark
  echo "}"
} > palette.css

#!/usr/bin/env sh
# Builds or serves the docs site with mdBook, pulling colors and assets from branding/.
#   scripts/docs.sh          build into target/book
#   scripts/docs.sh serve    live-reload server on http://localhost:3000
set -eu
cd "$(dirname "$0")/.."

branding/palette.sh

# mdBook copies files under docs/ into the site, and uses book-theme/favicon.{svg,png} as the favicon.
# Both copies are gitignored; branding/ stays the only place to edit.
mkdir -p docs/assets/branding
cp branding/logo.svg branding/logo-dark.svg docs/assets/branding/
cp branding/favicon.svg book-theme/favicon.svg
cp branding/favicon-32.png book-theme/favicon.png

mdbook "${1:-build}"

# A sitemap for search engines, listing every page except mdBook's own.
if [ "${1:-build}" = build ]; then
  base=https://nevzheng.github.io/mammoth-cache
  {
    echo '<?xml version="1.0" encoding="UTF-8"?>'
    echo '<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">'
    (cd target/book && find . -name '*.html' ! -name 404.html ! -name print.html ! -name toc.html) |
      sed 's|^\./||' | sort | while read -r page; do echo "  <url><loc>$base/$page</loc></url>"; done
    echo '</urlset>'
  } > target/book/sitemap.xml
fi

# Branding

Everything that defines how mammoth-cache looks lives in this folder. The docs site reads its
colors, logo and favicon from here, so rebranding means editing these files only.

| File           | What it is                                                                  |
| -------------- | --------------------------------------------------------------------------- |
| `palette.json` | Color tokens for light and dark themes. The source of truth.                |
| `palette.css`  | The same tokens as CSS custom properties (`--mc-*`). Generated; don't edit. |
| `palette.sh`   | Regenerates `palette.css` from `palette.json` (needs `jq`).                 |
| `logo.svg`     | Placeholder wordmark.                                                       |
| `favicon.svg`  | Placeholder square mark for browser tabs.                                   |

## Palette

A [Solarized](https://ethanschoonover.com/solarized/) base, with orange promoted to the primary
color.

| Token            | Light     | Dark      | Use                                       |
| ---------------- | --------- | --------- | ----------------------------------------- |
| `bg`             | `#fdf6e3` | `#002b36` | Page background                           |
| `bg-alt`         | `#eee8d5` | `#073642` | Sidebars, cards                           |
| `fg`             | `#586e75` | `#93a1a1` | Body text                                 |
| `fg-strong`      | `#073642` | `#eee8d5` | Headings                                  |
| `fg-muted`       | `#93a1a1` | `#657b83` | Secondary text, never for body copy       |
| `border`         | `#e0d8c0` | `#0d4654` | Rules and outlines                        |
| `primary`        | `#cb4b16` | `#e8692f` | Brand orange: logo, buttons, large text   |
| `primary-strong` | `#a33a0f` | `#f28c55` | Hover and pressed states                  |
| `accent`         | `#b58900` | `#d5a520` | Highlights                                |
| `link`           | `#a33a0f` | `#f28c55` | Links                                     |
| `code-bg`        | `#eee8d5` | `#073642` | Code blocks                               |
| `success`        | `#859900` | `#a3b81a` | Status: success                           |
| `danger`         | `#dc322f` | `#ef5a57` | Status: errors                            |
| `info`           | `#268bd2` | `#4aa3e0` | Status: information                       |

Body text (`fg`) and links meet WCAG AA contrast (4.5:1) on `bg` in both themes. `primary` sits just
below that in light mode, so use it for large text, icons and fills, and `link` for small text.

## Changing the brand

1. Edit `palette.json`.
2. Run `branding/palette.sh` to regenerate `palette.css`.
3. Replace `logo.svg` or `favicon.svg` if needed, keeping the file names.

The docs build checks that `palette.css` matches `palette.json`.

## Mascot

The mascot will be a mammoth. It is not drawn yet; `logo.svg` is a placeholder wordmark.

The mammoth is a nod to Gradle's elephant, but it must be clearly our own: a different silhouette,
pose and colors, with no tracing or reuse of Gradle's artwork. Gradle's elephant logo is a
registered trademark, and Gradle's
[trademark guidelines](https://gradle.com/legal/gradle-trademark-usage-guidelines/) don't allow
using it, or anything easily confused with it, without written permission. When mentioning Gradle,
use its name in plain text and don't imply endorsement.

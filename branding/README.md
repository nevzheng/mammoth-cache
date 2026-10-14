# Branding

Everything that defines how mammoth-cache looks lives in this folder. The docs site reads its
colors, logo and favicon from here, so rebranding means editing these files only.

| File                                 | What it is                                                   |
| ------------------------------------ | ------------------------------------------------------------ |
| `palette.json`                       | Color tokens for light and dark themes. The source of truth. |
| `palette.css`                        | The tokens as CSS custom properties. Generated; do not edit. |
| `palette.sh`                         | Regenerates `palette.css` from `palette.json` (needs `jq`).  |
| `logo*.svg`, `mark*.svg`, `favicon*` | The mascot and logo; see [Logo variants](#logo-variants).    |
| `social-preview.*`                   | 1280×640 card for link previews (GitHub social preview).     |

## Logo variants

The mascot is a mammoth, drawn as a flat silhouette with a peaked head dome, sloping back, shaggy
fringe and a big curved tusk.

| File                  | Use for                                            | Background           |
| --------------------- | -------------------------------------------------- | -------------------- |
| `logo.svg`            | Mark plus "mammoth" wordmark: README, site header  | Light                |
| `logo-dark.svg`       | The same lockup in the dark-mode orange            | Dark                 |
| `mark.svg`            | App icon, avatars: cream mammoth on orange square  | Any                  |
| `mark-dark.svg`       | App icon for dark surfaces: orange on dark square  | Dark                 |
| `mark-silhouette.svg` | Mammoth only, brand orange                         | Light or transparent |
| `mark-mono.svg`       | Mammoth only, `currentColor`; tint it with CSS     | Any (print, badges)  |
| `favicon.svg`         | Browser tabs; `mark.svg` without the eye           | Any                  |
| `favicon-32.png`      | PNG fallback for `favicon.svg`                     | Any                  |
| `social-preview.png`  | Upload in the repository settings (Social preview) | n/a                  |

The docs site shows `logo.svg` in light themes and `logo-dark.svg` in dark ones, and uses the
favicon. The root README switches between the two logos with the reader's color scheme.

## Palette

`palette.css` defines each token three ways: `--mc-light-<token>` and `--mc-dark-<token>` stay
fixed, for sites with their own theme switcher (the docs site uses these), and `--mc-<token>`
follows the reader's OS light or dark preference.

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
3. Replace the logo files if needed, keeping the file names.

The docs build checks that `palette.css` matches `palette.json`.

## Keep it our own

The mammoth nods to Gradle's elephant but must stay clearly distinct from it. Gradle's logo is a
registered trademark, and its
[trademark guidelines](https://gradle.com/legal/gradle-trademark-usage-guidelines/) don't allow
look-alikes without written permission. The mammoth was drawn from scratch, not traced or adapted
from Gradle's artwork. Keep any changes within these rules:

- **Unmistakably a mammoth:** high domed head, back sloping down to the hips, shaggy fur and big
  curved tusks.
- **Faces left**, in its own pose.
- **Our palette only:** colors from `palette.json`, never Gradle's.
- **No reuse** of Gradle's shapes, outlines or proportions.

When mentioning Gradle, use its name in plain text and don't imply endorsement.

Two earlier concepts (a walking mammoth carrying a cache block, and a front-facing badge) were
dropped; they remain in the history of
[pull request #1](https://github.com/nevzheng/mammoth-cache/pull/1).

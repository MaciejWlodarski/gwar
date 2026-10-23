# Gwar brand

## Logo

The logo is the lowercase wordmark **gwar**. The "w" is a voice wave in the
brand gradient; the other letters take the text colour.

| File | Use |
| --- | --- |
| `gwar-logo-on-dark.svg` | white letters, gradient "w"; dark backgrounds |
| `gwar-logo-on-light.svg` | graphite letters, gradient "w"; light backgrounds |
| `gwar-logo-mono-white.svg` | one colour, for photos and dark prints |
| `gwar-logo-mono-black.svg` | one colour, for light prints |

The letters are spaced by equal gaps (g–w, w–a, a–r). Keep clear space of at
least the height of the "a" counter around the wordmark, and do not stretch,
recolour the letters individually or add effects.

## Mark

The "w" alone is the mark. It is the app icon, the favicon and the tray icon,
and it stands in wherever the wordmark would be too small.

| File | Use |
| --- | --- |
| `gwar-mark.svg` | the gradient "w" on a transparent background |
| `gwar-app-icon.svg` | 1024 × 1024 app icon (graphite rounded square, macOS grid); the desktop icons in `apps/desktop/src-tauri/icons` are rendered from it with `pnpm tauri icon` |
| `favicon.svg` | the web favicon (`apps/web/public/favicon.svg`) |

## Colours

Brand gradient (the "w", left to right): `#6CEBC8` → `#34D3CF` → `#2EA8FF`.

The app's themes live in `apps/web/src/index.css`: a graphite ramp with a
slight teal cast, one teal accent, and the brand gradient for voice (the
talking ring and the mic level). Main tokens:

| Token | Dark | Light |
| --- | --- | --- |
| `--rail` (background) | `#0C0F10` | `#E6EBEB` |
| `--surface` | `#161B1C` | `#FFFFFF` |
| `--fg` | `#E8EEED` | `#121A1A` |
| `--accent` | `#3FCFC0` | `#097D74` |
| `--accent-strong` | `#22B3A5` | `#076A62` |
| `--accent-fg` (text on accent) | `#06201D` | `#FFFFFF` |
| `--talk-1 … --talk-3` (talking ring) | brand gradient | `#22C79C` → `#14B3B0` → `#1F8FE0` |
| `--ok` | `#6FCF97` | `#157F4B` |
| `--warn` | `#E8B25C` | `#9A6508` |
| `--danger` | `#F06A72` | `#C93A45` |

Text and accent colours meet WCAG AA (4.5:1) on their surfaces in both
themes; `--subtle` is for secondary labels only.

# Dashboard assets

Everything the server-rendered dashboard links to, embedded into `bentod`
with `rust-embed` and served under `/assets/` (SPEC 14.1). No build step:
edit a file here, rebuild the binary.

| File | What | Version | Source |
| --- | --- | --- | --- |
| `css/basecoat-lyra.min.css` | Basecoat, Lyra style pack, precompiled (no Tailwind needed) | basecoat-css 1.0.2 | `npm pack basecoat-css@1.0.2`, `dist/basecoat-lyra.cdn.min.css` |
| `js/basecoat.min.js`, `js/toast.min.js`, `js/dropdown-menu.min.js`, `js/select.min.js`, `js/combobox.min.js` | Basecoat runtime, toast, dropdown menu, select, combobox | basecoat-css 1.0.2 | same package, `dist/js/` |
| `js/htmx.min.js` | HTMX | 2.0.10 | `https://cdn.jsdelivr.net/npm/htmx.org@2.0.10/dist/htmx.min.js` |
| `js/uplot.min.js`, `css/uplot.min.css` | uPlot | 1.6.32 | `https://cdn.jsdelivr.net/npm/uplot@1.6.32/dist/` |
| `css/app.css` | Bento's tokens (Catppuccin Latte/Mocha, blue accent) and layout | — | this repository |
| `js/app.js` | Theme switch, charts, steppers, dialogs | — | this repository |
| `fonts/*.woff2` | IBM Plex Sans and Mono, latin subset (SPEC 14.3) | @fontsource 5.2.5 | previously vendored through `@fontsource/ibm-plex-*` |
| `branding/` | Favicon and wordmark | — | `branding/` at the repository root |

The Catppuccin tokens in `css/app.css` come from
`catppuccin/shadcn-ui` (`themes/{latte,mocha}/*-mauve.css`, commit
4435e5c), with `--primary` switched from mauve to blue. That theme is in the older shadcn format, bare HSL triplets;
Basecoat reads each token as a finished color, so the values here are
wrapped in `hsl()`. Keep this file as the committed token set (SPEC 19).

Basecoat's other components (dropdown menu, select, tabs, sidebar, ...)
are CSS-only as used here or not used. Copy the matching `dist/js/*.min.js`
into `js/` and load it from `crates/api/templates/layout.html` when a
page needs one of the JavaScript-backed components.

## Pick lists and autocomplete

Use Basecoat's `select` for a fixed list and its `combobox` for a typed
search over a list, never a native `<select>` or a `<datalist>`: the
browser draws those two itself and takes no styling. Both components
submit through a hidden input; give the combobox's visible input a name
of its own (`user_text` on the sharing form) and let the handler fall
back to it, so the form still works before JavaScript loads.

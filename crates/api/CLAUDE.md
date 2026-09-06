# crates/api

The JSON API (`src/*.rs`) and the server-rendered dashboard (`src/pages`, `templates/`) share one `Config` and the same adapters. Assets live in `../dashboard/assets`; that README records vendored versions.

## Pages

* A page handler and a JSON handler call the same adapter. Never duplicate logic.
* Handlers render templates. No HTML strings in Rust beyond the two fallbacks in `pages/mod.rs`.
* Every form works without JavaScript. HTMX adds boosted navigation and polling on top.
* A polled fragment sets `hx-target="this"`. HTMX inherits `hx-target` down the tree, and a missing one replaces the whole page.
* A stranger gets 404. A sharer who tries to write gets 403.
* Add a test in `src/pages/tests.rs` for every new route. Page tests reuse the fixture in `src/tests.rs`.

## Templates

* Basecoat components only. `select` for a pick list, `combobox` for a typed lookup. Never a native `<select>` or `<datalist>`.
* Destructive actions use `data-variant="destructive"`.
* Color never carries state alone. Pair it with a label.
* Placeholder data shows the "sample data" badge, driven by `placeholder: true` from `Metrics`.
* Copy is short. Say what the user can do, not why: "You cannot share this VM."
* Tokens are in `../dashboard/assets/css/app.css`. The accent is blue. Change it there only.
* Minijinja writes `/` as an HTML entity. Do not assert on a path inside rendered HTML.

## Checking

Run the dev server (root `CLAUDE.md`), then `node ../dashboard/dev/check.js` from a directory with Playwright installed. Look at the screenshots. A passing script is not a passing page.

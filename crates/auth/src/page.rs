//! The HTML the control plane writes itself.
//!
//! The dashboard is a separate single-page application, built by Vite and
//! embedded as static assets. These pages exist outside it because they
//! are reached without a session -- the sign-in landing page and the SSH
//! key-linking page -- so they cannot rely on a bundle that assumes one,
//! and they must render without a Node build in the loop.

pub(crate) fn escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            other => escaped.push(other),
        }
    }
    escaped
}

/// The shared frame. The dashboard is a separate single-page application;
/// these pages are the only HTML the control plane writes itself, so they
/// carry their own styling rather than pulling in that build.
pub(crate) fn page(title: &str, body: &str) -> String {
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="robots" content="noindex">
<title>{title} &middot; bento</title>
<style>
:root {{ color-scheme: light dark; --bg: #eff1f5; --fg: #4c4f69; --muted: #6c6f85;
         --card: #ffffff; --line: #ccd0da; --accent: #1e66f5; --accent-fg: #ffffff; }}
@media (prefers-color-scheme: dark) {{
  :root {{ --bg: #1e1e2e; --fg: #cdd6f4; --muted: #a6adc8;
           --card: #313244; --line: #45475a; --accent: #89b4fa; --accent-fg: #1e1e2e; }}
}}
* {{ box-sizing: border-box; }}
body {{ margin: 0; min-height: 100vh; display: flex; align-items: center; justify-content: center;
        padding: 1.5rem; background: var(--bg); color: var(--fg);
        font: 16px/1.5 ui-sans-serif, system-ui, -apple-system, "Segoe UI", sans-serif; }}
main {{ width: 100%; max-width: 30rem; background: var(--card); border: 1px solid var(--line);
        border-radius: 12px; padding: 1.75rem; }}
h1 {{ font-size: 1.25rem; margin: 0 0 0.75rem; }}
p {{ margin: 0 0 1rem; }}
dl {{ margin: 0 0 1.5rem; }}
dt {{ font-size: 0.8125rem; color: var(--muted); margin-top: 0.875rem; }}
dd {{ margin: 0.125rem 0 0; font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
      font-size: 0.875rem; overflow-wrap: anywhere; }}
.muted {{ color: var(--muted); font-size: 0.875rem; }}
button, .button {{ display: inline-block; font: inherit; font-weight: 600;
          padding: 0.625rem 1.25rem; border: 0; border-radius: 8px; text-decoration: none;
          background: var(--accent); color: var(--accent-fg); cursor: pointer; }}
.actions {{ display: flex; align-items: center; gap: 1rem; }}
a {{ color: var(--accent); }}
a.button {{ color: var(--accent-fg); }}
code {{ font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: 0.9em; }}
</style>
</head>
<body><main>
{body}
</main></body>
</html>
"#
    )
}

/// The sign-in page: the wordmark on the accent to the left with a few
/// cards about Bento under it, and the sign-in button to the right. On a
/// phone it is a band with the wordmark, the button, then the cards.
/// Colors are the dashboard's Catppuccin tokens, Latte and Mocha by the
/// OS preference. It carries its own styling, since it is served without
/// a session; the wordmark and font come from the dashboard's ungated
/// assets.
/// The sign-in page as a browser sees it, for previews outside the gate.
pub fn sign_in_page(provider_name: Option<&str>, base_domain: &str) -> String {
    let label = match provider_name {
        Some(name) if !name.is_empty() => format!("Sign in with {}", escape(name)),
        _ => "Sign in".to_string(),
    };
    splash(&label, base_domain)
}

/// The release's codename, shown beside the version on the sign-in page.
pub const CODENAME: &str = "Katsu";

pub(crate) fn splash(button_label: &str, base_domain: &str) -> String {
    let domain = escape(base_domain);
    let version = env!("CARGO_PKG_VERSION");
    let codename = CODENAME;
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="robots" content="noindex">
<title>Sign in &middot; bento</title>
<link rel="icon" type="image/svg+xml" href="/assets/branding/favicon.svg">
<link rel="alternate icon" type="image/png" href="/assets/branding/favicon.png">
<style>
@font-face {{ font-family: "IBM Plex Sans"; font-weight: 400; font-display: swap; src: url("/assets/fonts/ibm-plex-sans-latin-400-normal.woff2") format("woff2"); }}
@font-face {{ font-family: "IBM Plex Sans"; font-weight: 500; font-display: swap; src: url("/assets/fonts/ibm-plex-sans-latin-500-normal.woff2") format("woff2"); }}
@font-face {{ font-family: "IBM Plex Mono"; font-weight: 600; font-display: swap; src: url("/assets/fonts/ibm-plex-mono-latin-600-normal.woff2") format("woff2"); }}
:root {{ color-scheme: light dark; --background: #eff1f5; --foreground: #4c4f69; --muted: #6c6f85; --border: #bcc0cc; --primary: #1e66f5; --primary-foreground: #eff1f5; }}
@media (prefers-color-scheme: dark) {{
  :root {{ --background: #1e1e2e; --foreground: #cdd6f4; --muted: #a6adc8; --border: #45475a; --primary: #89b4fa; --primary-foreground: #1e1e2e; }}
}}
* {{ box-sizing: border-box; }}
html, body {{ min-height: 100%; margin: 0; }}
body {{ min-height: 100vh; display: grid; grid-template-columns: 2fr 3fr; grid-template-rows: 1fr auto 1fr; grid-template-areas: "top main" "brand main" "cards main" "bottom main"; grid-template-rows: 1fr auto auto 1fr;
        background: var(--background); color: var(--foreground); font: 400 14px/1.5 "IBM Plex Sans", ui-sans-serif, system-ui, sans-serif; }}
body::before {{ content: ""; grid-area: top; background: var(--primary); }}
body::after {{ content: ""; grid-area: bottom; background: var(--primary); }}
.brand {{ grid-area: brand; display: grid; place-items: center; background: var(--primary); color: var(--primary-foreground); padding: 2rem 2rem 1rem; }}
.brand span {{ display: block; width: min(60%, 18rem); aspect-ratio: 722 / 136; background: currentColor;
              -webkit-mask: url("/assets/branding/wordmark.png") no-repeat center / contain; mask: url("/assets/branding/wordmark.png") no-repeat center / contain; }}
.cards {{ grid-area: cards; display: grid; gap: 1rem; align-content: start; padding: 2rem; background: var(--primary); }}
.card {{ background: var(--background); color: var(--foreground); border: 1px solid var(--border); padding: 1rem; display: grid; gap: 0.25rem; }}
.card h2 {{ margin: 0; font: 600 0.95rem/1.3 "IBM Plex Mono", ui-monospace, monospace; }}
.card p {{ margin: 0; color: var(--muted); }}
.card code {{ font-family: "IBM Plex Mono", ui-monospace, monospace; font-size: 0.9em; color: var(--foreground); }}
main {{ grid-area: main; display: grid; grid-template-rows: 1fr auto; }}
.center {{ display: grid; place-items: center; padding: 2rem; }}
footer {{ padding: 1rem 2rem; text-align: center; font-size: 0.8rem; color: var(--muted); }}
footer a {{ color: inherit; }}
footer a:hover {{ color: var(--primary); }}
.btn {{ display: inline-flex; align-items: center; justify-content: center; width: min(100%, 26rem); height: 2.5rem; padding: 0 1rem;
        border: 1px solid var(--primary); background: var(--primary); color: var(--primary-foreground); font: inherit; font-weight: 500; text-decoration: none; }}
.btn:hover {{ filter: brightness(1.08); }}
.btn:focus-visible {{ outline: 2px solid var(--primary); outline-offset: 2px; }}
@media (max-width: 720px) {{
  body {{ grid-template-columns: 1fr; grid-template-rows: auto auto 1fr auto; grid-template-areas: "brand" "main" "cards" "foot"; }}
  main {{ display: contents; }}
  .center {{ grid-area: main; align-items: start; padding: 1.5rem; }}
  footer {{ grid-area: foot; }}
  body::before, body::after {{ display: none; }}
  .brand {{ padding: 2.5rem 1.5rem; }}
  .brand span {{ width: min(50%, 12rem); }}
  .btn {{ width: 100%; }}
  .cards {{ background: var(--background); padding: 0 1.5rem 2rem; }}
}}
</style>
</head>
<body>
<aside class="brand" aria-hidden="true"><span></span></aside>
<main>
<div class="center"><a class="btn" href="/login">{button_label}</a></div>
<footer>v{version} &ldquo;{codename}&rdquo; &middot; Made with love by the bento team &lt;3 &middot; <a href="https://github.com/abbyfluoroethane/bento/blob/main/LICENSE">MIT License</a></footer>
</main>
<section class="cards" aria-label="About Bento">
  <div class="card"><h2>One binary</h2><p>The control plane, the HTTPS proxy, and the SSH front end ship as a single Rust program.</p></div>
  <div class="card"><h2>Your name is your address</h2><p>Every VM answers at <code>name.{domain}</code> over HTTPS, with the certificate handled for you.</p></div>
  <div class="card"><h2>SSH does everything</h2><p><code>ssh {domain} new &lt;name&gt;</code> creates a machine; every action here has a command-line twin.</p></div>
</section>
</body>
</html>
"#
    )
}

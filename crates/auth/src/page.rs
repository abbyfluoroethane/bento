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

use bytes::Bytes;
use http::{Response, StatusCode, header};

use crate::{ProxyBody, full_body};
use bento_types::{Instance, State};

// The 404 and 503 pages are static HTML with inline CSS and no JavaScript
// (SPEC 14.5). They use the Catppuccin palette: Latte in light mode, Mocha
// in dark mode (SPEC 14.2). The font stacks name the self-hosted dashboard
// fonts first so a browser that has them applies them, with system
// fallbacks so the page renders when the dashboard bundle is unavailable.
//
// State colors follow SPEC 14.2: running green, starting yellow, stopped
// overlay1, error red. The color sits on a border and a dot, never alone:
// the state text is the label, and the text keeps the normal text color so
// Latte yellow never carries text (SPEC 14.2 contrast note).
macro_rules! page_css {
    () => {
        r#":root {
  --base: #eff1f5;
  --mantle: #e6e9ef;
  --surface: #ccd0da;
  --text: #4c4f69;
  --subtext: #6c6f85;
  --mauve: #8839ef;
  --green: #40a02b;
  --yellow: #df8e1d;
  --red: #d20f39;
  --overlay1: #8c8fa1;
}
@media (prefers-color-scheme: dark) {
  :root {
    --base: #1e1e2e;
    --mantle: #181825;
    --surface: #313244;
    --text: #cdd6f4;
    --subtext: #a6adc8;
    --mauve: #cba6f7;
    --green: #a6e3a1;
    --yellow: #f9e2af;
    --red: #f38ba8;
    --overlay1: #7f849c;
  }
}
* { margin: 0; padding: 0; box-sizing: border-box; }
body {
  background: var(--base);
  color: var(--text);
  font-family: "IBM Plex Sans", ui-sans-serif, system-ui, sans-serif;
  min-height: 100vh;
  display: flex;
  align-items: center;
  justify-content: center;
  padding: 2rem;
}
main {
  background: var(--mantle);
  border: 1px solid var(--surface);
  border-radius: 8px;
  padding: 2.5rem 3rem;
  max-width: 34rem;
}
.code {
  font-family: "IBM Plex Mono", ui-monospace, monospace;
  color: var(--mauve);
  font-size: 0.9rem;
  letter-spacing: 0.1em;
}
h1 {
  font-family: "IBM Plex Mono", ui-monospace, monospace;
  font-size: 1.4rem;
  font-weight: 600;
  margin: 0.4rem 0 1rem;
}
p { margin: 0.5rem 0; line-height: 1.5; }
code, .state {
  font-family: "IBM Plex Mono", ui-monospace, monospace;
}
.state {
  display: inline-flex;
  align-items: center;
  gap: 0.5em;
  margin-top: 0.75rem;
  padding: 0.3em 0.8em;
  border: 1px solid var(--overlay1);
  border-left: 4px solid var(--overlay1);
  border-radius: 4px;
}
.state .dot {
  width: 0.6em;
  height: 0.6em;
  border-radius: 50%;
  background: var(--overlay1);
}
.state.running { border-color: var(--green); }
.state.running .dot { background: var(--green); }
.state.starting { border-color: var(--yellow); }
.state.starting .dot { background: var(--yellow); }
.state.stopped { border-color: var(--overlay1); }
.state.stopped .dot { background: var(--overlay1); }
.state.error { border-color: var(--red); }
.state.error .dot { background: var(--red); }
.hint { color: var(--subtext); font-size: 0.85rem; margin-top: 1.25rem; }"#
    };
}

// Every 404 the proxy serves — a name that never existed, a name in
// cooldown, an instance with visibility off — uses this single body, so
// the responses are byte-identical by construction (SPEC 9.2). The page
// names nothing.
const NOT_FOUND_PAGE: &str = concat!(
    r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>404 · bento</title>
<style>"#,
    page_css!(),
    r#"</style>
</head>
<body>
<main>
<p class="code">404</p>
<h1>nothing here</h1>
<p>This name does not point to anything.</p>
</main>
</body>
</html>
"#
);

pub(crate) fn not_found() -> Response<ProxyBody> {
    response(StatusCode::NOT_FOUND, NOT_FOUND_PAGE, false)
}

/// Builds the 503 page from SPEC 9.3 and 14.5. It names the instance and
/// state, never the owner. The meta refresh matches the `Retry-After`
/// header without JavaScript.
pub(crate) fn unavailable(instance: &Instance) -> Response<ProxyBody> {
    let state = instance.state.as_str();
    let class = state_class(instance.state);
    let name = escape_html(&instance.name);
    let mut body = String::from(concat!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta http-equiv="refresh" content="5">
<title>503 · bento</title>
<style>"#,
        page_css!(),
        r#"</style>
</head>
<body>
<main>
<p class="code">503</p>
<h1>instance unavailable</h1>
<p>The instance <code>"#
    ));
    body.push_str(&name);
    body.push_str("</code> is not ready.</p>\n<p class=\"state ");
    body.push_str(class);
    body.push_str("\"><span class=\"dot\"></span>");
    body.push_str(state);
    body.push_str(
        r#"</p>
<p class="hint">This page refreshes every 5 seconds.</p>
</main>
</body>
</html>
"#,
    );
    response(StatusCode::SERVICE_UNAVAILABLE, body, true)
}

fn response(status: StatusCode, body: impl Into<Bytes>, retry: bool) -> Response<ProxyBody> {
    let mut response = Response::new(full_body(body));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        http::HeaderValue::from_static("text/html; charset=utf-8"),
    );
    headers.insert(
        header::CACHE_CONTROL,
        http::HeaderValue::from_static("no-store"),
    );
    if retry {
        headers.insert(header::RETRY_AFTER, http::HeaderValue::from_static("5"));
    }
    response
}

// Maps an observed state to its CSS class per the SPEC 14.2 state-to-color
// table. Anything unknown renders as an error.
fn state_class(state: State) -> &'static str {
    match state {
        State::Running => "running",
        State::Starting => "starting",
        State::Stopped => "stopped",
    }
}

fn escape_html(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&#34;"),
            '\'' => escaped.push_str("&#39;"),
            other => escaped.push(other),
        }
    }
    escaped
}

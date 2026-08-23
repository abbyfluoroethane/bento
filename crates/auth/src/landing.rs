//! What a signed-out visitor to the base domain gets (SPEC 13, 14).
//!
//! The dashboard bundle assumes a session: it opens straight into API
//! calls and has nothing to say about signing in. Serving it to a stranger
//! produces a dashboard-shaped page full of failed requests, so requests
//! that would reach it are intercepted here.
//!
//! A visitor who is already signed in to the identity provider should not
//! have to click anything, so the first such request is answered with a
//! silent `prompt=none` probe: the provider either bounces straight back
//! with a code, or reports `login_required` and the splash is rendered
//! instead. Whichever way it lands, the outcome is recorded in a
//! short-lived cookie so a signed-out visitor is sent to the provider once
//! rather than on every page load.

use http::header::HeaderValue;
use http::{HeaderMap, StatusCode};

use crate::page::{escape, page};
use crate::session::host_cookie;
use crate::{Service, append_cookie, cookie_value, html_response, redirect_response};

/// Marks a login flow as the silent probe, so the callback knows a refusal
/// is the expected answer rather than something to report.
pub(crate) const SILENT_COOKIE_NAME: &str = "bento_sso_silent";

/// Records that the probe already ran and came back empty-handed.
pub(crate) const PROBED_COOKIE_NAME: &str = "bento_sso_probed";

/// How long a fruitless probe is remembered. Long enough that reloading
/// the page does not bounce off the provider again, short enough that
/// signing in elsewhere is noticed on the next visit.
const PROBE_INTERVAL_SECONDS: i64 = 10 * 60;

impl Service {
    /// Decides whether a request may reach the dashboard bundle.
    ///
    /// `Some` is the response to send instead. `None` means the caller
    /// should serve the dashboard as usual.
    pub async fn dashboard_gate(&self, headers: &HeaderMap) -> Option<crate::HttpResponse> {
        if self.session_from_headers(headers).await.is_some() {
            return None;
        }
        let probe_is_useful = self.oidc().is_some()
            && cookie_value(headers, PROBED_COOKIE_NAME).is_none()
            // A crawler or a `curl` gains nothing from a round trip to the
            // provider, and would never keep the cookie that stops the
            // next one.
            && accepts_html(headers);
        if probe_is_useful {
            return Some(self.silent_login_response());
        }
        Some(html_response(StatusCode::OK, self.splash_page()))
    }

    /// Starts a `prompt=none` authorization request. It reuses the normal
    /// flow's state and nonce cookies, so the callback validates it
    /// exactly like a login the visitor asked for.
    fn silent_login_response(&self) -> crate::HttpResponse {
        let Some(oidc) = self.oidc() else {
            return html_response(StatusCode::OK, self.splash_page());
        };
        let oauth = &oidc.exchanger;
        let state = crate::random_token();
        let nonce = crate::random_token();
        let mut response = redirect_response(&oauth.auth_code_url_silent(&state, &nonce));
        crate::oidc::attach_flow_cookies(&mut response, &state, &nonce, "/");
        append_cookie(&mut response, flag_cookie(SILENT_COOKIE_NAME, true));
        response
    }

    /// The page a visitor with no session anywhere gets.
    pub(crate) fn splash_page(&self) -> String {
        let domain = escape(&self.base_domain);
        page(
            "Sign in",
            &format!(
                r#"<h1><span aria-hidden="true">&#127857;</span> bento</h1>
<p>Small virtual machines on one host, with a web dashboard and an
<code>ssh</code> command line.</p>
<p class="actions"><a class="button" href="/login">Sign in</a></p>
<p class="muted">Signing in creates your account the first time. To use the
command line afterwards, run <code>ssh {domain}</code> and open the link it
prints to attach your SSH key.</p>"#
            ),
        )
    }

    /// The splash, plus the cookie that stops the probe repeating. The
    /// callback renders this when the provider says nobody is signed in.
    pub(crate) fn probe_failed_response(&self) -> crate::HttpResponse {
        let mut response = html_response(StatusCode::OK, self.splash_page());
        append_cookie(&mut response, flag_cookie(SILENT_COOKIE_NAME, false));
        append_cookie(&mut response, flag_cookie(PROBED_COOKIE_NAME, true));
        response
    }

    /// Clears both probe cookies, so the next signed-out visit probes
    /// again rather than going straight to the splash.
    pub(crate) fn clear_probe_cookies(&self, response: &mut crate::HttpResponse) {
        append_cookie(response, flag_cookie(SILENT_COOKIE_NAME, false));
        append_cookie(response, flag_cookie(PROBED_COOKIE_NAME, false));
    }

    /// Stops the landing page probing on the next visit. Logout uses this
    /// so that signing out is not immediately undone by the provider's
    /// own session.
    pub(crate) fn suppress_next_probe(&self, response: &mut crate::HttpResponse) {
        append_cookie(response, flag_cookie(PROBED_COOKIE_NAME, true));
    }
}

fn flag_cookie(name: &str, set: bool) -> HeaderValue {
    if set {
        host_cookie(name, "1", PROBE_INTERVAL_SECONDS)
    } else {
        host_cookie(name, "", -1)
    }
}

/// Whether the request looks like a browser asking for a page. A missing
/// `Accept` header is treated as a no: every browser sends one.
fn accepts_html(headers: &HeaderMap) -> bool {
    headers
        .get(http::header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("text/html"))
}

/// Reports whether the request is for a built dashboard asset rather than
/// a page. Assets are content-addressed and carry no session meaning, so
/// they are served to anyone; gating them would break the splash's own
/// styling if it ever grew an external file, and would put HTML in front
/// of a request that asked for JavaScript.
pub fn is_dashboard_asset(path: &str) -> bool {
    path.rsplit('/')
        .next()
        .is_some_and(|segment| segment.contains('.'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        FakeClock, TEST_EPOCH, new_oidc_service, new_oidc_service_with_signups, new_test_service,
    };
    use http::header::{ACCEPT, COOKIE, LOCATION, SET_COOKIE};

    fn browser() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT, HeaderValue::from_static("text/html,*/*;q=0.8"));
        headers
    }

    fn cookies(response: &crate::HttpResponse) -> Vec<String> {
        response
            .headers()
            .get_all(SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok().map(str::to_owned))
            .collect()
    }

    #[tokio::test]
    async fn a_signed_in_visitor_reaches_the_dashboard() {
        let test = new_oidc_service();
        let session = test.service.new_session(42).await.unwrap();
        let mut headers = browser();
        headers.insert(
            COOKIE,
            HeaderValue::from_str(&format!("{}={}", crate::SESSION_COOKIE_NAME, session.id))
                .unwrap(),
        );
        assert!(test.service.dashboard_gate(&headers).await.is_none());
    }

    #[tokio::test]
    async fn a_first_visit_probes_the_provider_silently() {
        let test = new_oidc_service();
        let response = test.service.dashboard_gate(&browser()).await.unwrap();
        assert_eq!(response.status(), StatusCode::FOUND);
        let location = response.headers()[LOCATION].to_str().unwrap();
        assert!(location.starts_with("https://id.example.org/authorize"));
        // Without this the provider would show its own login form inside
        // what was meant to be an invisible check.
        assert!(location.contains("prompt=none"), "{location}");
        let set = cookies(&response);
        assert!(set.iter().any(|c| c.starts_with(SILENT_COOKIE_NAME)));
    }

    #[tokio::test]
    async fn a_visitor_the_provider_did_not_recognise_gets_the_splash_once() {
        let test = new_oidc_service();
        let refused = test.service.probe_failed_response();
        assert_eq!(refused.status(), StatusCode::OK);
        assert!(refused.body().contains("Sign in"));

        // The cookie it set must stop the next visit probing again, or
        // every page load bounces off the provider.
        let mut headers = browser();
        headers.insert(
            COOKIE,
            HeaderValue::from_str(&format!("{PROBED_COOKIE_NAME}=1")).unwrap(),
        );
        let response = test.service.dashboard_gate(&headers).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.body().contains("Sign in"));
    }

    #[tokio::test]
    async fn a_non_browser_is_never_sent_to_the_provider() {
        let test = new_oidc_service();
        // No Accept header: curl, a crawler, a health check. None of them
        // would keep the cookie that stops the probe repeating.
        let response = test
            .service
            .dashboard_gate(&HeaderMap::new())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.body().contains("bento"));
    }

    #[tokio::test]
    async fn without_oidc_the_splash_is_all_there_is() {
        let clock = FakeClock::new(TEST_EPOCH);
        let (service, _, _, _) = new_test_service(&clock);
        let response = service.dashboard_gate(&browser()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.body().contains("Sign in"));
    }

    #[tokio::test]
    async fn the_splash_names_the_deployment_and_escapes_it() {
        let test = new_oidc_service_with_signups();
        let body = test.service.splash_page();
        assert!(body.contains("ssh bento.example.org"), "{body}");
        assert!(body.contains(r#"href="/login""#));
    }

    /// Signing out of Bento leaves the provider's session alone, so a
    /// probe straight afterwards would sign the visitor back in and the
    /// logout would appear to do nothing.
    #[tokio::test]
    async fn logging_out_is_not_undone_by_the_probe() {
        let test = new_oidc_service();
        let session = test.service.new_session(42).await.unwrap();
        let mut headers = browser();
        headers.insert(
            COOKIE,
            HeaderValue::from_str(&format!("{}={}", crate::SESSION_COOKIE_NAME, session.id))
                .unwrap(),
        );

        let response = test.service.logout_response(&headers).await;
        let held = cookies(&response)
            .iter()
            .filter_map(|cookie| cookie.split(';').next().map(str::to_owned))
            .collect::<Vec<_>>()
            .join("; ");

        let mut after = browser();
        after.insert(COOKIE, HeaderValue::from_str(&held).unwrap());
        let landing = test.service.dashboard_gate(&after).await.unwrap();
        assert_eq!(landing.status(), StatusCode::OK, "logout bounced back in");
        assert!(landing.body().contains("Sign in"));
    }

    #[test]
    fn assets_are_told_apart_from_pages() {
        for path in ["/assets/index-ab12.js", "/favicon.ico", "/a/b/style.css"] {
            assert!(is_dashboard_asset(path), "{path}");
        }
        for path in ["/", "/instances", "/instances/web"] {
            assert!(!is_dashboard_asset(path), "{path}");
        }
    }
}

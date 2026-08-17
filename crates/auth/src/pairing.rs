//! The SSH key-linking page (SPEC 13).
//!
//! An unknown key presented to the SSH frontend creates a short-lived
//! pairing and nothing else. This module turns the link the frontend
//! printed into a page: sign in, look at the fingerprint, confirm.
//!
//! The confirmation is a POST, not the click on the link. A link that
//! bound a key by being visited would be a working attack — send yours to
//! someone signed in and their instances are yours — and it would fire on
//! anything that follows links, from a mail scanner to a chat preview.
//! Making the browser send a POST means the session cookie's
//! `SameSite=Lax` decides: a cross-site form submission arrives without
//! it, and the request is refused for want of a session.
//!
//! The fingerprint is on the page and on the terminal that produced the
//! link. They are meant to be compared.

use async_trait::async_trait;
use bento_types::{Pairing, User};
use http::{HeaderMap, StatusCode};

use crate::page::{escape, page};
use crate::token::hash_token;
use crate::{BoxError, Service, html_response, redirect_response, text_response};

/// The path the SSH frontend builds its link from. The token follows.
pub const LINK_PATH_PREFIX: &str = "/link/";

/// Returns a fresh link token and its hash: the first goes in the URL and
/// is never written down, the second is all the store keeps (SPEC 13).
/// Minting lives here so the token's shape and its hash cannot drift apart.
pub fn new_link_token() -> (String, String) {
    let plaintext = crate::random_token();
    let hash = hash_token(&plaintext);
    (plaintext, hash)
}

/// Reads and claims pending SSH key links.
#[async_trait]
pub trait PairingStore: Send + Sync {
    /// Returns the pairing whose link token hashes to `token_hash`.
    ///
    /// Expired and already-used rows come back rather than reading as
    /// missing: the page says something different for each, and only the
    /// row can tell them apart.
    async fn pairing_by_token_hash(
        &self,
        token_hash: &str,
    ) -> std::result::Result<Option<Pairing>, BoxError>;

    /// Attaches the pairing's key to the user and marks the pairing used,
    /// atomically. `false` means the pairing was claimed or expired
    /// first, and nothing was written.
    async fn link_pairing(&self, id: i64, user_id: i64) -> std::result::Result<bool, BoxError>;
}

/// What a link token resolved to, once the session and the row are known.
enum Resolved {
    /// Ready to confirm, or to claim.
    Live(Box<(Pairing, User)>),
    /// The token names nothing, or names something no longer usable.
    Dead(&'static str),
    /// The request could not be answered at all.
    Broken(StatusCode, &'static str),
}

impl Service {
    /// Renders the confirmation page for a link token. An unauthenticated
    /// visitor is sent through login first and returns here, which is what
    /// makes a first-time SSH user's account come into existence.
    pub async fn link_page_response(
        &self,
        headers: &HeaderMap,
        token: &str,
    ) -> crate::HttpResponse {
        if self.session_from_headers(headers).await.is_none() {
            // Nothing is disclosed about the token before sign-in; the
            // round trip through the provider is what creates the account
            // for a user who has never been here (SPEC 13).
            return redirect_response(&format!(
                "{}?next={LINK_PATH_PREFIX}{}",
                self.login_path,
                urlencode(token)
            ));
        }
        match self.resolve_link(headers, token).await {
            Resolved::Live(live) => {
                let (pairing, user) = *live;
                html_response(StatusCode::OK, confirm_page(token, &pairing, &user))
            }
            Resolved::Dead(reason) => html_response(StatusCode::GONE, dead_page(reason)),
            Resolved::Broken(status, message) => text_response(status, format!("{message}\n")),
        }
    }

    /// Claims a link token for the session's user, attaching the key.
    pub async fn link_confirm_response(
        &self,
        headers: &HeaderMap,
        token: &str,
    ) -> crate::HttpResponse {
        let (pairing, user) = match self.resolve_link(headers, token).await {
            Resolved::Live(live) => *live,
            Resolved::Dead(reason) => return html_response(StatusCode::GONE, dead_page(reason)),
            Resolved::Broken(status, message) => {
                return text_response(status, format!("{message}\n"));
            }
        };
        let Some(pairings) = self.pairings.as_ref() else {
            return text_response(StatusCode::INTERNAL_SERVER_ERROR, "linking is not wired\n");
        };
        match pairings.link_pairing(pairing.id, user.id).await {
            Ok(true) => {
                tracing::info!(
                    user = %user.name,
                    fingerprint = %pairing.fingerprint,
                    "ssh key linked"
                );
                html_response(StatusCode::OK, linked_page(&pairing, &user))
            }
            // The row was live a moment ago, so this is the losing half of
            // two confirmations, or a link that expired mid-click.
            Ok(false) => html_response(
                StatusCode::GONE,
                dead_page("This link was already used, or it ran out of time."),
            ),
            Err(error) => {
                tracing::error!(error = %error, "linking an ssh key failed");
                text_response(StatusCode::INTERNAL_SERVER_ERROR, "linking failed\n")
            }
        }
    }

    /// Resolves a token against the session and the store, with every
    /// unusable outcome already turned into what the caller should say.
    async fn resolve_link(&self, headers: &HeaderMap, token: &str) -> Resolved {
        let Some(pairings) = self.pairings.as_ref() else {
            return Resolved::Broken(StatusCode::INTERNAL_SERVER_ERROR, "linking is not wired");
        };
        let Some(session) = self.session_from_headers(headers).await else {
            return Resolved::Broken(StatusCode::UNAUTHORIZED, "sign in and open the link again");
        };
        let user = match self.users.user_by_id(session.user_id).await {
            Ok(Some(user)) => user,
            Ok(None) => {
                return Resolved::Broken(StatusCode::UNAUTHORIZED, "this session has no account");
            }
            Err(error) => {
                tracing::error!(error = %error, "link page: user lookup failed");
                return Resolved::Broken(StatusCode::INTERNAL_SERVER_ERROR, "user lookup failed");
            }
        };
        let pairing = match pairings.pairing_by_token_hash(&hash_token(token)).await {
            Ok(Some(pairing)) => pairing,
            Ok(None) => {
                return Resolved::Dead(
                    "This link is not one Bento handed out, or it has been cleaned up.",
                );
            }
            Err(error) => {
                tracing::error!(error = %error, "link page: pairing lookup failed");
                return Resolved::Broken(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "pairing lookup failed",
                );
            }
        };
        if pairing.linked_user_id.is_some() {
            return Resolved::Dead("This link has already been used.");
        }
        if pairing.expires_at <= (self.now)() {
            return Resolved::Dead("This link has expired. Connect over SSH again for a new one.");
        }
        Resolved::Live(Box::new((pairing, user)))
    }
}

/// Percent-encodes a token for use in a query value. The tokens Bento
/// mints are URL-safe base64 already; this covers whatever else arrives.
fn urlencode(value: &str) -> String {
    const QUERY: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'_')
        .remove(b'.')
        .remove(b'~');
    percent_encoding::utf8_percent_encode(value, QUERY).to_string()
}

fn confirm_page(token: &str, pairing: &Pairing, user: &User) -> String {
    let comment = if pairing.comment.is_empty() {
        String::new()
    } else {
        format!("<dt>Comment</dt><dd>{}</dd>", escape(&pairing.comment))
    };
    page(
        "Link an SSH key",
        &format!(
            r#"<h1>Link this SSH key?</h1>
<p>Signing in over SSH with this key will give it the access of
<strong>{name}</strong>.</p>
<p class="muted">Check the fingerprint against the one your terminal printed.
If they differ, or you did not just run <code>ssh</code>, close this page.</p>
<dl>
<dt>Account</dt><dd>{name}</dd>
<dt>Fingerprint</dt><dd>{fingerprint}</dd>
{comment}
</dl>
<form method="post" action="{prefix}{token}" class="actions">
<button type="submit">Link this key</button>
<span class="muted">or <a href="/">cancel</a></span>
</form>"#,
            name = escape(&user.name),
            fingerprint = escape(&pairing.fingerprint),
            comment = comment,
            prefix = LINK_PATH_PREFIX,
            token = escape(token),
        ),
    )
}

fn linked_page(pairing: &Pairing, user: &User) -> String {
    page(
        "SSH key linked",
        &format!(
            r#"<h1>Key linked</h1>
<p>This key now signs in as <strong>{name}</strong>.</p>
<dl><dt>Fingerprint</dt><dd>{fingerprint}</dd></dl>
<p class="muted">The terminal you started from should say so too. Run
<code>ssh {name_plain}@…</code> against an instance, or open the
<a href="/">dashboard</a>.</p>"#,
            name = escape(&user.name),
            name_plain = escape(&user.name),
            fingerprint = escape(&pairing.fingerprint),
        ),
    )
}

fn dead_page(reason: &str) -> String {
    page(
        "Link not usable",
        &format!(
            r#"<h1>This link cannot be used</h1>
<p>{reason}</p>
<p class="muted">Links last a few minutes and work once. Connect with
<code>ssh</code> again to get a fresh one.</p>"#,
            reason = escape(reason),
        ),
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use http::header::{COOKIE, HeaderValue, LOCATION};
    use time::Duration;

    use super::*;
    use crate::SESSION_COOKIE_NAME;
    use crate::test_support::{FakeClock, FakePairingStore, TEST_EPOCH, new_test_service};

    struct Fixture {
        service: Service,
        pairings: Arc<FakePairingStore>,
        clock: FakeClock,
    }

    fn fixture() -> Fixture {
        let clock = FakeClock::new(TEST_EPOCH);
        let (service, users, _, _) = new_test_service(&clock);
        users.insert("subject-1", 42, "riley");
        let pairings = Arc::new(FakePairingStore::new());
        Fixture {
            service: service.with_pairings(pairings.clone()),
            pairings,
            clock,
        }
    }

    async fn signed_in(service: &Service, user_id: i64) -> HeaderMap {
        let session = service.new_session(user_id).await.unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            COOKIE,
            HeaderValue::from_str(&format!("{SESSION_COOKIE_NAME}={}", session.id)).unwrap(),
        );
        headers
    }

    #[tokio::test]
    async fn the_page_sends_a_signed_out_visitor_through_login_and_back() {
        let Fixture {
            service, pairings, ..
        } = fixture();
        pairings.insert("tok", TEST_EPOCH + Duration::minutes(3));
        let response = service.link_page_response(&HeaderMap::new(), "tok").await;
        assert_eq!(response.status(), StatusCode::FOUND);
        assert_eq!(response.headers()[LOCATION], "/login?next=/link/tok");
        // Nothing about the token leaks before sign-in.
        assert!(response.body().is_empty());
    }

    #[tokio::test]
    async fn confirming_links_the_key_once() {
        let Fixture {
            service, pairings, ..
        } = fixture();
        let pairing = pairings.insert("tok", TEST_EPOCH + Duration::minutes(3));
        let headers = signed_in(&service, 42).await;

        let page = service.link_page_response(&headers, "tok").await;
        assert_eq!(page.status(), StatusCode::OK);
        assert!(page.body().contains(&pairing.fingerprint));
        assert!(page.body().contains("riley"));
        assert!(
            page.body()
                .contains(r#"<form method="post" action="/link/tok""#)
        );
        // Rendering the page must not have linked anything.
        assert!(pairings.linked().is_empty());

        let confirm = service.link_confirm_response(&headers, "tok").await;
        assert_eq!(confirm.status(), StatusCode::OK);
        assert!(confirm.body().contains("Key linked"));
        assert_eq!(pairings.linked(), vec![(pairing.id, 42)]);

        let again = service.link_confirm_response(&headers, "tok").await;
        assert_eq!(again.status(), StatusCode::GONE);
        assert_eq!(pairings.linked().len(), 1);
    }

    #[tokio::test]
    async fn an_expired_link_is_refused_by_the_service_clock() {
        let Fixture {
            service,
            pairings,
            clock,
        } = fixture();
        pairings.insert("tok", TEST_EPOCH + Duration::minutes(3));
        let headers = signed_in(&service, 42).await;
        clock.advance(Duration::minutes(4));

        for response in [
            service.link_page_response(&headers, "tok").await,
            service.link_confirm_response(&headers, "tok").await,
        ] {
            assert_eq!(response.status(), StatusCode::GONE);
            assert!(response.body().contains("expired"));
        }
        assert!(pairings.linked().is_empty());
    }

    #[tokio::test]
    async fn an_unknown_token_links_nothing() {
        let Fixture {
            service, pairings, ..
        } = fixture();
        pairings.insert("tok", TEST_EPOCH + Duration::minutes(3));
        let headers = signed_in(&service, 42).await;
        let response = service.link_confirm_response(&headers, "forged").await;
        assert_eq!(response.status(), StatusCode::GONE);
        assert!(pairings.linked().is_empty());
    }

    #[tokio::test]
    async fn confirming_without_a_session_links_nothing() {
        let Fixture {
            service, pairings, ..
        } = fixture();
        pairings.insert("tok", TEST_EPOCH + Duration::minutes(3));
        // A cross-site POST arrives without the SameSite=Lax cookie, which
        // is exactly this case.
        let response = service
            .link_confirm_response(&HeaderMap::new(), "tok")
            .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(pairings.linked().is_empty());
    }

    #[test]
    fn rendered_pages_escape_what_the_client_chose() {
        let pairing = Pairing {
            id: 1,
            token_hash: "hash".into(),
            public_key: "ssh-ed25519 AAAA".into(),
            fingerprint: "SHA256:abc".into(),
            comment: "<script>alert(1)</script>".into(),
            created_at: TEST_EPOCH,
            expires_at: TEST_EPOCH,
            linked_user_id: None,
        };
        let user = User {
            id: 42,
            name: "riley".into(),
            email: "riley@example.org".into(),
            oidc_subject: Some("subject-1".into()),
            subnet: "10.100.0.0/24".into(),
            created_at: TEST_EPOCH,
        };
        // The comment is whatever the connecting client put in its key.
        let body = confirm_page("tok", &pairing, &user);
        assert!(!body.contains("<script>"));
        assert!(body.contains("&lt;script&gt;"));
    }

    #[test]
    fn a_key_with_no_comment_gets_no_empty_row() {
        let pairing = Pairing {
            id: 1,
            token_hash: "hash".into(),
            public_key: "ssh-ed25519 AAAA".into(),
            fingerprint: "SHA256:abc".into(),
            comment: String::new(),
            created_at: TEST_EPOCH,
            expires_at: TEST_EPOCH,
            linked_user_id: None,
        };
        let user = User {
            id: 42,
            name: "riley".into(),
            email: String::new(),
            oidc_subject: None,
            subnet: "10.100.0.0/24".into(),
            created_at: TEST_EPOCH,
        };
        assert!(!confirm_page("tok", &pairing, &user).contains("Comment"));
    }
}

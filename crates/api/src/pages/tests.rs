//! The pages over the same fakes as the API tests.

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use tower::ServiceExt;

use crate::tests::{TestResponse, fixture};

async fn get(app: &Router, path: &str) -> TestResponse {
    send(app, Method::GET, path, "", false).await
}

async fn post(app: &Router, path: &str, form: &str) -> TestResponse {
    send(app, Method::POST, path, form, false).await
}

async fn send(app: &Router, method: Method, path: &str, form: &str, hx: bool) -> TestResponse {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
    if hx {
        builder = builder.header("hx-request", "true");
    }
    let response = app
        .clone()
        .oneshot(builder.body(Body::from(form.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    TestResponse {
        status,
        headers,
        body,
    }
}

fn text(response: &TestResponse) -> String {
    String::from_utf8_lossy(&response.body).into_owned()
}

fn location(response: &TestResponse) -> String {
    response
        .headers
        .get(header::LOCATION)
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default()
}

#[tokio::test]
async fn home_lists_machines_quota_and_host_figures() {
    let fx = fixture();
    let response = get(&fx.pages, "/").await;
    assert_eq!(response.status, StatusCode::OK);
    let body = text(&response);
    assert!(body.contains("<title>Virtual Machines · Bento</title>"));
    // Alice's own machine and the one shared with her, sorted by name.
    let db = body.find("/vm/uuid-db").unwrap();
    let web = body.find("/vm/uuid-web").unwrap();
    assert!(db < web, "sorted by name");
    assert!(body.contains("shared by bob"));
    // Host figures say they are generated.
    assert!(body.contains("sample data"));
    // Provisioned memory is real: 2048 + 1024 MiB.
    assert!(body.contains("3 GiB"));
    // The viewer's tiles compare against the host, never a quota.
    assert!(body.contains("Your VMs"));
    assert!(!body.contains("quota"));
    assert!(
        body.contains("<small>/ 8</small>"),
        "vCPU out of host cores"
    );
}

#[tokio::test]
async fn fragments_render_without_the_shell() {
    let fx = fixture();
    let response = get(&fx.pages, "/fragments/instances").await;
    assert_eq!(response.status, StatusCode::OK);
    let body = text(&response);
    assert!(body.starts_with("<section id=\"instances\""));
    assert!(!body.contains("<html"));
    let response = get(&fx.pages, "/fragments/sidebar").await;
    assert!(text(&response).contains("id=\"vm-list\""));
    let response = get(&fx.pages, "/vm/uuid-web/fragments/state").await;
    assert!(text(&response).contains("data-state=\"running\""));
}

#[tokio::test]
async fn metrics_json_is_column_oriented_and_flagged() {
    let fx = fixture();
    let response = get(&fx.pages, "/metrics/host.json?window=600").await;
    assert_eq!(response.status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
    assert_eq!(json["placeholder"], true);
    assert_eq!(
        json["cpu_pct"]["at"].as_array().unwrap().len(),
        json["cpu_pct"]["value"].as_array().unwrap().len()
    );
    assert!(json["cpu_pct"]["at"].as_array().unwrap().len() >= 20);
    let response = get(&fx.pages, "/vm/uuid-web/metrics.json").await;
    let json: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
    assert_eq!(json["memory_total_mib"], 2048);
    // A stranger's machine is not found, not forbidden.
    fx.auth.0.lock().unwrap().replace(fx.bob.clone());
    assert_eq!(
        get(&fx.pages, "/vm/uuid-web/metrics.json").await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn new_form_and_creation() {
    let fx = fixture();
    let response = get(&fx.pages, "/new").await;
    assert_eq!(response.status, StatusCode::OK);
    let body = text(&response);
    assert!(body.contains(".bento.example"));
    assert!(body.contains("value=\"2\"")); // default vCPU
    assert!(body.contains("value=\"2\"") && body.contains("value=\"20\""));

    let response = post(
        &fx.pages,
        "/new",
        "name=Bad_Name&image=debian-13&vcpu=1&memory_gib=1&disk_gib=10",
    )
    .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert!(text(&response).contains("DNS label"));

    let response = post(
        &fx.pages,
        "/new",
        "name=fresh&image=debian-13&vcpu=1&memory_gib=1.5&disk_gib=10&public=on&ksm=on",
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::SEE_OTHER,
        "{}",
        text(&response)
    );
    assert!(location(&response).starts_with("/vm/"));
    assert!(location(&response).contains("toast=Created+fresh"));
    let calls = fx.lifecycle.calls.lock().unwrap().clone();
    assert_eq!(
        calls,
        vec!["create fresh", "visibility uuid-fresh public"],
        "the public switch is a second step"
    );
    let row = fx.store.data.lock().unwrap().instances["uuid-fresh"].clone();
    assert_eq!(row.memory_mib, 1536);
    assert!(row.ksm);
}

#[tokio::test]
async fn vm_pages_and_a_toast_from_the_query() {
    let fx = fixture();
    for path in [
        "/vm/uuid-web",
        "/vm/uuid-web/terminal",
        "/vm/uuid-web/settings",
        "/vm/uuid-web/sharing",
        "/vm/uuid-web/danger",
    ] {
        let response = get(&fx.pages, path).await;
        assert_eq!(response.status, StatusCode::OK, "{path}");
        assert!(
            text(&response).contains("web.bento.example")
                || text(&response).contains("<span>web</span>")
        );
    }
    let response = get(&fx.pages, "/vm/uuid-web?toast=Created+web").await;
    assert!(text(&response).contains("class=\"toast\""));
    assert!(text(&response).contains("Created web"));
    // Unknown UUID: a 404 inside the shell.
    let response = get(&fx.pages, "/vm/nope").await;
    assert_eq!(response.status, StatusCode::NOT_FOUND);
    assert!(text(&response).contains("No such VM"));
    // A shared machine renders its pages read-only.
    let response = get(&fx.pages, "/vm/uuid-db/settings").await;
    assert_eq!(response.status, StatusCode::OK);
    let body = text(&response);
    assert!(body.contains("You cannot change this VM."));
    assert!(!body.contains("Save changes"));
}

#[tokio::test]
async fn settings_apply_only_what_changed_and_rename_last() {
    let fx = fixture();
    // Nothing changed.
    let response = post(
        &fx.pages,
        "/vm/uuid-web/settings",
        "name=web&vcpu=2&memory_gib=2&disk_gib=20&http_port=80",
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::SEE_OTHER,
        "{}",
        text(&response)
    );
    assert!(location(&response).contains("Nothing+changed"));
    assert!(fx.lifecycle.calls.lock().unwrap().is_empty());

    // Resize, port, visibility, and a rename.
    let response = post(
        &fx.pages,
        "/vm/uuid-web/settings",
        "name=web2&vcpu=4&memory_gib=4&disk_gib=40&nested=on&public=on&http_port=8080",
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::SEE_OTHER,
        "{}",
        text(&response)
    );
    assert!(location(&response).contains("Saved+resources%2C+port%2C+visibility%2C+name"));
    let calls = fx.lifecycle.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 4, "{calls:?}");
    assert!(calls[0].starts_with("resize uuid-web"), "{calls:?}");
    assert_eq!(calls[1], "port uuid-web 8080");
    assert_eq!(calls[2], "visibility uuid-web public");
    assert_eq!(calls[3], "rename uuid-web web2");
    let row = fx.store.data.lock().unwrap().instances["uuid-web"].clone();
    assert_eq!(
        (row.vcpu, row.memory_mib, row.disk_gib, row.nested),
        (4, 4096, 40, true)
    );

    // Switching the public VM off makes it private; leaving an off VM
    // unchecked does not touch its visibility.
    let response = post(
        &fx.pages,
        "/vm/uuid-web/settings",
        "name=web2&vcpu=4&memory_gib=4&disk_gib=40&nested=on&http_port=8080",
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::SEE_OTHER,
        "{}",
        text(&response)
    );
    assert_eq!(
        fx.lifecycle.calls.lock().unwrap().last().unwrap(),
        "visibility uuid-web private"
    );
    let response = post(
        &fx.pages,
        "/vm/uuid-web/settings",
        "name=web2&vcpu=4&memory_gib=4&disk_gib=40&nested=on&http_port=8080",
    )
    .await;
    assert!(location(&response).contains("Nothing+changed"));

    // A shrink is refused before anything is applied.
    let response = post(
        &fx.pages,
        "/vm/uuid-web/settings",
        "name=web2&vcpu=4&memory_gib=4&disk_gib=10&public=on&http_port=8080",
    )
    .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert!(text(&response).contains("never shrink"));

    // Bob cannot change Alice's machine: it is shared with nobody, so 404.
    fx.auth.0.lock().unwrap().replace(fx.bob.clone());
    let response = post(
        &fx.pages,
        "/vm/uuid-web/settings",
        "name=web2&vcpu=4&memory_gib=4&disk_gib=40&http_port=80",
    )
    .await;
    assert_eq!(response.status, StatusCode::NOT_FOUND);
    // Alice on Bob's shared machine: 403.
    fx.auth.0.lock().unwrap().replace(fx.alice.clone());
    let response = post(
        &fx.pages,
        "/vm/uuid-db/settings",
        "name=db&vcpu=1&memory_gib=1&disk_gib=10&http_port=80",
    )
    .await;
    assert_eq!(response.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn danger_zone_actions_and_typed_delete() {
    let fx = fixture();
    let response = post(&fx.pages, "/vm/uuid-web/stop", "").await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    assert!(location(&response).contains("Stopping+web"));
    assert_eq!(
        fx.lifecycle.calls.lock().unwrap().last().unwrap(),
        "stop uuid-web"
    );

    let response = post(&fx.pages, "/vm/uuid-web/delete", "confirm=wrong").await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    assert!(location(&response).contains("warn="));
    assert!(
        fx.store
            .data
            .lock()
            .unwrap()
            .instances
            .contains_key("uuid-web")
    );

    let response = post(&fx.pages, "/vm/uuid-web/delete", "confirm=web").await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    assert_eq!(location(&response), "/?toast=Deleted+web");
    assert!(
        !fx.store
            .data
            .lock()
            .unwrap()
            .instances
            .contains_key("uuid-web")
    );
}

#[tokio::test]
async fn sharing_from_the_settings_page() {
    let fx = fixture();
    let body = text(&get(&fx.pages, "/vm/uuid-web/sharing").await);
    assert!(body.contains("This VM has not been shared yet."));
    assert!(body.contains("data-value=\"bob\""), "bob is suggested");
    assert!(!body.contains("data-value=\"alice\""), "the owner is not");
    // Typed text works without a pick (and without JavaScript).
    let response = post(&fx.pages, "/vm/uuid-web/shares", "user=&user_text=bob").await;
    assert!(location(&response).contains("Shared+with+bob"));
    let response = post(&fx.pages, "/vm/uuid-web/shares/bob/remove", "").await;
    assert!(location(&response).contains("Revoked+bob"));
    let response = post(&fx.pages, "/vm/uuid-web/shares", "user=bob").await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    assert!(location(&response).contains("Shared+with+bob"));
    let body = text(&get(&fx.pages, "/vm/uuid-web/sharing").await);
    assert!(body.contains("/vm/uuid-web/shares/bob/remove"));
    assert!(
        !body.contains("data-value=\"bob\""),
        "shared users drop out of the suggestions"
    );
    let response = post(&fx.pages, "/vm/uuid-web/shares", "user=nobody").await;
    assert!(location(&response).contains("warn=no+user+named+nobody"));
    let response = post(&fx.pages, "/vm/uuid-web/shares/bob/remove", "").await;
    assert!(location(&response).contains("Revoked+bob"));
}

#[tokio::test]
async fn operator_settings_pages() {
    let fx = fixture();
    let response = get(&fx.pages, "/settings").await;
    assert_eq!(response.status, StatusCode::OK);
    let body = text(&response);
    assert!(body.contains("alice") && body.contains("bob"));
    assert!(body.contains("Provisioned memory"));
    let response = get(&fx.pages, "/settings/configuration").await;
    assert_eq!(response.status, StatusCode::OK);
    let body = text(&response);
    assert!(body.contains("bento.db"), "{body}");
    assert!(body.contains("Add bootc OCI image"));
    let response = post(
        &fx.pages,
        "/settings/images",
        "name=web-os&reference=quay.io/example/web-os",
    )
    .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    assert_eq!(fx.image_admin.0.lock().unwrap().len(), 1);
    // Bob is not an operator.
    fx.auth.0.lock().unwrap().replace(fx.bob.clone());
    let response = get(&fx.pages, "/settings").await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    assert_eq!(location(&response), "/settings/account");
    assert_eq!(
        get(&fx.pages, "/settings/configuration").await.status,
        StatusCode::FORBIDDEN
    );
    // The account tab is for everyone, and shows only its own tab to Bob.
    let body = text(&get(&fx.pages, "/settings/account").await);
    assert!(body.contains("bob@example.com"));
    assert!(!body.contains("href=\"/settings/configuration\""));
    assert_eq!(
        post(&fx.pages, "/settings/images", "name=x&reference=y")
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(fx.image_admin.0.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn account_page_and_keys() {
    let fx = fixture();
    let response = get(&fx.pages, "/account").await;
    assert_eq!(response.status, StatusCode::PERMANENT_REDIRECT);
    assert_eq!(location(&response), "/settings/account");
    let response = get(&fx.pages, "/settings/account").await;
    assert_eq!(response.status, StatusCode::OK);
    assert!(text(&response).contains("ssh bento.example ls"));
    let response = post(
        &fx.pages,
        "/settings/account/keys",
        "public_key=not+a+key&comment=",
    )
    .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    let key =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGb1wnTMwbA4iywOr0SEe8k5AvVkQYCJ4Nk3E7AeHA3O laptop";
    let response = post(
        &fx.pages,
        "/settings/account/keys",
        &format!("public_key={}&comment=", key.replace(' ', "+")),
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::SEE_OTHER,
        "{}",
        text(&response)
    );
    let body = text(&get(&fx.pages, "/settings/account").await);
    assert!(body.contains("laptop"));
    assert!(body.contains("/settings/account/keys/1/remove"));
    let response = post(&fx.pages, "/settings/account/keys/1/remove", "").await;
    assert!(location(&response).contains("Key+removed"));
}

#[tokio::test]
async fn signed_out_requests_get_a_link_or_a_redirect_header() {
    let fx = fixture();
    fx.auth.0.lock().unwrap().take();
    let response = get(&fx.pages, "/").await;
    assert_eq!(response.status, StatusCode::UNAUTHORIZED);
    assert!(text(&response).contains("Sign in again"));
    assert!(response.headers.get("hx-redirect").is_none());
    let response = send(&fx.pages, Method::GET, "/fragments/instances", "", true).await;
    assert_eq!(response.status, StatusCode::UNAUTHORIZED);
    assert_eq!(response.headers["hx-redirect"], "/");
}

#[test]
fn formatting_helpers() {
    use super::{cooldown_text, gib, mib, pct, urlencode};
    assert_eq!(mib(512), "512 MiB");
    assert_eq!(mib(1024), "1 GiB");
    assert_eq!(mib(1536), "1.5 GiB");
    assert_eq!(gib(82.8), "82.8 GiB");
    assert_eq!(gib(20.0), "20 GiB");
    assert_eq!(pct(27.6), "28%");
    assert_eq!(cooldown_text(90), "2 min");
    assert_eq!(cooldown_text(7200), "2 h");
    assert_eq!(
        urlencode("Saved resources, port"),
        "Saved+resources%2C+port"
    );
}

/// Writes every page to `$BENTO_PAGE_DUMP` for a visual review outside
/// the test run. Skipped unless the variable is set.
#[tokio::test]
async fn dump_pages_for_preview() {
    let Ok(dir) = std::env::var("BENTO_PAGE_DUMP") else {
        return;
    };
    let fx = fixture();
    {
        let mut data = fx.store.data.lock().unwrap();
        data.images.push(bento_types::Image {
            name: "debian-13".to_string(),
            url:
                "https://cloud.debian.org/images/cloud/trixie/latest/debian-13-generic-amd64.qcow2"
                    .to_string(),
            kind: bento_types::ImageKind::Qcow2,
            pinned_checksum: Some("sha256-3b9a1c0d7e2f".to_string()),
            current_checksum: Some("sha256-3b9a1c0d7e2f".to_string()),
        });
        data.keys.insert(
            fx.alice.id,
            vec![bento_types::SshKey {
                id: 1,
                user_id: fx.alice.id,
                public_key: "ssh-ed25519 AAAA".to_string(),
                fingerprint: "SHA256:Q7mZk3v9AbLp0eR2c4Xn".to_string(),
                comment: "work laptop".to_string(),
                created_at: time::OffsetDateTime::now_utc() - time::Duration::days(2),
            }],
        );
    }
    let pages = [
        ("home.html", "/?toast=Created+web"),
        ("new.html", "/new"),
        ("vm.html", "/vm/uuid-web"),
        ("vm_settings.html", "/vm/uuid-web/settings"),
        ("vm_sharing.html", "/vm/uuid-web/sharing"),
        ("vm_danger.html", "/vm/uuid-web/danger"),
        ("vm_terminal.html", "/vm/uuid-web/terminal"),
        ("settings.html", "/settings"),
        ("configuration.html", "/settings/configuration"),
        ("account.html", "/settings/account"),
        ("host.json", "/metrics/host.json?window=3600"),
        ("vm.json", "/vm/uuid-web/metrics.json?window=3600"),
    ];
    std::fs::create_dir_all(&dir).unwrap();
    for (file, path) in pages {
        let response = get(&fx.pages, path).await;
        assert_eq!(
            response.status,
            StatusCode::OK,
            "{path}: {}",
            text(&response)
        );
        std::fs::write(format!("{dir}/{file}"), &response.body).unwrap();
    }
}

/// Serves the pages and the dashboard assets over the fakes on
/// `$BENTO_DEV_PORT`, for driving a browser at the real templates during
/// development. Skipped unless the variable is set; runs until killed.
#[tokio::test]
async fn dev_server() {
    let Ok(port) = std::env::var("BENTO_DEV_PORT") else {
        return;
    };
    let fx = fixture();
    seed_demo(&fx);
    {
        let mut data = fx.store.data.lock().unwrap();
        data.images.push(bento_types::Image {
            name: "debian-13".to_string(),
            url:
                "https://cloud.debian.org/images/cloud/trixie/latest/debian-13-generic-amd64.qcow2"
                    .to_string(),
            kind: bento_types::ImageKind::Qcow2,
            pinned_checksum: Some("sha256-3b9a1c0d7e2f".to_string()),
            current_checksum: Some("sha256-3b9a1c0d7e2f".to_string()),
        });
        data.images.push(bento_types::Image {
            name: "web-os".to_string(),
            url: "quay.io/example/web-os@sha256:9f1c2e8d44aa".to_string(),
            kind: bento_types::ImageKind::Oci,
            pinned_checksum: None,
            current_checksum: Some("sha256-9f1c2e8d44aa".to_string()),
        });
        data.keys.insert(
            fx.alice.id,
            vec![bento_types::SshKey {
                id: 1,
                user_id: fx.alice.id,
                public_key: "ssh-ed25519 AAAA".to_string(),
                fingerprint: "SHA256:Q7mZk3v9AbLp0eR2c4Xn".to_string(),
                comment: "work laptop".to_string(),
                created_at: time::OffsetDateTime::now_utc() - time::Duration::days(2),
            }],
        );
    }
    // The sign-in page is normally served by the gate, which this server
    // has no use for; it is mounted here so it can be looked at.
    let app = fx.pages.merge(bento_dashboard::router()).route(
        "/dev/sign-in",
        axum::routing::get(|| async {
            super::html(
                StatusCode::OK,
                bento_auth::sign_in_page(Some("id.foid.space"), "bento.foid.space"),
            )
        }),
    );
    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{port}"))
        .await
        .unwrap();
    eprintln!("dev server on http://127.0.0.1:{port}/");
    axum::serve(listener, app).await.unwrap();
}

/// A realistic deployment for the dev server and screenshots: two
/// accounts, a handful of machines in every state, real-looking
/// addresses and ages.
fn seed_demo(fx: &crate::tests::Fixture) {
    use bento_types::{DesiredState, State as InstanceState, Visibility};
    use time::{Duration, OffsetDateTime};
    let now = OffsetDateTime::now_utc();
    let mut data = fx.store.data.lock().unwrap();
    // The viewer is "abby" (an operator); the other account is "zack".
    for user in data.users.values_mut() {
        if user.id == fx.alice.id {
            user.name = "abby".to_string();
            user.email = "abby@example.org".to_string();
        } else {
            user.name = "zack".to_string();
            user.email = "zack@example.org".to_string();
        }
    }
    let viewer = data.users[&fx.alice.id].clone();
    fx.auth.0.lock().unwrap().replace(viewer);
    data.instances.clear();
    data.shares.clear();
    let rows = [
        (
            "blog",
            fx.alice.id,
            InstanceState::Running,
            "10.100.1.2",
            "debian-13",
            (2, 2048, 20),
            Visibility::Public,
            80,
            3,
            41,
        ),
        (
            "ci-runner",
            fx.alice.id,
            InstanceState::Running,
            "10.100.1.3",
            "fedora-42",
            (4, 8192, 60),
            Visibility::Off,
            80,
            0,
            12,
        ),
        (
            "matrix",
            fx.alice.id,
            InstanceState::Starting,
            "10.100.1.4",
            "web-os",
            (2, 4096, 40),
            Visibility::Private,
            8008,
            1,
            0,
        ),
        (
            "scratch",
            fx.alice.id,
            InstanceState::Stopped,
            "10.100.1.5",
            "debian-13",
            (1, 1024, 10),
            Visibility::Off,
            80,
            9,
            27,
        ),
        (
            "staging",
            fx.bob.id,
            InstanceState::Running,
            "10.100.2.2",
            "web-os",
            (2, 4096, 30),
            Visibility::Private,
            3000,
            0,
            5,
        ),
    ];
    for (name, owner, state, address, image, resources, visibility, port, days_seen, days_old) in
        rows
    {
        let mut row = crate::tests::instance(
            &format!("uuid-{name}"),
            name,
            owner,
            state,
            if state == InstanceState::Stopped {
                DesiredState::Stopped
            } else {
                DesiredState::Running
            },
            resources,
        );
        row.address = address.to_string();
        row.image_name = image.to_string();
        row.visibility = visibility;
        row.http_port = port;
        row.created_at = now - Duration::days(days_old);
        row.last_seen_at = if state == InstanceState::Starting {
            None
        } else {
            Some(now - Duration::days(days_seen) - Duration::minutes(7))
        };
        data.instances.insert(row.uuid.clone(), row);
    }
    data.shares.insert(
        "uuid-staging".to_string(),
        vec![bento_types::Share {
            instance_uuid: "uuid-staging".to_string(),
            user_id: fx.alice.id,
            created_at: now - Duration::days(4),
        }],
    );
}

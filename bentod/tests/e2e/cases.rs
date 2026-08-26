//! The end-to-end scenarios.
//!
//! Every test drives the shipped `bentod` binary over HTTP and then reads
//! back what reached the host: the domains and networks the control plane
//! defined, the nftables ruleset it applied, and the files it left in the
//! storage directory.
//!
//! All of them use the multi-threaded runtime. The fake libvirtd answers
//! on the test's own runtime, and the harness blocks a thread while it
//! waits for the daemon; on the single-threaded runtime the two deadlock.

use serde_json::json;

use crate::harness::{Bento, IMAGE_NAME, USER_NAME};

/// A throwaway key. `add_ssh_key` parses it, so it has to be real.
const PUBLIC_KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIL3wJx6Q0Cs8Z6xUOD1sV0OJnZ8XkQ0k1J8pQzZ5nAaX tester@e2e";

#[tokio::test(flavor = "multi_thread")]
async fn an_instance_runs_through_its_whole_lifecycle() {
    let mut bento = Bento::start().await;

    // The token identifies the seeded account (SPEC 13).
    let whoami = bento.get("/api/whoami").await.expect_status(200);
    assert_eq!(whoami.json()["user"]["name"], USER_NAME);
    assert_eq!(
        whoami.json()["operator"],
        true,
        "the account is an operator"
    );

    // `fetch-images` ran during setup, so the allowlisted image has a
    // current checksum (SPEC 5.1).
    let images = bento.get("/api/images").await.expect_status(200);
    let image = images
        .json()
        .as_array()
        .and_then(|images| {
            images
                .iter()
                .find(|image| image["name"] == IMAGE_NAME)
                .cloned()
        })
        .unwrap_or_else(|| panic!("image {IMAGE_NAME} is missing: {}", images.body));
    assert_eq!(
        image["current_checksum"].as_str().unwrap_or_default().len(),
        64,
        "image has no stored version: {image}"
    );

    // The owner's key rides into the guest through the seed ISO (SPEC 5.2).
    bento
        .post("/api/ssh-keys", json!({ "public_key": PUBLIC_KEY }))
        .await
        .expect_status(201);

    // --- create ---------------------------------------------------------
    let created = bento
        .post(
            "/api/instances",
            json!({
                "name": "web",
                "image": IMAGE_NAME,
                "vcpu": 1,
                "memory_mib": 512,
                "disk_gib": 1,
            }),
        )
        .await
        .expect_status(201);
    let uuid = created.json()["uuid"]
        .as_str()
        .expect("created instance has a uuid")
        .to_string();
    let instance = bento.wait_for_state("web", "running").await;
    assert_eq!(instance["owner"], USER_NAME);
    assert!(
        instance["address"]
            .as_str()
            .unwrap_or_default()
            .starts_with(bento.user_subnet.trim_end_matches("0/24")),
        "instance address is outside the owner's subnet: {instance}"
    );

    // libvirt holds a domain that Bento defined, started, and left with
    // autostart off, because Bento restores state itself (SPEC 11.2).
    let domain = bento
        .libvirtd
        .domain("web")
        .unwrap_or_else(|| panic!("no domain was defined: {:?}", bento.libvirtd.domain_names()));
    assert!(domain.running, "the domain was defined but never started");
    assert!(!domain.autostart, "autostart was left on");
    assert!(bento.libvirtd.saw("set-domain-autostart web 0"));
    assert!(
        domain.xml.contains("<memory unit='MiB'>512</memory>"),
        "domain XML does not carry the requested memory: {}",
        domain.xml
    );

    // The overlay disk and the seed ISO both landed (SPEC 5.2).
    let files = bento.storage_files();
    assert!(
        files.iter().any(|name| name.ends_with(".qcow2")),
        "no overlay disk in the storage directory: {files:?}"
    );
    assert!(
        files.iter().any(|name| name.ends_with(".iso")),
        "no seed ISO in the storage directory: {files:?}"
    );

    // --- stop -----------------------------------------------------------
    // A lifecycle action is accepted, not completed, so the state comes
    // from the poller and not from the response.
    bento
        .post_empty(&format!("/api/instances/{uuid}/stop"))
        .await
        .expect_status(202);
    bento.wait_for_state("web", "stopped").await;
    assert!(bento.libvirtd.saw("shutdown-domain web"));
    assert!(
        !bento
            .libvirtd
            .domain("web")
            .expect("domain still defined")
            .running
    );

    // --- rename ---------------------------------------------------------
    // Renaming needs the instance stopped (SPEC 7.3), which is why it
    // comes between the stop and the start.
    let renamed = bento
        .post(
            &format!("/api/instances/{uuid}/rename"),
            json!({ "new_name": "api" }),
        )
        .await
        .expect_status(200);
    assert_eq!(
        renamed.json()["uuid"],
        uuid,
        "rename must keep the UUID (SPEC 7.3)"
    );
    assert_eq!(renamed.json()["name"], "api");

    // --- start again ----------------------------------------------------
    bento
        .post_empty(&format!("/api/instances/{uuid}/start"))
        .await
        .expect_status(202);
    bento.wait_for_state("api", "running").await;

    // --- delete ---------------------------------------------------------
    bento
        .delete(&format!("/api/instances/{uuid}"))
        .await
        .expect_status(204);
    let listed = bento.get("/api/instances").await.expect_status(200);
    assert_eq!(
        listed.json()["instances"]
            .as_array()
            .map(Vec::len)
            .unwrap_or_default(),
        0,
        "the deleted instance is still listed: {}",
        listed.body
    );
    assert!(
        bento.libvirtd.domain_names().is_empty(),
        "libvirt still holds {:?}",
        bento.libvirtd.domain_names()
    );
    assert!(
        bento.storage_files().is_empty(),
        "delete left files behind: {:?}",
        bento.storage_files()
    );

    // SPEC 11.2 makes an orderly stop part of the contract.
    bento.shutdown();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_host_gets_a_network_and_a_firewall_for_every_user() {
    let bento = Bento::start().await;
    let subnet = bento.user_subnet.clone();

    // Startup defines and starts the user's libvirt network (SPEC 6.2).
    let network = bento
        .libvirtd
        .network(&bento.user_network)
        .unwrap_or_else(|| {
            panic!(
                "no network {} for {USER_NAME}: {:?}",
                bento.user_network,
                bento.libvirtd.network_names()
            )
        });
    assert!(
        network.active,
        "the user network was defined but not started"
    );
    assert!(
        network.autostart,
        "the user network is not set to autostart"
    );

    // And loads one whole-table nftables ruleset (SPEC 6.3).
    let ruleset = bento.nft_rulesets();
    assert!(
        ruleset.contains("table inet bento"),
        "no Bento table was applied: {ruleset}"
    );
    assert!(
        ruleset.contains(&subnet),
        "the ruleset does not mention the user subnet {subnet}: {ruleset}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_api_refuses_a_request_with_no_token() {
    let bento = Bento::start().await;
    let response = bento.get_anonymous("/api/instances").await;
    assert_eq!(response.status, 401, "body was {:?}", response.body);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_request_over_quota_is_refused() {
    let bento = Bento::start().await;

    // The seeded quota allows 8192 MiB in total (SPEC 6.1).
    let response = bento
        .post(
            "/api/instances",
            json!({
                "name": "huge",
                "image": IMAGE_NAME,
                "vcpu": 1,
                "memory_mib": 65536,
                "disk_gib": 1,
            }),
        )
        .await
        .expect_status(409);
    assert!(
        response.json()["quota"].is_object(),
        "the refusal carries no quota detail: {}",
        response.body
    );
    assert!(
        bento.libvirtd.domain_names().is_empty(),
        "a refused create still reached libvirt: {:?}",
        bento.libvirtd.domain_names()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn reconcile_finds_nothing_to_report() {
    let mut bento = Bento::start().await;
    bento
        .post(
            "/api/instances",
            json!({
                "name": "web",
                "image": IMAGE_NAME,
                "vcpu": 1,
                "memory_mib": 512,
                "disk_gib": 1,
            }),
        )
        .await
        .expect_status(201);
    bento.wait_for_state("web", "running").await;

    // The database and libvirt agree, so the operator command reports no
    // disagreement (SPEC 15). It runs after shutdown so that it, not the
    // control plane, holds the database.
    bento.shutdown();
    let report = bento.run_subcommand("reconcile");
    assert!(
        !report.to_lowercase().contains("missing"),
        "reconcile reported a disagreement:\n{report}"
    );
}

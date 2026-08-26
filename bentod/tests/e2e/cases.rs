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

use crate::harness::{
    BOOTC_ROOTFS, BUILDER_IMAGE, Bento, IMAGE_NAME, OCI_IMAGE_NAME, OCI_REFERENCE, Setup, USER_NAME,
};

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
async fn a_bootc_image_is_converted_to_a_disk_an_instance_boots_from() {
    let mut bento = Bento::start_with(Setup { bootc: true }).await;

    // --- the conversion pipeline ----------------------------------------
    // Startup ran `fetch-images`, which for an OCI entry converts rather
    // than downloads. Every step is checked, in order, because each one
    // carries a flag that matters.
    let podman = bento.podman_commands();
    let step = |needle: &str| {
        podman
            .iter()
            .position(|command| command.contains(needle))
            .unwrap_or_else(|| panic!("no podman step matched {needle:?}: {podman:#?}"))
    };

    // The tag moves, so the pipeline pulls and inspects before it can
    // decide whether an earlier build still applies.
    let pull = step(&format!("pull --quiet --policy=always -- {OCI_REFERENCE}"));
    let inspect = step(&format!(
        "image inspect --format={{{{.Digest}}}} -- {OCI_REFERENCE}"
    ));
    // The contract check runs unprivileged and without a network, before
    // the image ever reaches the privileged builder.
    let contract = step("--network=none");
    assert!(
        podman[contract].contains("/usr/bin/cloud-init") && podman[contract].contains("qemu-ga"),
        "the contract check does not look for what a Bento guest needs: {}",
        podman[contract]
    );
    assert!(
        !podman[contract].contains("--privileged"),
        "the contract check ran privileged: {}",
        podman[contract]
    );
    let builder_pull = step(&format!("pull --quiet --policy=always -- {BUILDER_IMAGE}"));
    let build = step("--bootc-ref");
    assert!(
        pull < inspect && inspect < contract && contract < builder_pull && builder_pull < build,
        "the pipeline ran out of order: {podman:#?}"
    );

    // The build itself: privileged, with the output directory and the
    // rootful container store both mounted, and told the configured
    // filesystem rather than the fallback.
    let build = &podman[build];
    for expected in [
        "--privileged",
        BUILDER_IMAGE,
        &format!("--bootc-ref {OCI_REFERENCE}"),
        &format!("--bootc-default-fs {BOOTC_ROOTFS}"),
        ":/var/lib/containers/storage",
        ":/output",
    ] {
        assert!(
            build.contains(expected),
            "the image-builder run is missing {expected:?}: {build}"
        );
    }

    // --- what the conversion produced -----------------------------------
    let images = bento.get("/api/images").await.expect_status(200);
    let image = images
        .json()
        .as_array()
        .and_then(|images| {
            images
                .iter()
                .find(|image| image["name"] == OCI_IMAGE_NAME)
                .cloned()
        })
        .unwrap_or_else(|| panic!("image {OCI_IMAGE_NAME} is missing: {}", images.body));
    assert_eq!(image["kind"], "oci", "the entry is not reported as OCI");
    assert_eq!(
        image["source"], OCI_REFERENCE,
        "the entry does not carry its OCI reference"
    );
    let checksum = image["current_checksum"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert_eq!(
        checksum.len(),
        64,
        "the OCI entry has no built version: {image}"
    );

    // The built disk is stored under the checksum of its own content, next
    // to the downloaded one (SPEC 5.1).
    let backing = bento.image_path(&checksum);
    assert!(
        backing.is_file(),
        "no disk at the content-addressed path {}",
        backing.display()
    );

    // --- an instance boots from it --------------------------------------
    // The conversion is only worth anything if the disk it produced can
    // back an overlay, which is what `qemu-img create -b` refuses for
    // anything that is not a real image.
    bento
        .post(
            "/api/instances",
            json!({
                "name": "bootc",
                "image": OCI_IMAGE_NAME,
                "vcpu": 1,
                "memory_mib": 512,
                "disk_gib": 1,
            }),
        )
        .await
        .expect_status(201);
    bento.wait_for_state("bootc", "running").await;

    let overlay = bento
        .storage_files()
        .into_iter()
        .find(|name| name.ends_with(".qcow2"))
        .unwrap_or_else(|| panic!("no overlay disk: {:?}", bento.storage_files()));
    let info = qemu_img_info(&bento.storage_dir().join(overlay));
    assert!(
        info.contains(&backing.display().to_string()),
        "the overlay is not backed by the converted disk ({}): {info}",
        backing.display()
    );

    // --- the build is cached by source digest ---------------------------
    // `fetch-images` runs after shutdown so that it, and not the control
    // plane, holds the database.
    bento.shutdown();
    let before = bento.podman_commands().len();
    bento.run_subcommand("fetch-images");
    let after = bento.podman_commands();

    assert!(
        after.len() > before,
        "the second run did not re-check the moving tag: {after:#?}"
    );
    let rebuilt = after[before..]
        .iter()
        .filter(|command| command.contains("--bootc-ref"))
        .count();
    assert_eq!(
        rebuilt,
        0,
        "the same OCI digest was built a second time: {:#?}",
        &after[before..]
    );
    assert!(
        backing.is_file(),
        "the cached run removed the disk it should have reused"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_operator_adds_a_bootc_image_at_runtime() {
    let bento = Bento::start_with(Setup { bootc: true }).await;
    const REFERENCE: &str = "quay.io/centos-bootc/centos-bootc:stream10";

    // The allowlist is durable, so an operator can append to it without an
    // edit to bento.toml and a restart.
    let added = bento
        .post(
            "/api/images",
            json!({ "name": "centos-bootc", "reference": REFERENCE }),
        )
        .await
        .expect_status(201);
    assert_eq!(added.json()["name"], "centos-bootc");

    // The addition built the image before it answered, so the entry comes
    // back ready to create instances from.
    let images = bento.get("/api/images").await.expect_status(200);
    let image = images
        .json()
        .as_array()
        .and_then(|images| {
            images
                .iter()
                .find(|image| image["name"] == "centos-bootc")
                .cloned()
        })
        .unwrap_or_else(|| panic!("the added image is missing: {}", images.body));
    assert_eq!(image["kind"], "oci");
    assert_eq!(image["source"], REFERENCE);
    let checksum = image["current_checksum"].as_str().unwrap_or_default();
    assert_eq!(
        checksum.len(),
        64,
        "the added image has no version: {image}"
    );
    assert!(
        bento.image_path(checksum).is_file(),
        "the added image has a version but no disk"
    );

    // Its disk is its own. Two different operating-system images must not
    // collapse onto one content-addressed file.
    let converted = bento.get("/api/images").await.expect_status(200);
    let other = converted.json();
    let other = other
        .as_array()
        .expect("images list")
        .iter()
        .find(|image| image["name"] == OCI_IMAGE_NAME)
        .expect("the configured bootc image");
    assert_ne!(
        other["current_checksum"].as_str().unwrap_or_default(),
        checksum,
        "two bootc images share one disk"
    );

    // A request without a credential never reaches the allowlist.
    let anonymous = bento.post_anonymous("/api/images", json!({})).await;
    assert_eq!(anonymous.status, 401, "body was {:?}", anonymous.body);
}

/// Reads back what `qemu-img` says about a disk, backing chain included.
fn qemu_img_info(path: &std::path::Path) -> String {
    let output = std::process::Command::new("qemu-img")
        .arg("info")
        .arg(path)
        .output()
        .expect("run qemu-img info");
    assert!(
        output.status.success(),
        "qemu-img info {} failed: {}",
        path.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
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

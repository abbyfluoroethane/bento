//! The end-to-end harness.
//!
//! It builds a whole Bento deployment in a temporary directory and runs
//! the real `bentod` binary against it: real configuration parsing, a real
//! SQLite database, real `qemu-img` and `xorriso`, a real HTTP listener on
//! loopback, and real bearer-token authentication. Only three things
//! outside the binary are substituted, because a CI runner cannot supply
//! them:
//!
//! * libvirtd, replaced by [`crate::libvirtd::Libvirtd`] on a Unix socket;
//! * the image mirror, replaced by [`crate::imageserver::ImageServer`];
//! * `nft`, replaced by a stub on `PATH` that records the ruleset. Loading
//!   a real ruleset needs `CAP_NET_ADMIN` and would rewrite the runner's
//!   own firewall.
//!
//! Everything else is the shipped code path.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use bento_types::Quota;
use sha2::{Digest, Sha256};
use tempfile::TempDir;

use crate::imageserver::ImageServer;
use crate::libvirtd::Libvirtd;

/// The image name in the allowlist, and so the image every test creates
/// instances from.
pub const IMAGE_NAME: &str = "debian-13";
/// The seeded account. It is also the one operator.
pub const USER_NAME: &str = "tester";
/// How long readiness and state polling wait before failing.
const TIMEOUT: Duration = Duration::from_secs(60);

/// A running Bento under test.
pub struct Bento {
    dir: TempDir,
    config: PathBuf,
    base_url: String,
    token: String,
    http: reqwest::Client,
    serve: Option<std::process::Child>,
    /// The subnet the store allocated to the seeded account.
    pub user_subnet: String,
    /// The libvirt network that subnet implies, for example
    /// `bento-user-0`.
    pub user_network: String,
    pub libvirtd: Libvirtd,
    _images: ImageServer,
}

impl Drop for Bento {
    fn drop(&mut self) {
        // A test that fails mid-way still leaves no stray daemon.
        if let Some(child) = self.serve.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Bento {
    /// Brings up a deployment: fake libvirtd, image mirror, configuration,
    /// a seeded account, a real `bentod fetch-images`, and finally
    /// `bentod serve` answering on loopback.
    pub async fn start() -> Self {
        install_crypto_provider();
        let dir = tempfile::Builder::new()
            .prefix("bento-e2e-")
            .tempdir()
            .expect("create temporary directory");
        let path = dir.path().to_path_buf();
        for name in ["images", "storage", "keys", "bin"] {
            std::fs::create_dir(path.join(name)).expect("create directory");
        }
        write_nft_stub(&path.join("bin"));

        let socket = path.join("libvirt.sock");
        let libvirtd = Libvirtd::start(&socket).expect("start fake libvirtd");

        let image_body = build_image(&path);
        let checksum = hex_digest(&image_body);
        let images = ImageServer::start(image_body)
            .await
            .expect("start image server");

        let port = free_port();
        let base_url = format!("http://127.0.0.1:{port}");
        let config = path.join("bento.toml");
        std::fs::write(
            &config,
            configuration(&path, &socket, port, images.url(), &checksum),
        )
        .expect("write configuration");

        let (token, user_subnet) = seed_account(&path.join("bento.db")).await;
        let user_network = network_name(&user_subnet);

        run_to_completion(&config, &path, "fetch-images");

        let serve = spawn_serve(&config, &path);
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()
            .expect("build HTTP client");

        let mut bento = Self {
            dir,
            config,
            base_url,
            token,
            http,
            serve: Some(serve),
            user_subnet,
            user_network,
            libvirtd,
            _images: images,
        };
        bento.wait_until_ready().await;
        bento
    }

    // ------------------------------------------------------------ HTTP

    pub async fn get(&self, path: &str) -> Response {
        self.send(self.http.get(self.url(path))).await
    }

    pub async fn post(&self, path: &str, body: serde_json::Value) -> Response {
        self.send(self.http.post(self.url(path)).json(&body)).await
    }

    pub async fn post_empty(&self, path: &str) -> Response {
        self.send(self.http.post(self.url(path))).await
    }

    pub async fn delete(&self, path: &str) -> Response {
        self.send(self.http.delete(self.url(path))).await
    }

    /// Sends a request with no credential, for the tests that check the
    /// endpoint refuses one.
    pub async fn get_anonymous(&self, path: &str) -> Response {
        let response = self
            .http
            .get(self.url(path))
            .send()
            .await
            .unwrap_or_else(|error| self.fail(&format!("GET {path}: {error}")));
        Response::read(response).await
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> Response {
        let response = request
            .bearer_auth(&self.token)
            .send()
            .await
            .unwrap_or_else(|error| self.fail(&format!("request failed: {error}")));
        Response::read(response).await
    }

    // ----------------------------------------------------------- Waits

    /// Polls the instance list until `name` reports `state`, and returns
    /// the instance. Bento reaches a state through the lifecycle poller,
    /// so no single response is authoritative.
    pub async fn wait_for_state(&self, name: &str, state: &str) -> serde_json::Value {
        let deadline = Instant::now() + TIMEOUT;
        let mut last = String::from("<never listed>");
        loop {
            let response = self.get("/api/instances").await;
            if let Some(instance) = response
                .json()
                .get("instances")
                .and_then(|value| value.as_array())
                .and_then(|instances| {
                    instances
                        .iter()
                        .find(|instance| instance["name"] == name)
                        .cloned()
                })
            {
                last = instance["state"].as_str().unwrap_or_default().to_string();
                if last == state {
                    return instance;
                }
            }
            if Instant::now() >= deadline {
                self.fail(&format!(
                    "instance {name} never reached state {state}; last state was {last}"
                ));
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    async fn wait_until_ready(&mut self) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(child) = self.serve.as_mut()
                && let Ok(Some(status)) = child.try_wait()
            {
                self.fail(&format!("bentod serve exited early with {status}"));
            }
            if let Ok(response) = self
                .http
                .get(self.url("/api/whoami"))
                .bearer_auth(&self.token)
                .send()
                .await
                && response.status().is_success()
            {
                return;
            }
            if Instant::now() >= deadline {
                self.fail("bentod serve never answered /api/whoami");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    // ------------------------------------------------------- Assertions

    /// Every nftables ruleset `bentod` has applied, concatenated. The
    /// control plane reloads the whole table on each change (SPEC 6.3).
    pub fn nft_rulesets(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("nft.log")).unwrap_or_default()
    }

    /// The files under the storage directory, sorted. Overlay disks and
    /// seed ISOs land here (SPEC 5.2).
    pub fn storage_files(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.dir.path().join("storage"))
            .expect("read storage directory")
            .filter_map(|entry| Some(entry.ok()?.file_name().to_string_lossy().into_owned()))
            .collect();
        names.sort();
        names
    }

    /// Stops the daemon with SIGTERM and asserts it shut down cleanly.
    /// SPEC 11.2 makes an orderly stop part of the contract, so the test
    /// checks it rather than killing the process.
    pub fn shutdown(&mut self) {
        let Some(mut child) = self.serve.take() else {
            return;
        };
        let pid = child.id().to_string();
        let signalled = Command::new("kill")
            .args(["-TERM", &pid])
            .status()
            .expect("send SIGTERM");
        assert!(signalled.success(), "kill -TERM {pid} failed");

        let deadline = Instant::now() + TIMEOUT;
        loop {
            match child.try_wait().expect("wait for bentod") {
                Some(status) => {
                    assert!(
                        status.success(),
                        "bentod serve exited with {status}\n{}",
                        self.log()
                    );
                    return;
                }
                None if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!(
                        "bentod serve ignored SIGTERM for {TIMEOUT:?}\n{}",
                        self.log()
                    );
                }
                None => std::thread::sleep(Duration::from_millis(50)),
            }
        }
    }

    /// The daemon's stderr so far. Every failure report carries it,
    /// because a failure here is usually explained by one log line.
    pub fn log(&self) -> String {
        let text = std::fs::read_to_string(self.dir.path().join("serve.log")).unwrap_or_default();
        format!("---- bentod serve log ----\n{text}--------------------------")
    }

    fn fail(&self, message: &str) -> ! {
        panic!("{message}\n{}", self.log());
    }

    /// Runs another `bentod` subcommand against the same deployment.
    pub fn run_subcommand(&self, command: &str) -> String {
        run_to_completion(&self.config, self.dir.path(), command)
    }
}

/// One API response, read in full so the body is available after the
/// status has been checked.
pub struct Response {
    pub status: u16,
    pub body: String,
}

impl Response {
    async fn read(response: reqwest::Response) -> Self {
        let status = response.status().as_u16();
        let body = response.text().await.unwrap_or_default();
        Self { status, body }
    }

    /// Asserts the status and returns the response, for chaining.
    pub fn expect_status(self, want: u16) -> Self {
        assert_eq!(
            self.status, want,
            "unexpected status; body was {:?}",
            self.body
        );
        self
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body)
            .unwrap_or_else(|error| panic!("body is not JSON ({error}): {:?}", self.body))
    }
}

// ------------------------------------------------------------- Setup

/// Writes the configuration. Every path points inside the temporary
/// directory, so a test can never touch a real deployment.
fn configuration(dir: &Path, socket: &Path, port: u16, image_url: &str, checksum: &str) -> String {
    let dir = dir.display();
    format!(
        r#"base_domain = "e2e.test"
libvirt_uri = "qemu:///system?socket={socket}"
image_dir = "{dir}/images"
storage_dir = "{dir}/storage"
db_path = "{dir}/bento.db"
key_dir = "{dir}/keys"
private_range = "10.100.0.0/16"
dns = ["1.1.1.1"]
operators = ["{USER_NAME}"]

[listen]
http = "127.0.0.1:{port}"
# The proxy and the SSH frontend are separate processes and are not
# started here, but the ports still have to stay off the control plane's.
https = "127.0.0.1:0"
ssh = "127.0.0.1:0"
tls = "off"

[defaults]
image = "{IMAGE_NAME}"
vcpu = 1
memory_mib = 512
disk_gib = 1

# An empty issuer leaves dashboard login off. The API tests authenticate
# with bearer tokens, which need no provider (SPEC 13).
[oidc]
issuer = ""
client_id = ""
client_secret = ""
allow_signup = false

[[images]]
name = "{IMAGE_NAME}"
url = "{image_url}"
pinned_checksum = "sha256:{checksum}"
"#,
        socket = socket.display(),
    )
}

/// Creates the account, its quota, and an API token, then closes the
/// database so `bentod` owns it alone. Returns the token plaintext and
/// the subnet the store allocated.
async fn seed_account(db_path: &Path) -> (String, String) {
    let store = bento_store::Store::open(db_path)
        .await
        .expect("open database");
    let range = bento_config::parse_prefix("10.100.0.0/16").expect("parse private range");
    let user = store
        .register_user(USER_NAME, "tester@e2e.test", None, range)
        .await
        .expect("register user");
    store
        .set_quota(Quota {
            user_id: user.id,
            max_instances: 4,
            max_vcpu: 8,
            max_memory_mib: 8192,
            max_disk_gib: 64,
        })
        .await
        .expect("set quota");

    // The store keeps only the hash, so the plaintext exists here and
    // nowhere else (SPEC 13).
    let plaintext = format!("{}e2e-{}", bento_auth::TOKEN_PREFIX, std::process::id());
    store
        .create_token(
            user.id,
            bento_auth::hash_token(&plaintext),
            time::OffsetDateTime::UNIX_EPOCH,
        )
        .await
        .expect("create token");
    store.close().await.expect("close database");
    (plaintext, user.subnet)
}

/// Builds the qcow2 that the image server hands out. It is real: the
/// overlay step runs `qemu-img create -b` against it and would reject a
/// file that is not an image.
fn build_image(dir: &Path) -> Vec<u8> {
    let path = dir.join("base.qcow2");
    let status = Command::new("qemu-img")
        .args(["create", "-f", "qcow2"])
        .arg(&path)
        .arg("64M")
        .stdout(Stdio::null())
        .status()
        .expect("run qemu-img create");
    assert!(status.success(), "qemu-img create failed: {status}");
    let body = std::fs::read(&path).expect("read base image");
    std::fs::remove_file(&path).expect("remove base image");
    body
}

/// Writes the `nft` stub. It appends each ruleset it is given, so the
/// assertions can read what the control plane would have loaded.
fn write_nft_stub(bin: &Path) {
    let path = bin.join("nft");
    let mut file = std::fs::File::create(&path).expect("create nft stub");
    file.write_all(
        b"#!/bin/sh\n\
          # Stands in for nft(8): records the ruleset and accepts it.\n\
          printf '===== ruleset =====\\n' >> \"$BENTO_E2E_NFT_LOG\"\n\
          cat >> \"$BENTO_E2E_NFT_LOG\"\n\
          exit 0\n",
    )
    .expect("write nft stub");
    drop(file);
    let mut permissions = std::fs::metadata(&path)
        .expect("stat nft stub")
        .permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
    std::fs::set_permissions(&path, permissions).expect("make nft stub executable");
}

fn command(config: &Path, dir: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_bentod"));
    command
        .arg("--config")
        .arg(config)
        // The stub directory comes first so `nft` resolves to the stub.
        // `qemu-img` and `xorriso` still resolve to the real binaries.
        .env(
            "PATH",
            format!(
                "{}:{}",
                dir.join("bin").display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .env("BENTO_E2E_NFT_LOG", dir.join("nft.log"))
        .env("RUST_LOG", "info");
    command
}

/// Runs a one-shot subcommand and returns its output. A failure here is
/// a test failure: these commands are part of what is under test.
fn run_to_completion(config: &Path, dir: &Path, subcommand: &str) -> String {
    let output = command(config, dir)
        .arg(subcommand)
        .output()
        .unwrap_or_else(|error| panic!("run bentod {subcommand}: {error}"));
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.status.success(),
        "bentod {subcommand} failed with {}\n{combined}",
        output.status
    );
    combined
}

fn spawn_serve(config: &Path, dir: &Path) -> std::process::Child {
    let log = std::fs::File::create(dir.join("serve.log")).expect("create serve log");
    command(config, dir)
        .arg("serve")
        .stdout(Stdio::null())
        .stderr(Stdio::from(log))
        .spawn()
        .expect("spawn bentod serve")
}

/// Reserves a loopback port by binding and releasing it. The control
/// plane has no way to report a port it chose itself, so the test picks
/// one.
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback port");
    listener.local_addr().expect("read local address").port()
}

/// Names the libvirt network of a user subnet the way the control plane
/// does, through the same `bento_network` types.
fn network_name(subnet: &str) -> String {
    let plan = bento_network::Plan::new("10.100.0.0/16").expect("build the address plan");
    let prefix = bento_config::parse_prefix(subnet).expect("parse the user subnet");
    let index = plan.index(prefix).expect("locate the subnet in the plan");
    bento_network::UserNetwork::new(plan, index as isize)
        .expect("build the user network")
        .name
}

fn hex_digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// The workspace pins rustls to the `ring` provider and builds it with no
/// default, so every process that makes an HTTPS client has to install one
/// first. `bentod` does it in `main`; the test client needs its own.
fn install_crypto_provider() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

//! Integration tests use a real store and lifecycle manager over the
//! in-memory hypervisor and fake host tools: the same seams assembled in
//! production.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bento_api::Lifecycle as _;
use bento_auth::TokenStore as _;
use bento_cli::Lifecycle as _;
use bento_cloudinit::Seed;
use bento_hypervisor::{Definer, Fake, Hypervisor};
use bento_lifecycle::{ISOBuilder, ImageStore};
use bento_network::{Plan, PortRange};
use bento_proxy::InstanceSource as _;
use bento_sshfront::Registrar as _;
use bento_store::{Error as StoreError, Store};
use bento_types::{DesiredState, Image, ImageVersion, State, Visibility};
use time::OffsetDateTime;

use crate::adapters::{
    ApiBackend, AuthAccess, AuthTokens, AuthUsers, Backend, CliBackend, LifecycleStore,
    NetworkEnsurer, ProxySource, Registrar, access_status,
};
use crate::firewall::Firewall;
use crate::firewall::tests::RecordingApplier;

const OWNER_KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIFoo owner@laptop";

struct FakeImages;

#[async_trait]
impl ImageStore for FakeImages {
    async fn create_overlay(
        &self,
        _checksum: &str,
        path: &Path,
        _disk_gib: i64,
    ) -> Result<(), bento_lifecycle::DynError> {
        tokio::fs::write(path, b"overlay").await?;
        Ok(())
    }
}

#[derive(Default)]
struct FakeIso(Mutex<HashMap<PathBuf, Seed>>);

#[async_trait]
impl ISOBuilder for FakeIso {
    async fn build(&self, seed: &Seed, path: &Path) -> Result<(), bento_lifecycle::DynError> {
        self.0.lock().unwrap().insert(path.to_owned(), seed.clone());
        Ok(())
    }
}

struct FakeDefiner(Arc<Fake>);

#[async_trait]
impl Definer for FakeDefiner {
    async fn define(&self, xml: &str) -> Result<(), bento_hypervisor::Error> {
        let name = xml
            .split_once("<name>")
            .and_then(|(_, rest)| rest.split_once("</name>"))
            .map(|(name, _)| name)
            .ok_or_else(|| bento_hypervisor::Error::Operation("missing name".into()))?;
        let replacing = self.0.state(name).await.is_ok();
        if replacing {
            self.0.remove(name).await?;
        }
        self.0.create(xml).await?;
        if !replacing {
            self.0.stop(name).await?;
        }
        Ok(())
    }
}

struct Env {
    _temp: tempfile::TempDir,
    store: Store,
    plan: Plan,
    hypervisor: Arc<Fake>,
    manager: Arc<bento_lifecycle::Manager>,
    iso: Arc<FakeIso>,
    host_id: i64,
}

impl Env {
    async fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path().join("bento.db")).await.unwrap();
        let plan = Plan::new("10.100.0.0/16").unwrap();
        let host = store
            .ensure_host("testhost", "qemu:///system")
            .await
            .unwrap();
        let hypervisor = Arc::new(Fake::default());
        let iso = Arc::new(FakeIso::default());
        let manager = Arc::new(
            bento_lifecycle::Manager::new(bento_lifecycle::Config {
                hypervisor: Some(hypervisor.clone()),
                definer: Some(Arc::new(FakeDefiner(hypervisor.clone()))),
                store: Some(Arc::new(LifecycleStore(store.clone()))),
                images: Some(Arc::new(FakeImages)),
                iso: Some(iso.clone()),
                plan: Some(plan),
                storage_dir: temp.path().to_owned(),
                nested_enabled: Some(Arc::new(|| (true, String::new()))),
                delete_iso: Some(Arc::new(|_| Box::pin(async { Ok(()) }))),
                iso_exists: Some(Arc::new(|_| false)),
                ..Default::default()
            })
            .unwrap(),
        );
        Self {
            _temp: temp,
            store,
            plan,
            hypervisor,
            manager,
            iso,
            host_id: host.id,
        }
    }

    async fn add_user(&self, name: &str) -> bento_types::User {
        let user = self
            .store
            .register_user(
                name,
                format!("{name}@example.org"),
                Some(format!("oidc-{name}")),
                self.plan.range(),
            )
            .await
            .unwrap();
        self.store
            .add_ssh_key(
                user.id,
                OWNER_KEY,
                &format!("SHA256:fp-{name}"),
                "owner@laptop",
            )
            .await
            .unwrap();
        user
    }

    async fn add_image(&self) {
        self.store
            .upsert_image(Image {
                name: "debian-13".into(),
                url: "https://example.test/debian-13".into(),
                pinned_checksum: None,
                current_checksum: None,
            })
            .await
            .unwrap();
        self.store
            .add_image_version(ImageVersion {
                checksum: "aa11".into(),
                image_name: "debian-13".into(),
                path: "/var/lib/bento/images/sha256-aa11.qcow2".into(),
                size: 1,
                fetched_at: OffsetDateTime::now_utc(),
            })
            .await
            .unwrap();
        self.store
            .set_current_checksum("debian-13", "aa11")
            .await
            .unwrap();
    }

    fn backend(&self, frontend_key: &str) -> CliBackend {
        CliBackend(Backend {
            manager: self.manager.clone(),
            store: self.store.clone(),
            host_id: self.host_id,
            frontend_key: frontend_key.into(),
            firewall: None,
        })
    }

    async fn create(
        &self,
        backend: &CliBackend,
        owner: &bento_types::User,
        name: &str,
    ) -> bento_types::Instance {
        backend
            .create(bento_cli::CreateRequest {
                owner_id: owner.id,
                name: name.into(),
                image: "debian-13".into(),
                vcpu: 2,
                memory_mib: 2048,
                disk_gib: 20,
                nested: false,
                ksm: true,
            })
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn cli_backend_create_seeds_frontend_key() {
    let env = Env::new().await;
    let owner = env.add_user("amber").await;
    env.add_image().await;
    let frontend = "ssh-ed25519 AAAAfrontend bento-frontend";
    let instance = env.create(&env.backend(frontend), &owner, "web").await;
    let seed = env
        .iso
        .0
        .lock()
        .unwrap()
        .get(&env.manager.seed_iso_path(&instance.uuid))
        .cloned()
        .unwrap();
    assert!(seed.authorized_keys.iter().any(|key| key == OWNER_KEY));
    assert!(seed.authorized_keys.iter().any(|key| key == frontend));
    assert_eq!(seed.user_name, bento_sshfront::DEFAULT_GUEST_USER);
    assert!(env.hypervisor.domain("web").is_some());
}

#[tokio::test]
async fn cli_backend_stop_start_restart() {
    let env = Env::new().await;
    let owner = env.add_user("amber").await;
    env.add_image().await;
    let backend = env.backend("");
    let instance = env.create(&backend, &owner, "web").await;
    assert_eq!(
        backend.stop(instance.clone()).await.unwrap(),
        bento_hypervisor::StopResult::Graceful
    );
    let row = env.store.instance(&instance.uuid).await.unwrap();
    assert_eq!(row.desired_state, DesiredState::Stopped);
    assert_eq!(row.state, State::Stopped);
    backend.start(instance.clone()).await.unwrap();
    assert_eq!(
        env.store
            .instance(&instance.uuid)
            .await
            .unwrap()
            .desired_state,
        DesiredState::Running
    );
    backend.restart(instance).await.unwrap();
}

#[tokio::test]
async fn cli_backend_rename_moves_domain() {
    let env = Env::new().await;
    let owner = env.add_user("amber").await;
    env.add_image().await;
    let backend = env.backend("");
    let instance = env.create(&backend, &owner, "web").await;
    backend.stop(instance.clone()).await.unwrap();
    backend.rename(instance, "api").await.unwrap();
    assert!(env.store.instance_by_name("api").await.is_ok());
    assert!(env.hypervisor.domain("web").is_none());
    assert!(env.hypervisor.domain("api").is_some());
    assert!(env.store.released_name("web").await.is_ok());
}

#[tokio::test]
async fn cli_backend_resize_fills_unchanged_fields() {
    let env = Env::new().await;
    let owner = env.add_user("amber").await;
    env.add_image().await;
    let backend = env.backend("");
    let instance = env.create(&backend, &owner, "web").await;
    backend
        .resize(
            instance.clone(),
            bento_cli::ResizeRequest {
                memory_mib: Some(4096),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let row = env.store.instance(&instance.uuid).await.unwrap();
    assert_eq!(
        (row.vcpu, row.memory_mib, row.disk_gib, row.nested),
        (2, 4096, 20, false)
    );
}

#[tokio::test]
async fn cli_backend_copy_and_remove() {
    let env = Env::new().await;
    let owner = env.add_user("amber").await;
    env.add_image().await;
    let backend = env.backend("");
    let source = env.create(&backend, &owner, "web").await;
    backend.stop(source.clone()).await.unwrap();
    let clone = backend
        .copy(
            source,
            bento_cli::CreateRequest {
                owner_id: owner.id,
                name: "web2".into(),
                image: "debian-13".into(),
                vcpu: 2,
                memory_mib: 2048,
                disk_gib: 20,
                nested: false,
                ksm: true,
            },
        )
        .await
        .unwrap();
    assert_eq!(clone.base_checksum, "aa11");
    backend.remove(clone.clone()).await.unwrap();
    assert!(matches!(
        env.store.instance(&clone.uuid).await,
        Err(StoreError::NotFound)
    ));
    assert!(env.store.released_name("web2").await.is_ok());
}

#[tokio::test]
async fn cli_backend_console_unavailable() {
    let env = Env::new().await;
    let mut stream = std::io::Cursor::new(Vec::new());
    let error = env
        .backend("")
        .console(
            bento_types::Instance {
                name: "web".into(),
                uuid: String::new(),
                owner_id: 0,
                host_id: 0,
                image_name: String::new(),
                base_checksum: String::new(),
                state: State::Stopped,
                desired_state: DesiredState::Stopped,
                address: String::new(),
                mac: String::new(),
                vcpu: 0,
                memory_mib: 0,
                disk_gib: 0,
                nested: false,
                ksm: false,
                http_port: 0,
                visibility: Visibility::Off,
                created_at: OffsetDateTime::now_utc(),
                last_seen_at: None,
            },
            &mut stream,
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("console"));
}

#[derive(Default)]
struct RecordingNetworks(Mutex<Vec<(String, String)>>);

#[async_trait]
impl NetworkEnsurer for RecordingNetworks {
    async fn ensure_network(&self, name: &str, xml: &str) -> anyhow::Result<()> {
        self.0.lock().unwrap().push((name.into(), xml.into()));
        Ok(())
    }
}

#[tokio::test]
async fn registrar_allocates_subnet_key_and_network() {
    let env = Env::new().await;
    let networks = Arc::new(RecordingNetworks::default());
    let registrar = Registrar {
        store: env.store.clone(),
        plan: env.plan,
        networks: Some(networks.clone()),
        firewall: None,
    };
    let user = registrar
        .register(bento_sshfront::Registration {
            name: "amber".into(),
            email: "amber@example.org".into(),
            public_key: OWNER_KEY.into(),
            fingerprint: "SHA256:fp".into(),
            comment: "owner@laptop".into(),
        })
        .await
        .unwrap();
    assert_eq!(user.subnet, "10.100.0.0/24");
    assert_eq!(env.store.ssh_keys_for_user(user.id).await.unwrap().len(), 1);
    let ensured = networks.0.lock().unwrap();
    assert_eq!(ensured[0].0, "bento-user-0");
    assert!(ensured[0].1.contains("bento0"));
}

#[tokio::test]
async fn auth_adapters_round_trip_and_access() {
    let env = Env::new().await;
    let owner = env.add_user("amber").await;
    let other = env.add_user("blair").await;
    env.add_image().await;
    let instance = env.create(&env.backend(""), &owner, "web").await;
    let service = bento_auth::Service::new(
        "bento.example.org",
        Arc::new(AuthUsers(env.store.clone())),
        Arc::new(AuthAccess(env.store.clone())),
        Arc::new(AuthTokens(env.store.clone())),
    );
    let (owner_plaintext, _) = service
        .mint_token(owner.id, time::Duration::ZERO)
        .await
        .unwrap();
    let (other_plaintext, _) = service
        .mint_token(other.id, time::Duration::ZERO)
        .await
        .unwrap();
    let headers = |token: &str| {
        let mut headers = http::HeaderMap::new();
        headers.insert(
            http::header::AUTHORIZATION,
            format!("Bearer {token}").parse().unwrap(),
        );
        headers
    };
    assert_eq!(
        access_status(&service, &headers(&owner_plaintext), &instance.uuid).await,
        http::StatusCode::NO_CONTENT
    );
    assert_eq!(
        access_status(&service, &headers(&other_plaintext), &instance.uuid).await,
        http::StatusCode::FORBIDDEN
    );
    assert_eq!(
        access_status(&service, &http::HeaderMap::new(), &instance.uuid).await,
        http::StatusCode::UNAUTHORIZED
    );
    let token = AuthTokens(env.store.clone())
        .token_by_hash(&bento_auth::hash_token(&owner_plaintext))
        .await
        .unwrap();
    assert!(matches!(
        token,
        bento_auth::TokenLookup::Found(_) | bento_auth::TokenLookup::Expired(_)
    ));
}

#[tokio::test]
async fn proxy_source_hides_missing_names() {
    let env = Env::new().await;
    assert!(
        ProxySource(env.store)
            .instance_by_name("ghost")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn api_and_cli_changes_reload_firewall() {
    let env = Env::new().await;
    let owner = env.add_user("amber").await;
    env.add_image().await;
    let applier = Arc::new(RecordingApplier::default());
    let firewall = Arc::new(Firewall::new(
        env.store.clone(),
        env.plan,
        applier.clone(),
        PortRange { from: 0, to: 0 },
    ));
    let cli = CliBackend(Backend {
        manager: env.manager.clone(),
        store: env.store.clone(),
        host_id: env.host_id,
        frontend_key: String::new(),
        firewall: Some(firewall.clone()),
    });
    let instance = env.create(&cli, &owner, "web").await;
    cli.set_http_port(instance.clone(), 8080).await.unwrap();
    cli.set_visibility(instance.clone(), Visibility::Public)
        .await
        .unwrap();
    {
        let applied = applier.0.lock().unwrap();
        assert_eq!(applied.len(), 2);
        assert!(applied[1].contains("8080"));
    }

    let api = ApiBackend(Backend {
        manager: env.manager.clone(),
        store: env.store.clone(),
        host_id: env.host_id,
        frontend_key: String::new(),
        firewall: Some(firewall),
    });
    api.set_http_port(&instance.uuid, 3000).await.unwrap();
    assert!(applier.0.lock().unwrap().last().unwrap().contains("3000"));
    cli.remove(instance.clone()).await.unwrap();
    assert!(
        !applier
            .0
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .contains(&instance.address)
    );
}

/// The interpreter is an async function that takes blocking handles, so
/// `CliRunner` drives it with `block_on` on a blocking thread. That
/// establishes a runtime context for the whole call, and anything on the
/// interpreter's side of the streams that reaches for the runtime again —
/// `SyncIoBridge` did, on every write — panics with "Cannot start a runtime
/// from within a runtime" and the session ends without producing a byte.
///
/// The unit tests of the interpreter cannot catch that: they pass plain
/// `Vec<u8>` handles, which never touch a runtime. It takes the real adapter
/// on a real runtime, which is what this does.
#[tokio::test]
async fn cli_runner_writes_output_from_inside_the_runtime() {
    use bento_sshfront::CLIRunner as _;

    let env = Env::new().await;
    let user = env.add_user("riley").await;
    let lifecycle = Arc::new(CliBackend(Backend {
        manager: env.manager.clone(),
        store: env.store.clone(),
        host_id: env.host_id,
        frontend_key: String::new(),
        firewall: None,
    }));
    let cli = crate::adapters::CliRunner(Arc::new(bento_cli::Cli::new(
        Arc::new(env.store.clone()),
        lifecycle,
        bento_cli::Options {
            domain: "bento.example.org".into(),
            ..Default::default()
        },
    )));

    let (client_in, server_in) = tokio::io::duplex(4096);
    let (server_out, mut client_out) = tokio::io::duplex(4096);
    let (server_err, _client_err) = tokio::io::duplex(4096);
    drop(client_in);

    let code = cli
        .run(
            user,
            vec!["help".into()],
            Box::pin(server_in),
            Box::pin(server_out),
            Box::pin(server_err),
        )
        .await;

    let mut output = Vec::new();
    tokio::io::AsyncReadExt::read_to_end(&mut client_out, &mut output)
        .await
        .unwrap();
    assert_eq!(code, 0);
    assert!(
        !output.is_empty(),
        "the help text never reached the ssh stream"
    );
}

/// End of stream on the read half is the client closing stdin, which has to
/// read as end-of-file rather than an error or a hang: `confirm` prompts read
/// a line from it.
#[test]
fn channel_reader_reports_eof_when_the_sender_is_gone() {
    use std::io::Read as _;

    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let mut reader = crate::adapters::ChannelReader {
        rx,
        chunk: Vec::new(),
        offset: 0,
    };
    tx.send(b"yes\n".to_vec()).unwrap();
    drop(tx);

    let mut got = Vec::new();
    reader.read_to_end(&mut got).unwrap();
    assert_eq!(got, b"yes\n");
    assert_eq!(reader.read(&mut [0u8; 4]).unwrap(), 0);
}

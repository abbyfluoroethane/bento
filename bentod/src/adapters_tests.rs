//! Integration tests use a real store and lifecycle manager over the
//! in-memory hypervisor and fake host tools: the same seams assembled in
//! production.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bento_api::Lifecycle as _;
use bento_auth::Provisioner as _;
use bento_auth::TokenStore as _;
use bento_cli::Lifecycle as _;
use bento_cloudinit::Seed;
use bento_hypervisor::{Definer, Fake, Hypervisor};
use bento_lifecycle::{Fleet, ISOBuilder, ProvisionSpec};
use bento_network::{Plan, PortRange};
use bento_proxy::InstanceSource as _;
use bento_sshfront::KeyLinker as _;
use bento_store::{Error as StoreError, Store};
use bento_types::{DesiredState, Image, ImageVersion, State, Visibility};
use time::OffsetDateTime;

use crate::adapters::{
    AccountProvisioner, ApiBackend, AuthAccess, AuthPairings, AuthTokens, AuthUsers, Backend,
    CliBackend, LifecycleStore, Linker, NetworkEnsurer, ProxySource, access_status,
    api_lifecycle_error,
};
use crate::firewall::Firewall;
use crate::firewall::tests::RecordingApplier;

const OWNER_KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIFoo owner@laptop";

/// Stands in for the machine that builds an instance, writing the same
/// files and defining the same domain a real one would.
struct FakeImages {
    hypervisor: Arc<Fake>,
    iso: Arc<FakeIso>,
    storage_dir: PathBuf,
}

#[async_trait]
impl Fleet for FakeImages {
    async fn provision(
        &self,
        spec: &ProvisionSpec,
    ) -> Result<bento_types::State, bento_lifecycle::DynError> {
        let uuid = &spec.instance.uuid;
        tokio::fs::write(self.storage_dir.join(format!("{uuid}.qcow2")), b"overlay").await?;
        if spec.with_seed_iso {
            self.iso
                .build(
                    &spec.seed,
                    &self.storage_dir.join(format!("{uuid}-seed.iso")),
                )
                .await?;
        }
        let xml = bento_hypervisor::domain_xml(&bento_hypervisor::DomainSpec {
            name: spec.instance.name.clone(),
            uuid: uuid.clone(),
            vcpu: spec.instance.vcpu,
            memory_mib: spec.instance.memory_mib,
            disk_path: self
                .storage_dir
                .join(format!("{uuid}.qcow2"))
                .display()
                .to_string(),
            iso_path: String::new(),
            network: spec.network.clone(),
            mac: spec.instance.mac.clone(),
            nested: spec.instance.nested,
            ksm: spec.instance.ksm,
            arch: String::new(),
        })?;
        bento_hypervisor::Hypervisor::create(self.hypervisor.as_ref(), &xml).await?;
        Ok(bento_types::State::Running)
    }

    async fn deprovision(
        &self,
        _host_id: i64,
        _instance: &bento_types::Instance,
    ) -> Result<(), bento_lifecycle::DynError> {
        Ok(())
    }

    async fn hypervisor(&self, _host_id: i64) -> Option<Arc<dyn bento_hypervisor::Hypervisor>> {
        Some(self.hypervisor.clone())
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
            .ensure_host(
                "00000000000000000000000000000001",
                "testhost",
                "qemu:///system",
            )
            .await
            .unwrap();
        let hypervisor = Arc::new(Fake::default());
        let iso = Arc::new(FakeIso::default());
        let manager = Arc::new(
            bento_lifecycle::Manager::new(bento_lifecycle::Config {
                hypervisor: Some(hypervisor.clone()),
                definer: Some(Arc::new(FakeDefiner(hypervisor.clone()))),
                store: Some(Arc::new(LifecycleStore(
                    store.clone(),
                    crate::adapters::LocalCapacity {
                        host_id: host.id,
                        capacity: bento_types::Capacity::unbounded(),
                        overcommit_ratio: 1.0,
                    },
                ))),
                // The manager and the backend must agree which machine
                // this is. A copy checks that its source runs here
                // (MULTI-NODE 13.3), and a mismatch would refuse a copy
                // of an instance that is in fact local.
                host_id: host.id,
                fleet: Some(Arc::new(FakeImages {
                    hypervisor: hypervisor.clone(),
                    iso: iso.clone(),
                    storage_dir: temp.path().to_owned(),
                })),
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
                url: "https://images.example.org/debian-13".into(),
                kind: Default::default(),
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
                kind: Default::default(),
                source_digest: None,
                fetched_at: OffsetDateTime::now_utc(),
            })
            .await
            .unwrap();
        self.store
            .set_current_checksum("debian-13", "aa11")
            .await
            .unwrap();
        // This fixture has one active host. Its local image store holds the
        // current version, so create can pass the fleet gate (MULTI-NODE 13.2).
        self.store
            .record_host_image(self.host_id, "debian-13", "aa11")
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

#[test]
fn the_fleet_image_gate_maps_to_conflict() {
    let error = api_lifecycle_error(bento_lifecycle::Error::FleetImageNotReady {
        image_name: "debian-13".into(),
        missing: "runner-b.example.org is not ready".into(),
    });
    let status = error
        .downcast_ref::<bento_api::StatusError>()
        .expect("the API adapter assigns a status");

    assert_eq!(status.http_status(), http::StatusCode::CONFLICT);
    assert!(status.to_string().contains("runner-b.example.org"));
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
                slot: None,
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
async fn provisioning_allocates_a_subnet_and_network_but_no_key() {
    let env = Env::new().await;
    let networks = Arc::new(RecordingNetworks::default());
    let provisioner = AccountProvisioner {
        store: env.store.clone(),
        plan: env.plan,
        networks: Some(networks.clone()),
        firewall: None,
    };
    let user = provisioner
        .provision(bento_auth::NewAccount {
            preferred_name: "amber".into(),
            email: "amber@example.org".into(),
            oidc_subject: "subject-amber".into(),
        })
        .await
        .unwrap();
    assert_eq!(user.name, "amber");
    assert_eq!(user.subnet, "10.100.0.0/24");
    assert_eq!(user.oidc_subject.as_deref(), Some("subject-amber"));
    // An OIDC account starts with no SSH key; keys arrive through linking.
    assert!(
        env.store
            .ssh_keys_for_user(user.id)
            .await
            .unwrap()
            .is_empty()
    );
    let ensured = networks.0.lock().unwrap();
    assert_eq!(ensured[0].0, "bento-user-0");
    assert!(ensured[0].1.contains("bento0"));
}

/// The whole of SPEC 13 over one real store: an unknown SSH key gets a
/// link, the link sends a first-time user through OIDC, that login creates
/// the account, confirming attaches the key, and the waiting SSH session
/// sees it. Each half is covered in its own crate; only here do they meet.
#[tokio::test]
async fn a_first_time_user_gets_an_account_from_oidc_and_a_key_from_the_link() {
    let env = Env::new().await;
    let networks = Arc::new(RecordingNetworks::default());
    let linker = Linker {
        store: env.store.clone(),
        base_domain: "bento.example.org".into(),
    };
    let exchanger = Arc::new(FakeExchanger::default());
    let verifier = Arc::new(FakeVerifier::default());
    let service = bento_auth::Service::new(
        "bento.example.org",
        Arc::new(AuthUsers(env.store.clone())),
        Arc::new(AuthAccess(env.store.clone())),
        Arc::new(AuthTokens(env.store.clone())),
    )
    .with_pairings(Arc::new(AuthPairings(env.store.clone())))
    .with_provisioner(Arc::new(AccountProvisioner {
        store: env.store.clone(),
        plan: env.plan,
        networks: Some(networks.clone()),
        firewall: None,
    }))
    .with_oidc(exchanger.clone(), verifier.clone());

    // 1. The frontend meets an unknown key and mints a link. No account
    //    exists yet, and none is created by this.
    let link = linker
        .begin(bento_sshfront::PairingRequest {
            public_key: OWNER_KEY.into(),
            fingerprint: "SHA256:fp".into(),
            comment: "owner@laptop".into(),
        })
        .await
        .unwrap();
    assert!(env.store.users().await.unwrap().is_empty());
    assert!(linker.linked_user(link.id).await.unwrap().is_none());

    let token = link
        .url
        .strip_prefix("https://bento.example.org/link/")
        .expect("the url is the base domain plus the link path");
    let next = format!("/link/{token}");

    // 2. Opening the link signed out sends the browser to login.
    let page = service
        .link_page_response(&http::HeaderMap::new(), token)
        .await;
    assert_eq!(page.status(), 302);
    assert_eq!(
        page.headers()[http::header::LOCATION],
        format!("/login?next={next}").as_str()
    );

    // 3. Login through the provider creates the account (SPEC 13).
    let login = service.login_response(&format!("/login?next={next}").parse().unwrap());
    let (state, nonce) = exchanger.seen();
    verifier.allow(bento_auth::Claims {
        subject: "subject-amber".into(),
        email: "amber@example.org".into(),
        preferred_username: "Amber".into(),
        nonce,
        ..Default::default()
    });
    let callback = service
        .callback_response(
            &cookie_header(&set_cookies(&login)),
            &format!("/callback?code=good-code&state={state}")
                .parse()
                .unwrap(),
        )
        .await;
    assert_eq!(callback.status(), 302, "{}", callback.body());
    assert_eq!(callback.headers()[http::header::LOCATION], next.as_str());
    let user = env.store.user_by_name("amber").await.unwrap();
    assert_eq!(user.subnet, "10.100.0.0/24");
    assert_eq!(networks.0.lock().unwrap()[0].0, "bento-user-0");

    // 4. Back on the link, now signed in, the page names the account and
    //    the fingerprint, and links nothing by itself.
    let session = cookie_header(&set_cookies(&callback));
    let page = service.link_page_response(&session, token).await;
    assert_eq!(page.status(), 200);
    assert!(page.body().contains("SHA256:fp"), "{}", page.body());
    assert!(page.body().contains("amber"));
    assert!(
        env.store
            .ssh_keys_for_user(user.id)
            .await
            .unwrap()
            .is_empty()
    );

    // 5. Confirming attaches the key, and the waiting SSH session sees it.
    let confirm = service.link_confirm_response(&session, token).await;
    assert_eq!(confirm.status(), 200, "{}", confirm.body());
    assert_eq!(
        linker
            .linked_user(link.id)
            .await
            .unwrap()
            .map(|user| user.name),
        Some("amber".to_owned())
    );
    let key = env.store.ssh_key_by_fingerprint("SHA256:fp").await.unwrap();
    assert_eq!(key.user_id, user.id);
    assert_eq!(key.public_key, OWNER_KEY);
    assert_eq!(key.comment, "owner@laptop");

    // 6. The link is spent.
    assert_eq!(
        service
            .link_confirm_response(&session, token)
            .await
            .status(),
        410
    );
    assert_eq!(env.store.ssh_keys_for_user(user.id).await.unwrap().len(), 1);
}

/// Records the state and nonce the service generated, so the test can play
/// the provider's part without one.
#[derive(Default)]
struct FakeExchanger(Mutex<(String, String)>);

impl FakeExchanger {
    fn seen(&self) -> (String, String) {
        self.0.lock().unwrap().clone()
    }
}

#[async_trait]
impl bento_auth::Exchanger for FakeExchanger {
    fn auth_code_url(&self, state: &str, nonce: &str) -> String {
        *self.0.lock().unwrap() = (state.into(), nonce.into());
        format!("https://id.example.org/authorize?state={state}")
    }

    async fn exchange(&self, code: &str) -> Result<String, bento_auth::BoxError> {
        if code == "good-code" {
            Ok("raw-token".into())
        } else {
            Err("unknown code".into())
        }
    }
}

#[derive(Default)]
struct FakeVerifier(Mutex<Option<bento_auth::Claims>>);

impl FakeVerifier {
    fn allow(&self, claims: bento_auth::Claims) {
        *self.0.lock().unwrap() = Some(claims);
    }
}

#[async_trait]
impl bento_auth::Verifier for FakeVerifier {
    async fn verify(&self, _raw: &str) -> Result<bento_auth::Claims, bento_auth::BoxError> {
        self.0
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| "no claims allowed".into())
    }
}

fn set_cookies(response: &http::Response<String>) -> Vec<String> {
    response
        .headers()
        .get_all(http::header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .filter_map(|value| value.split(';').next())
        .map(str::to_owned)
        .collect()
}

fn cookie_header(cookies: &[String]) -> http::HeaderMap {
    let mut headers = http::HeaderMap::new();
    headers.insert(
        http::header::COOKIE,
        http::HeaderValue::from_str(&cookies.join("; ")).unwrap(),
    );
    headers
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
        env.host_id,
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

/// A guest on another machine is not in this machine's libvirt, so its
/// charts stay empty unless the controller asks the machine that runs it
/// (SPEC 14.4, MULTI-NODE 20).
#[tokio::test]
async fn the_sampler_charts_a_machine_and_a_guest_it_cannot_see_locally() {
    use bento_runner::{CpuTimeSample, DomainUsage, HostSample, Samples};

    /// Answers for the other machine without a network.
    struct FakeRunner;

    #[async_trait]
    impl crate::metrics::RemoteSampler for FakeRunner {
        async fn sample(&self, _host: &bento_types::Host) -> Result<Samples, String> {
            Ok(Samples {
                host: HostSample {
                    cpu: Some(CpuTimeSample {
                        total: 1000,
                        idle: 850,
                    }),
                    memory_total_bytes: 8 * 1024 * 1024 * 1024,
                    memory_available_bytes: 6 * 1024 * 1024 * 1024,
                    storage_total_bytes: 165 * 1024 * 1024 * 1024,
                    storage_available_bytes: 154 * 1024 * 1024 * 1024,
                    cpu_count: 4,
                },
                domains: vec![DomainUsage {
                    uuid: "uuid-remote".into(),
                    cpu_time_ns: 0,
                    vcpus: 2,
                    rss_kib: Some(700 * 1024),
                    storage_used_bytes: 3 * 1024 * 1024 * 1024,
                }],
            })
        }
    }

    /// This machine runs no guest in this test, so the local path
    /// reports the machine and nothing else.
    struct NoDomains;

    #[async_trait]
    impl bento_hypervisor::DomainSampler for NoDomains {
        async fn sample(
            &self,
            _name: &str,
        ) -> Result<Option<bento_hypervisor::DomainSample>, bento_hypervisor::Error> {
            Ok(None)
        }
    }

    let env = Env::new().await;
    env.add_image().await;
    let owner = env.add_user("amber").await;
    let other = env
        .store
        .register_runner("tsukasa.example.org", "http://10.0.0.97:10443", "10.0.0.97")
        .await
        .unwrap();
    env.store
        .create_instance(
            bento_types::Instance {
                uuid: "uuid-remote".into(),
                name: "remote".into(),
                owner_id: owner.id,
                host_id: other.id,
                image_name: "debian-13".into(),
                base_checksum: "aa11".into(),
                state: State::Running,
                desired_state: DesiredState::Running,
                address: "10.100.0.2".into(),
                mac: "ba:c9:e6:00:00:09".into(),
                vcpu: 2,
                memory_mib: 2048,
                disk_gib: 20,
                nested: false,
                ksm: true,
                http_port: 80,
                visibility: Visibility::Off,
                created_at: OffsetDateTime::UNIX_EPOCH,
                last_seen_at: None,
                // No slot: this test is about reading a machine, not
                // about allocation, and a row from before subdivision
                // looks exactly like this (MULTI-NODE 7.1).
                slot: None,
            },
            std::time::Duration::from_secs(0),
            bento_types::Capacity::unbounded(),
        )
        .await
        .unwrap();

    let sampler = Arc::new(crate::metrics::Sampler::new());
    let task = crate::metrics::SamplerTask {
        sampler: sampler.clone(),
        store: env.store.clone(),
        manager: env.manager.clone(),
        domains: Arc::new(NoDomains),
        storage_dir: env._temp.path().display().to_string(),
        local_host_id: env.host_id,
        remote: Some(Arc::new(FakeRunner)),
    };
    task.tick().await;

    let window = std::time::Duration::from_secs(3600);
    let hosts = bento_api::Metrics::hosts(sampler.as_ref(), window)
        .await
        .unwrap();
    assert_eq!(hosts.len(), 2, "both machines are charted");
    let remote = hosts
        .iter()
        .find(|host| host.host_id == other.id)
        .expect("the other machine is listed");
    assert_eq!(remote.host_name, "tsukasa.example.org");
    assert_eq!(remote.memory_total_mib, 8 * 1024);
    assert_eq!(remote.cpu_count, 4);
    // Two of the eight gibibytes are free, so six are in use.
    assert_eq!(remote.memory_used_mib.last().unwrap().value, 2.0 * 1024.0);

    let metrics = bento_api::Metrics::instance(sampler.as_ref(), "uuid-remote", window)
        .await
        .unwrap();
    assert_eq!(
        metrics.memory_used_mib.last().unwrap().value,
        700.0,
        "the guest reports memory through its own machine"
    );
    assert_eq!(metrics.storage_used_gib, 3.0);

    // And the account total counts it, against the cores of the machine
    // it really runs on.
    let user = bento_api::Metrics::user(sampler.as_ref(), owner.id)
        .await
        .unwrap();
    assert_eq!(user.memory_used_mib, 700);
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

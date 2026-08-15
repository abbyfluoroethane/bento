use std::collections::HashMap;
use std::io::Cursor;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bento_hypervisor::StopResult;
use bento_store::{Error as StoreError, Usage};
use bento_types::{DesiredState, Image, Instance, Quota, Share, SshKey, State, User, Visibility};
use russh::keys::ssh_key::{HashAlg, PublicKey};
use time::OffsetDateTime;

use super::*;

#[derive(Default)]
struct StoreData {
    users: HashMap<i64, User>,
    quota: Option<Quota>,
    usage: Usage,
    instances: Vec<Instance>,
    shared: Vec<Instance>,
    access: HashMap<String, Vec<i64>>,
    shares: HashMap<String, Vec<Share>>,
    keys: Vec<SshKey>,
    images: Vec<Image>,
    added_shares: Vec<(String, i64)>,
    removed_shares: Vec<(String, i64)>,
    added_keys: Vec<SshKey>,
    deleted_keys: Vec<i64>,
}

#[derive(Default)]
struct FakeStore(Mutex<StoreData>);

#[async_trait]
impl Store for FakeStore {
    async fn user_by_id(&self, id: i64) -> Result<User, BoxError> {
        self.0
            .lock()
            .unwrap()
            .users
            .get(&id)
            .cloned()
            .ok_or_else(not_found)
    }

    async fn user_by_name(&self, name: &str) -> Result<User, BoxError> {
        self.0
            .lock()
            .unwrap()
            .users
            .values()
            .find(|user| user.name == name)
            .cloned()
            .ok_or_else(not_found)
    }

    async fn quota_for(&self, _user_id: i64) -> Result<Quota, BoxError> {
        self.0.lock().unwrap().quota.ok_or_else(not_found)
    }

    async fn usage_for(&self, _user_id: i64) -> Result<Usage, BoxError> {
        Ok(self.0.lock().unwrap().usage)
    }

    async fn instance_by_name(&self, name: &str) -> Result<Instance, BoxError> {
        self.0
            .lock()
            .unwrap()
            .instances
            .iter()
            .find(|instance| instance.name == name)
            .cloned()
            .ok_or_else(not_found)
    }

    async fn instances_by_owner(&self, owner_id: i64) -> Result<Vec<Instance>, BoxError> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .instances
            .iter()
            .filter(|instance| instance.owner_id == owner_id)
            .cloned()
            .collect())
    }

    async fn instances_shared_with(&self, _user_id: i64) -> Result<Vec<Instance>, BoxError> {
        Ok(self.0.lock().unwrap().shared.clone())
    }

    async fn instances(&self) -> Result<Vec<Instance>, BoxError> {
        Ok(self.0.lock().unwrap().instances.clone())
    }

    async fn has_access(&self, uuid: &str, user_id: i64) -> Result<bool, BoxError> {
        let data = self.0.lock().unwrap();
        Ok(data
            .instances
            .iter()
            .any(|instance| instance.uuid == uuid && instance.owner_id == user_id)
            || data
                .access
                .get(uuid)
                .is_some_and(|ids| ids.contains(&user_id)))
    }

    async fn add_share(&self, uuid: &str, user_id: i64) -> Result<(), BoxError> {
        self.0
            .lock()
            .unwrap()
            .added_shares
            .push((uuid.into(), user_id));
        Ok(())
    }

    async fn remove_share(&self, uuid: &str, user_id: i64) -> Result<(), BoxError> {
        let mut data = self.0.lock().unwrap();
        if data
            .shares
            .get(uuid)
            .is_some_and(|shares| shares.iter().any(|share| share.user_id == user_id))
        {
            data.removed_shares.push((uuid.into(), user_id));
            Ok(())
        } else {
            Err(not_found())
        }
    }

    async fn shares_for(&self, uuid: &str) -> Result<Vec<Share>, BoxError> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .shares
            .get(uuid)
            .cloned()
            .unwrap_or_default())
    }

    async fn add_ssh_key(
        &self,
        user_id: i64,
        public_key: &str,
        fingerprint: &str,
        comment: &str,
    ) -> Result<i64, BoxError> {
        let mut data = self.0.lock().unwrap();
        data.added_keys.push(SshKey {
            id: 0,
            user_id,
            public_key: public_key.into(),
            fingerprint: fingerprint.into(),
            comment: comment.into(),
            created_at: test_time(),
        });
        Ok(data.added_keys.len() as i64)
    }

    async fn ssh_keys_for_user(&self, _user_id: i64) -> Result<Vec<SshKey>, BoxError> {
        Ok(self.0.lock().unwrap().keys.clone())
    }

    async fn delete_ssh_key(&self, _user_id: i64, key_id: i64) -> Result<(), BoxError> {
        let mut data = self.0.lock().unwrap();
        if data.keys.iter().any(|key| key.id == key_id) {
            data.deleted_keys.push(key_id);
            Ok(())
        } else {
            Err(not_found())
        }
    }

    async fn images(&self) -> Result<Vec<Image>, BoxError> {
        Ok(self.0.lock().unwrap().images.clone())
    }
}

#[derive(Clone)]
enum Failure {
    Cooldown { name: String, remaining: Duration },
    Quota,
}

#[derive(Default)]
struct LifecycleData {
    failure: Option<Failure>,
    stop_result: Option<StopResult>,
    created: Vec<CreateRequest>,
    started: Vec<String>,
    stopped: Vec<String>,
    restarted: Vec<String>,
    removed: Vec<String>,
    renamed: Vec<(String, String)>,
    copied: Vec<CreateRequest>,
    resized: Vec<ResizeRequest>,
    consoled: Vec<String>,
    set_port: Option<(String, u16)>,
    set_visibility: Option<(String, Visibility)>,
}

#[derive(Default)]
struct FakeLifecycle(Mutex<LifecycleData>);

impl FakeLifecycle {
    fn failure(&self) -> Option<BoxError> {
        self.0
            .lock()
            .unwrap()
            .failure
            .clone()
            .map(|failure| match failure {
                Failure::Cooldown { name, remaining } => {
                    Box::new(StoreError::NameCooldown { name, remaining }) as BoxError
                }
                Failure::Quota => Box::new(StoreError::Quota {
                    limit: "memory",
                    used: 6144,
                    requested: 4096,
                    max: 8192,
                }),
            })
    }
}

#[async_trait]
impl Lifecycle for FakeLifecycle {
    async fn create(&self, request: CreateRequest) -> Result<Instance, BoxError> {
        if let Some(error) = self.failure() {
            return Err(error);
        }
        self.0.lock().unwrap().created.push(request.clone());
        Ok(instance(
            &format!("uuid-{}", request.name),
            &request.name,
            request.owner_id,
            State::Starting,
            "10.100.0.2",
            request.vcpu,
            request.memory_mib,
            request.disk_gib,
            Visibility::Off,
        ))
    }

    async fn start(&self, instance: Instance) -> Result<(), BoxError> {
        self.0.lock().unwrap().started.push(instance.name);
        if let Some(error) = self.failure() {
            Err(error)
        } else {
            Ok(())
        }
    }

    async fn stop(&self, instance: Instance) -> Result<StopResult, BoxError> {
        if let Some(error) = self.failure() {
            return Err(error);
        }
        let mut data = self.0.lock().unwrap();
        data.stopped.push(instance.name);
        Ok(data.stop_result.unwrap_or(StopResult::Graceful))
    }

    async fn restart(&self, instance: Instance) -> Result<(), BoxError> {
        self.0.lock().unwrap().restarted.push(instance.name);
        if let Some(error) = self.failure() {
            Err(error)
        } else {
            Ok(())
        }
    }

    async fn remove(&self, instance: Instance) -> Result<(), BoxError> {
        if let Some(error) = self.failure() {
            return Err(error);
        }
        self.0.lock().unwrap().removed.push(instance.name);
        Ok(())
    }

    async fn rename(&self, instance: Instance, new_name: &str) -> Result<(), BoxError> {
        if let Some(error) = self.failure() {
            return Err(error);
        }
        self.0
            .lock()
            .unwrap()
            .renamed
            .push((instance.name, new_name.into()));
        Ok(())
    }

    async fn copy(&self, _source: Instance, request: CreateRequest) -> Result<Instance, BoxError> {
        if let Some(error) = self.failure() {
            return Err(error);
        }
        self.0.lock().unwrap().copied.push(request.clone());
        Ok(instance(
            "uuid-copy",
            &request.name,
            request.owner_id,
            State::Stopped,
            "10.100.0.3",
            request.vcpu,
            request.memory_mib,
            request.disk_gib,
            Visibility::Off,
        ))
    }

    async fn resize(&self, _instance: Instance, request: ResizeRequest) -> Result<(), BoxError> {
        if let Some(error) = self.failure() {
            return Err(error);
        }
        self.0.lock().unwrap().resized.push(request);
        Ok(())
    }

    async fn console(&self, instance: Instance, _rw: &mut dyn ReadWrite) -> Result<(), BoxError> {
        self.0.lock().unwrap().consoled.push(instance.name);
        if let Some(error) = self.failure() {
            Err(error)
        } else {
            Ok(())
        }
    }

    async fn set_http_port(&self, instance: Instance, port: u16) -> Result<(), BoxError> {
        if let Some(error) = self.failure() {
            return Err(error);
        }
        self.0.lock().unwrap().set_port = Some((instance.uuid, port));
        Ok(())
    }

    async fn set_visibility(
        &self,
        instance: Instance,
        visibility: Visibility,
    ) -> Result<(), BoxError> {
        if let Some(error) = self.failure() {
            return Err(error);
        }
        self.0.lock().unwrap().set_visibility = Some((instance.uuid, visibility));
        Ok(())
    }
}

fn not_found() -> BoxError {
    Box::new(StoreError::NotFound)
}

fn test_time() -> OffsetDateTime {
    time::macros::datetime!(2026-08-10 12:00 UTC)
}

fn user(id: i64, name: &str) -> User {
    User {
        id,
        name: name.into(),
        email: format!("{name}@example.com"),
        oidc_subject: None,
        subnet: format!("10.100.{}.0/24", id - 1),
        created_at: test_time(),
    }
}

#[allow(clippy::too_many_arguments)]
fn instance(
    uuid: &str,
    name: &str,
    owner_id: i64,
    state: State,
    address: &str,
    vcpu: u32,
    memory_mib: i64,
    disk_gib: i64,
    visibility: Visibility,
) -> Instance {
    Instance {
        uuid: uuid.into(),
        name: name.into(),
        owner_id,
        host_id: 1,
        image_name: "debian-13".into(),
        base_checksum: "aaa".into(),
        state,
        desired_state: if state == State::Stopped {
            DesiredState::Stopped
        } else {
            DesiredState::Running
        },
        address: address.into(),
        mac: String::new(),
        vcpu,
        memory_mib,
        disk_gib,
        nested: false,
        ksm: true,
        http_port: 80,
        visibility,
        created_at: test_time(),
        last_seen_at: None,
    }
}

fn fixture() -> (Arc<FakeStore>, Arc<FakeLifecycle>, Cli) {
    let alice = user(1, "alice");
    let bob = user(2, "bob");
    let mut web = instance(
        "uuid-web",
        "web",
        1,
        State::Running,
        "10.100.0.2",
        2,
        2048,
        20,
        Visibility::Off,
    );
    web.last_seen_at = Some(test_time() - time::Duration::hours(3));
    let mut db = instance(
        "uuid-db",
        "db",
        1,
        State::Stopped,
        "10.100.0.3",
        4,
        4096,
        40,
        Visibility::Public,
    );
    db.base_checksum = "bbb".into();
    let theirs = instance(
        "uuid-theirs",
        "theirs",
        2,
        State::Stopped,
        "10.100.1.2",
        1,
        1024,
        10,
        Visibility::Off,
    );
    let store = Arc::new(FakeStore(Mutex::new(StoreData {
        users: HashMap::from([(1, alice), (2, bob)]),
        quota: Some(Quota {
            user_id: 1,
            max_instances: 4,
            max_vcpu: 8,
            max_memory_mib: 8192,
            max_disk_gib: 100,
        }),
        usage: Usage {
            instances: 2,
            vcpu: 6,
            memory_mib: 6144,
            disk_gib: 60,
        },
        instances: vec![web, db, theirs],
        access: HashMap::from([("uuid-theirs".into(), vec![1])]),
        ..StoreData::default()
    })));
    let lifecycle = Arc::new(FakeLifecycle::default());
    let cli = Cli::new(
        store.clone(),
        lifecycle.clone(),
        Options {
            domain: "bento.example.org".into(),
            default_image: "debian-13".into(),
            now: Arc::new(test_time),
            ..Options::default()
        },
    );
    (store, lifecycle, cli)
}

async fn run(cli: &Cli, acting_user: User, input: &str, args: &[&str]) -> (i32, String, String) {
    let args: Vec<String> = args.iter().map(|arg| (*arg).into()).collect();
    let mut input = Cursor::new(input.as_bytes().to_vec());
    let mut output = Vec::new();
    let mut error = Vec::new();
    let code = cli
        .run(acting_user, &args, &mut input, &mut output, &mut error)
        .await;
    (
        code,
        String::from_utf8(output).unwrap(),
        String::from_utf8(error).unwrap(),
    )
}

#[tokio::test]
async fn new_flags_and_defaults() {
    for (args, expected) in [
        (
            vec!["new", "box"],
            CreateRequest {
                owner_id: 1,
                name: "box".into(),
                image: "debian-13".into(),
                vcpu: 2,
                memory_mib: 2048,
                disk_gib: 20,
                nested: false,
                ksm: true,
            },
        ),
        (
            vec![
                "new",
                "--image",
                "ubuntu-lts",
                "--memory",
                "4G",
                "--cpu",
                "8",
                "--disk",
                "50G",
                "--nested",
                "--no-ksm",
                "box",
            ],
            CreateRequest {
                owner_id: 1,
                name: "box".into(),
                image: "ubuntu-lts".into(),
                vcpu: 8,
                memory_mib: 4096,
                disk_gib: 50,
                nested: true,
                ksm: false,
            },
        ),
    ] {
        let (_, lifecycle, cli) = fixture();
        let (code, output, error) = run(&cli, user(1, "alice"), "", &args).await;
        assert_eq!(code, 0, "{error}");
        assert_eq!(lifecycle.0.lock().unwrap().created, vec![expected]);
        assert!(output.contains("created box"));
    }
}

#[tokio::test]
async fn new_rejects_bad_input() {
    for (args, expected_code, expected_error) in [
        (vec!["new"], 2, "usage"),
        (vec!["new", "Bad_Name"], 1, "lowercase"),
        (vec!["new", "bento"], 1, "reserved"),
        (vec!["new", "--memory", "lots", "box"], 1, "memory"),
        (vec!["new", "--cpu", "0", "box"], 1, "--cpu"),
    ] {
        let (_, lifecycle, cli) = fixture();
        let (code, _, error) = run(&cli, user(1, "alice"), "", &args).await;
        assert_eq!(code, expected_code, "{error}");
        assert!(error.contains(expected_error), "{error}");
        assert!(lifecycle.0.lock().unwrap().created.is_empty());
    }
}

#[tokio::test]
async fn new_reports_cooldown() {
    let (_, lifecycle, cli) = fixture();
    lifecycle.0.lock().unwrap().failure = Some(Failure::Cooldown {
        name: "web".into(),
        remaining: Duration::from_secs(90 * 60),
    });
    let (code, _, error) = run(&cli, user(1, "alice"), "", &["new", "web"]).await;
    assert_eq!(code, 1);
    for value in ["\"web\"", "released by another user", "cooldown", "1h30m"] {
        assert!(error.contains(value), "{error}");
    }
}

#[tokio::test]
async fn new_reports_quota() {
    let (_, lifecycle, cli) = fixture();
    lifecycle.0.lock().unwrap().failure = Some(Failure::Quota);
    let (code, _, error) = run(&cli, user(1, "alice"), "", &["new", "box"]).await;
    assert_eq!(code, 1);
    assert!(
        error.contains("quota exceeded") && error.contains("memory limit is 8192"),
        "{error}"
    );
}

#[tokio::test]
async fn rm_confirmation() {
    for (input, args, expected_code, removed, prompted) in [
        ("n\n", vec!["rm", "web"], 1, false, true),
        ("", vec!["rm", "web"], 1, false, true),
        ("y\n", vec!["rm", "web"], 0, true, true),
        ("", vec!["rm", "--force", "web"], 0, true, false),
    ] {
        let (_, lifecycle, cli) = fixture();
        let (code, output, _) = run(&cli, user(1, "alice"), input, &args).await;
        assert_eq!(code, expected_code);
        assert_eq!(!lifecycle.0.lock().unwrap().removed.is_empty(), removed);
        assert_eq!(output.contains("delete instance \"web\"?"), prompted);
    }
}

#[tokio::test]
async fn rename_confirmation_gating() {
    for (args, input, expected_code, renamed, prompted) in [
        (vec!["rename", "web", "web2"], "", 0, true, false),
        (vec!["rename", "db", "db2"], "n\n", 1, false, true),
        (vec!["rename", "db", "db2"], "yes\n", 0, true, true),
    ] {
        let (_, lifecycle, cli) = fixture();
        let (code, output, _) = run(&cli, user(1, "alice"), input, &args).await;
        assert_eq!(code, expected_code);
        assert_eq!(!lifecycle.0.lock().unwrap().renamed.is_empty(), renamed);
        if prompted {
            for fact in [
                "https://db.bento.example.org/ stops working",
                "no redirect",
                "SSH user name changes",
            ] {
                assert!(output.contains(fact), "{output}");
            }
        } else {
            assert!(!output.contains("[y/N]"));
        }
    }
}

#[tokio::test]
async fn rename_propagates_cooldown() {
    let (_, lifecycle, cli) = fixture();
    lifecycle.0.lock().unwrap().failure = Some(Failure::Cooldown {
        name: "web2".into(),
        remaining: Duration::from_secs(24 * 3600),
    });
    let (code, _, error) = run(&cli, user(1, "alice"), "", &["rename", "web", "web2"]).await;
    assert_eq!(code, 1);
    assert!(
        error.contains("cooldown") && error.contains("24h"),
        "{error}"
    );
}

#[tokio::test]
async fn authorization() {
    for (acting_user, args, expected) in [
        (
            user(2, "bob"),
            vec!["start", "web"],
            "no such instance or no access: web",
        ),
        (
            user(1, "alice"),
            vec!["start", "ghost"],
            "no such instance or no access: ghost",
        ),
        (
            user(1, "alice"),
            vec!["rm", "--force", "theirs"],
            "only the owner",
        ),
        (
            user(1, "alice"),
            vec!["rename", "theirs", "mine"],
            "only the owner",
        ),
        (
            user(1, "alice"),
            vec!["visibility", "theirs", "public"],
            "only the owner",
        ),
    ] {
        let (_, lifecycle, cli) = fixture();
        let (code, _, error) = run(&cli, acting_user, "", &args).await;
        assert_eq!(code, 1);
        assert!(error.contains(expected), "{error}");
        let data = lifecycle.0.lock().unwrap();
        assert!(data.started.is_empty() && data.removed.is_empty() && data.renamed.is_empty());
    }
}

#[tokio::test]
async fn shared_user_can_start() {
    let (_, lifecycle, cli) = fixture();
    let (code, output, error) = run(&cli, user(1, "alice"), "", &["start", "theirs"]).await;
    assert_eq!(code, 0, "{error}");
    assert_eq!(lifecycle.0.lock().unwrap().started, ["theirs"]);
    assert!(output.contains("theirs is starting"));
}

#[tokio::test]
async fn start_already_running() {
    let (_, lifecycle, cli) = fixture();
    let (code, output, _) = run(&cli, user(1, "alice"), "", &["start", "web"]).await;
    assert_eq!(code, 0);
    assert!(output.contains("already running"));
    assert!(lifecycle.0.lock().unwrap().started.is_empty());
}

#[tokio::test]
async fn stop_reports_path() {
    for (result, expected) in [
        (StopResult::Graceful, "shut down after the ACPI request"),
        (StopResult::Forced, "forced off"),
        (StopResult::AlreadyStopped, "already stopped"),
    ] {
        let (_, lifecycle, cli) = fixture();
        lifecycle.0.lock().unwrap().stop_result = Some(result);
        let (code, output, _) = run(&cli, user(1, "alice"), "", &["stop", "web"]).await;
        assert_eq!(code, 0);
        assert!(output.contains(expected), "{output}");
    }
}

#[tokio::test]
async fn copy_requires_stopped_source() {
    let (_, lifecycle, cli) = fixture();
    let (code, _, error) = run(&cli, user(1, "alice"), "", &["cp", "web", "web2"]).await;
    assert_eq!(code, 1);
    assert!(error.contains("must be stopped"));
    assert!(lifecycle.0.lock().unwrap().copied.is_empty());
    let (code, output, error) = run(&cli, user(1, "alice"), "", &["cp", "db", "db2"]).await;
    assert_eq!(code, 0, "{error}");
    let copied = lifecycle.0.lock().unwrap().copied.clone();
    assert_eq!(copied[0].name, "db2");
    assert_eq!(
        (copied[0].vcpu, copied[0].memory_mib, copied[0].disk_gib),
        (4, 4096, 40)
    );
    assert!(output.contains("created db2 from db"));
}

#[tokio::test]
async fn resize() {
    let (_, lifecycle, cli) = fixture();
    let (code, output, error) = run(
        &cli,
        user(1, "alice"),
        "",
        &["resize", "--memory", "8G", "--cpu", "4", "web"],
    )
    .await;
    assert_eq!(code, 0, "{error}");
    assert!(output.contains("after a restart"));
    assert_eq!(
        lifecycle.0.lock().unwrap().resized,
        [ResizeRequest {
            vcpu: Some(4),
            memory_mib: Some(8192),
            disk_gib: None,
            nested: None
        }]
    );

    let (_, lifecycle, cli) = fixture();
    let (code, _, error) = run(
        &cli,
        user(1, "alice"),
        "",
        &["resize", "--disk", "10", "web"],
    )
    .await;
    assert_eq!(code, 1);
    assert!(error.contains("can only grow"));
    assert!(lifecycle.0.lock().unwrap().resized.is_empty());

    let (_, _, cli) = fixture();
    assert_eq!(
        run(&cli, user(1, "alice"), "", &["resize", "web"]).await.0,
        2
    );
    assert_eq!(
        run(
            &cli,
            user(1, "alice"),
            "",
            &["resize", "--nested", "--no-nested", "web"]
        )
        .await
        .0,
        2
    );
}

#[tokio::test]
async fn ls_output() {
    let (store, _, cli) = fixture();
    store.0.lock().unwrap().shared = vec![instance(
        "uuid-theirs",
        "theirs",
        2,
        State::Stopped,
        "10.100.1.2",
        0,
        0,
        0,
        Visibility::Off,
    )];
    let (code, output, error) = run(&cli, user(1, "alice"), "", &["ls"]).await;
    assert_eq!(code, 0, "{error}");
    assert_eq!(
        output,
        "instances 2/4 · vcpu 6/8 · memory 6144/8192 MiB · disk 60/100 GiB\n\
NAME  STATE    ADDRESS     IMAGE      VISIBILITY  LAST USE\n\
db    stopped  10.100.0.3  debian-13  public      never\n\
web   running  10.100.0.2  debian-13  off         3h ago\n\
\nshared with you:\n\
NAME    STATE    ADDRESS     OWNER  LAST USE\n\
theirs  stopped  10.100.1.2  bob    never\n"
    );
}

#[tokio::test]
async fn ls_unlimited_quota() {
    let (store, _, cli) = fixture();
    store.0.lock().unwrap().quota = None;
    let (code, output, _) = run(&cli, user(1, "alice"), "", &["ls"]).await;
    assert_eq!(code, 0);
    assert!(output.contains("instances 2/- · vcpu 6/-"));
}

#[tokio::test]
async fn images_output() {
    let (store, _, cli) = fixture();
    store.0.lock().unwrap().images = vec![
        Image {
            name: "ubuntu-lts".into(),
            url: "https://example.org/u".into(),
            pinned_checksum: None,
            current_checksum: Some("ccc".into()),
        },
        Image {
            name: "debian-13".into(),
            url: "https://example.org/d".into(),
            pinned_checksum: None,
            current_checksum: Some("aaa".into()),
        },
    ];
    let (code, output, error) = run(&cli, user(1, "alice"), "", &["images"]).await;
    assert_eq!(code, 0, "{error}");
    assert_eq!(
        output,
        "NAME        CURRENT CHECKSUM  ON OLDER VERSIONS\n\
debian-13   aaa               1\n\
ubuntu-lts  ccc               0\n"
    );
}

#[tokio::test]
async fn port() {
    let (_, lifecycle, cli) = fixture();
    let (code, output, _) = run(&cli, user(1, "alice"), "", &["port", "web", "3456"]).await;
    assert_eq!(code, 0);
    assert_eq!(
        lifecycle.0.lock().unwrap().set_port,
        Some(("uuid-web".into(), 3456))
    );
    assert!(output.contains("now 3456"));
    for bad in ["0", "65536", "-1", "http"] {
        let (code, _, error) = run(&cli, user(1, "alice"), "", &["port", "web", bad]).await;
        assert_eq!(code, 1);
        assert!(error.contains("not a port"));
    }
}

#[tokio::test]
async fn visibility() {
    let (_, lifecycle, cli) = fixture();
    let (code, output, _) = run(&cli, user(1, "alice"), "", &["visibility", "web", "public"]).await;
    assert_eq!(code, 0);
    assert_eq!(
        lifecycle.0.lock().unwrap().set_visibility,
        Some(("uuid-web".into(), Visibility::Public))
    );
    assert!(output.contains("anyone can reach https://web.bento.example.org/"));
    assert_eq!(
        run(&cli, user(1, "alice"), "", &["visibility", "web", "hidden"])
            .await
            .0,
        2
    );
}

#[tokio::test]
async fn share() {
    let (store, _, cli) = fixture();
    let (code, output, error) = run(&cli, user(1, "alice"), "", &["share", "web", "bob"]).await;
    assert_eq!(code, 0, "{error}");
    assert_eq!(
        store.0.lock().unwrap().added_shares,
        [("uuid-web".into(), 2)]
    );
    assert!(output.contains("bob can now use web"));

    let (store, _, cli) = fixture();
    store.0.lock().unwrap().shares.insert(
        "uuid-web".into(),
        vec![Share {
            instance_uuid: "uuid-web".into(),
            user_id: 2,
            created_at: test_time(),
        }],
    );
    let (code, output, _) = run(
        &cli,
        user(1, "alice"),
        "",
        &["share", "--revoke", "web", "bob"],
    )
    .await;
    assert_eq!(code, 0);
    assert_eq!(store.0.lock().unwrap().removed_shares.len(), 1);
    assert!(output.contains("no longer has access"));

    let (_, _, cli) = fixture();
    let (code, _, error) = run(
        &cli,
        user(1, "alice"),
        "",
        &["share", "--revoke", "web", "bob"],
    )
    .await;
    assert_eq!(code, 1);
    assert!(error.contains("has no share"));
    let (code, _, error) = run(&cli, user(1, "alice"), "", &["share", "web", "mallory"]).await;
    assert_eq!(code, 1);
    assert!(error.contains("no such user: mallory"));
    let (code, _, error) = run(&cli, user(1, "alice"), "", &["share", "web", "alice"]).await;
    assert_eq!(code, 1);
    assert!(error.contains("you own"));
}

#[tokio::test]
async fn ssh_key_add_list_remove() {
    let line =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILM+rvN+ot98qgEN796jTiQfZfG1KaT0PtFDJ/XFSqti laptop";
    let expected = PublicKey::from_openssh(line)
        .unwrap()
        .fingerprint(HashAlg::Sha256)
        .to_string();
    let (store, _, cli) = fixture();
    let (code, output, error) = run(&cli, user(1, "alice"), "", &["ssh-key", "add", line]).await;
    assert_eq!(code, 0, "{error}");
    {
        let data = store.0.lock().unwrap();
        assert_eq!(data.added_keys.len(), 1);
        assert_eq!(
            (
                data.added_keys[0].fingerprint.as_str(),
                data.added_keys[0].comment.as_str(),
                data.added_keys[0].user_id
            ),
            (expected.as_str(), "laptop", 1)
        );
    }
    assert!(output.contains(&expected));

    let (store2, _, cli2) = fixture();
    assert_eq!(
        run(
            &cli2,
            user(1, "alice"),
            &format!("{line}\n"),
            &["ssh-key", "add"]
        )
        .await
        .0,
        0
    );
    assert_eq!(store2.0.lock().unwrap().added_keys.len(), 1);
    let (code, _, error) = run(&cli, user(1, "alice"), "", &["ssh-key", "add", "not a key"]).await;
    assert_eq!(code, 1);
    assert!(error.contains("authorized_keys"));

    store.0.lock().unwrap().keys = vec![SshKey {
        id: 7,
        user_id: 1,
        public_key: line.into(),
        fingerprint: expected.clone(),
        comment: "laptop".into(),
        created_at: test_time(),
    }];
    let (code, output, _) = run(&cli, user(1, "alice"), "", &["ssh-key", "list"]).await;
    assert_eq!(code, 0);
    assert!(output.contains(&expected) && output.contains('7'));
    assert_eq!(
        run(&cli, user(1, "alice"), "", &["ssh-key", "remove", "7"])
            .await
            .0,
        0
    );
    assert_eq!(store.0.lock().unwrap().deleted_keys, [7]);
    let (code, _, error) = run(&cli, user(1, "alice"), "", &["ssh-key", "remove", "99"]).await;
    assert_eq!(code, 1);
    assert!(error.contains("no key with id 99"));
}

#[tokio::test]
async fn whoami() {
    let (_, _, cli) = fixture();
    let (code, output, _) = run(&cli, user(1, "alice"), "", &["whoami"]).await;
    assert_eq!(code, 0);
    for expected in [
        "alice",
        "alice@example.com",
        "10.100.0.0/24",
        "instances 2/4",
    ] {
        assert!(output.contains(expected), "{output}");
    }
}

#[tokio::test]
async fn console() {
    let (_, lifecycle, cli) = fixture();
    let (code, output, _) = run(&cli, user(1, "alice"), "", &["console", "web"]).await;
    assert_eq!(code, 0);
    assert_eq!(lifecycle.0.lock().unwrap().consoled, ["web"]);
    assert!(output.contains("attached to web"));
}

#[tokio::test]
async fn unknown_command() {
    let (_, _, cli) = fixture();
    let (code, _, error) = run(&cli, user(1, "alice"), "", &["destroy", "web"]).await;
    assert_eq!(code, 2);
    assert!(error.contains("unknown command \"destroy\""));
}

#[tokio::test]
async fn help_on_no_args() {
    let (_, _, cli) = fixture();
    let (code, output, _) = run(&cli, user(1, "alice"), "", &[]).await;
    assert_eq!(code, 0);
    assert!(output.contains("ssh bento.example.org <command>"));
}

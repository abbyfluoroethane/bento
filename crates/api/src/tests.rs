use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use axum::http::{HeaderMap, Method};
use bento_types::{DesiredState, Image, State as InstanceState};
use tower::ServiceExt;

use super::*;

#[derive(Default)]
struct FakeData {
    users: HashMap<i64, User>,
    quotas: HashMap<i64, Quota>,
    instances: HashMap<String, Instance>,
    shares: HashMap<String, Vec<Share>>,
    keys: HashMap<i64, Vec<SshKey>>,
    images: Vec<Image>,
    next_key_id: i64,
}

struct FakeStore {
    data: Mutex<FakeData>,
    dump_bytes: Vec<u8>,
}

impl Default for FakeStore {
    fn default() -> Self {
        Self {
            data: Mutex::new(FakeData::default()),
            dump_bytes: b"SQLite format 3\0fake".to_vec(),
        }
    }
}

fn not_found_error() -> BoxError {
    Box::new(StoreError::NotFound)
}

#[async_trait]
impl Store for FakeStore {
    async fn user_by_id(&self, id: i64) -> Result<User, BoxError> {
        self.data
            .lock()
            .unwrap()
            .users
            .get(&id)
            .cloned()
            .ok_or_else(not_found_error)
    }

    async fn user_by_name(&self, name: &str) -> Result<User, BoxError> {
        self.data
            .lock()
            .unwrap()
            .users
            .values()
            .find(|user| user.name == name)
            .cloned()
            .ok_or_else(not_found_error)
    }

    async fn quota_for(&self, user_id: i64) -> Result<Quota, BoxError> {
        self.data
            .lock()
            .unwrap()
            .quotas
            .get(&user_id)
            .copied()
            .ok_or_else(not_found_error)
    }

    async fn usage_for(&self, user_id: i64) -> Result<Usage, BoxError> {
        let data = self.data.lock().unwrap();
        let mut usage = Usage::default();
        for instance in data
            .instances
            .values()
            .filter(|instance| instance.owner_id == user_id)
        {
            usage.instances += 1;
            usage.vcpu += i64::from(instance.vcpu);
            usage.memory_mib += instance.memory_mib;
            usage.disk_gib += instance.disk_gib;
        }
        Ok(usage)
    }

    async fn instance(&self, uuid: &str) -> Result<Instance, BoxError> {
        self.data
            .lock()
            .unwrap()
            .instances
            .get(uuid)
            .cloned()
            .ok_or_else(not_found_error)
    }

    async fn instances_by_owner(&self, owner_id: i64) -> Result<Vec<Instance>, BoxError> {
        Ok(self
            .data
            .lock()
            .unwrap()
            .instances
            .values()
            .filter(|instance| instance.owner_id == owner_id)
            .cloned()
            .collect())
    }

    async fn instances_shared_with(&self, user_id: i64) -> Result<Vec<Instance>, BoxError> {
        let data = self.data.lock().unwrap();
        Ok(data
            .shares
            .iter()
            .filter(|(_, shares)| shares.iter().any(|share| share.user_id == user_id))
            .filter_map(|(uuid, _)| data.instances.get(uuid).cloned())
            .collect())
    }

    async fn instances(&self) -> Result<Vec<Instance>, BoxError> {
        Ok(self
            .data
            .lock()
            .unwrap()
            .instances
            .values()
            .cloned()
            .collect())
    }

    async fn add_share(&self, instance_uuid: &str, user_id: i64) -> Result<(), BoxError> {
        let mut data = self.data.lock().unwrap();
        let shares = data.shares.entry(instance_uuid.to_string()).or_default();
        if !shares.iter().any(|share| share.user_id == user_id) {
            shares.push(Share {
                instance_uuid: instance_uuid.to_string(),
                user_id,
                created_at: OffsetDateTime::UNIX_EPOCH,
            });
        }
        Ok(())
    }

    async fn remove_share(&self, instance_uuid: &str, user_id: i64) -> Result<(), BoxError> {
        let mut data = self.data.lock().unwrap();
        let Some(shares) = data.shares.get_mut(instance_uuid) else {
            return Err(not_found_error());
        };
        let old_len = shares.len();
        shares.retain(|share| share.user_id != user_id);
        if shares.len() == old_len {
            return Err(not_found_error());
        }
        Ok(())
    }

    async fn shares_for(&self, instance_uuid: &str) -> Result<Vec<Share>, BoxError> {
        Ok(self
            .data
            .lock()
            .unwrap()
            .shares
            .get(instance_uuid)
            .cloned()
            .unwrap_or_default())
    }

    async fn images(&self) -> Result<Vec<Image>, BoxError> {
        Ok(self.data.lock().unwrap().images.clone())
    }

    async fn add_ssh_key(
        &self,
        user_id: i64,
        public_key: &str,
        fingerprint: &str,
        comment: &str,
    ) -> Result<i64, BoxError> {
        let mut data = self.data.lock().unwrap();
        data.next_key_id += 1;
        let id = data.next_key_id;
        data.keys.entry(user_id).or_default().push(SshKey {
            id,
            user_id,
            public_key: public_key.to_string(),
            fingerprint: fingerprint.to_string(),
            comment: comment.to_string(),
            created_at: OffsetDateTime::UNIX_EPOCH,
        });
        Ok(id)
    }

    async fn ssh_keys_for_user(&self, user_id: i64) -> Result<Vec<SshKey>, BoxError> {
        Ok(self
            .data
            .lock()
            .unwrap()
            .keys
            .get(&user_id)
            .cloned()
            .unwrap_or_default())
    }

    async fn delete_ssh_key(&self, user_id: i64, key_id: i64) -> Result<(), BoxError> {
        let mut data = self.data.lock().unwrap();
        let keys = data.keys.entry(user_id).or_default();
        let old_len = keys.len();
        keys.retain(|key| key.id != key_id);
        if keys.len() == old_len {
            return Err(not_found_error());
        }
        Ok(())
    }

    async fn dump_db(&self, destination: &std::path::Path) -> Result<(), BoxError> {
        std::fs::write(destination, &self.dump_bytes).map_err(|error| Box::new(error) as BoxError)
    }
}

#[derive(Clone)]
enum Failure {
    Quota,
    Cooldown,
    NameTaken,
    Teapot,
}

struct FakeLifecycle {
    store: Arc<FakeStore>,
    calls: Mutex<Vec<String>>,
    failure: Mutex<Option<Failure>>,
}

impl FakeLifecycle {
    fn new(store: Arc<FakeStore>) -> Self {
        Self {
            store,
            calls: Mutex::new(Vec::new()),
            failure: Mutex::new(None),
        }
    }

    fn record(&self, call: String) {
        self.calls.lock().unwrap().push(call);
    }

    fn error(&self) -> Option<BoxError> {
        match self.failure.lock().unwrap().clone()? {
            Failure::Quota => Some(Box::new(StoreError::Quota {
                limit: "memory".to_string(),
                used: 6144,
                requested: 4096,
                max: 8192,
            })),
            Failure::Cooldown => Some(Box::new(StoreError::NameCooldown {
                name: "api".to_string(),
                remaining: Duration::from_secs(3 * 3600),
            })),
            Failure::NameTaken => Some(Box::new(StoreError::NameTaken)),
            Failure::Teapot => Some(Box::new(StatusError::new(
                StatusCode::IM_A_TEAPOT,
                "short and stout",
            ))),
        }
    }
}

#[async_trait]
impl Lifecycle for FakeLifecycle {
    async fn create(&self, owner: User, spec: CreateSpec) -> Result<Instance, BoxError> {
        self.record(format!("create {}", spec.name));
        if let Some(error) = self.error() {
            return Err(error);
        }
        let instance = Instance {
            uuid: format!("uuid-{}", spec.name),
            name: spec.name,
            owner_id: owner.id,
            host_id: 1,
            image_name: spec.image,
            base_checksum: String::new(),
            state: InstanceState::Starting,
            desired_state: DesiredState::Running,
            address: String::new(),
            mac: String::new(),
            vcpu: spec.vcpu,
            memory_mib: spec.memory_mib,
            disk_gib: spec.disk_gib,
            nested: spec.nested,
            ksm: spec.ksm,
            http_port: 80,
            visibility: Visibility::Off,
            created_at: OffsetDateTime::UNIX_EPOCH,
            last_seen_at: None,
        };
        self.store
            .data
            .lock()
            .unwrap()
            .instances
            .insert(instance.uuid.clone(), instance.clone());
        Ok(instance)
    }

    async fn delete(&self, uuid: &str) -> Result<(), BoxError> {
        self.record(format!("delete {uuid}"));
        if let Some(error) = self.error() {
            return Err(error);
        }
        let mut data = self.store.data.lock().unwrap();
        data.instances.remove(uuid);
        data.shares.remove(uuid);
        Ok(())
    }

    async fn start(&self, uuid: &str) -> Result<(), BoxError> {
        self.record(format!("start {uuid}"));
        self.error().map_or(Ok(()), Err)
    }

    async fn stop(&self, uuid: &str) -> Result<(), BoxError> {
        self.record(format!("stop {uuid}"));
        self.error().map_or(Ok(()), Err)
    }

    async fn restart(&self, uuid: &str) -> Result<(), BoxError> {
        self.record(format!("restart {uuid}"));
        self.error().map_or(Ok(()), Err)
    }

    async fn rename(&self, uuid: &str, new_name: &str) -> Result<(), BoxError> {
        self.record(format!("rename {uuid} {new_name}"));
        if let Some(error) = self.error() {
            return Err(error);
        }
        self.store
            .data
            .lock()
            .unwrap()
            .instances
            .get_mut(uuid)
            .unwrap()
            .name = new_name.to_string();
        Ok(())
    }

    async fn resize(&self, uuid: &str, spec: ResizeSpec) -> Result<(), BoxError> {
        self.record(format!(
            "resize {uuid} vcpu={} mem={} disk={} nested={}",
            spec.vcpu, spec.memory_mib, spec.disk_gib, spec.nested
        ));
        if let Some(error) = self.error() {
            return Err(error);
        }
        let mut data = self.store.data.lock().unwrap();
        let instance = data.instances.get_mut(uuid).unwrap();
        instance.vcpu = spec.vcpu;
        instance.memory_mib = spec.memory_mib;
        instance.disk_gib = spec.disk_gib;
        instance.nested = spec.nested;
        Ok(())
    }

    async fn set_http_port(&self, uuid: &str, port: u16) -> Result<(), BoxError> {
        self.record(format!("port {uuid} {port}"));
        if let Some(error) = self.error() {
            return Err(error);
        }
        self.store
            .data
            .lock()
            .unwrap()
            .instances
            .get_mut(uuid)
            .unwrap()
            .http_port = port;
        Ok(())
    }

    async fn set_visibility(&self, uuid: &str, visibility: Visibility) -> Result<(), BoxError> {
        self.record(format!("visibility {uuid} {visibility}"));
        if let Some(error) = self.error() {
            return Err(error);
        }
        self.store
            .data
            .lock()
            .unwrap()
            .instances
            .get_mut(uuid)
            .unwrap()
            .visibility = visibility;
        Ok(())
    }
}

struct FakeAuth(Mutex<Option<User>>);

#[async_trait]
impl Authenticator for FakeAuth {
    async fn user_from_headers(&self, _headers: &HeaderMap) -> Result<User, BoxError> {
        self.0
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| Box::new(std::io::Error::other("no session")) as BoxError)
    }
}

fn user(id: i64, name: &str, email: &str, created_at: i64) -> User {
    User {
        id,
        name: name.to_string(),
        email: email.to_string(),
        oidc_subject: None,
        subnet: format!("10.42.{id}.0/24"),
        created_at: OffsetDateTime::from_unix_timestamp(created_at).unwrap(),
    }
}

fn instance(
    uuid: &str,
    name: &str,
    owner_id: i64,
    state: InstanceState,
    desired_state: DesiredState,
    resources: (u32, i64, i64),
) -> Instance {
    Instance {
        uuid: uuid.to_string(),
        name: name.to_string(),
        owner_id,
        host_id: 1,
        image_name: "debian-13".to_string(),
        base_checksum: "aaa".to_string(),
        state,
        desired_state,
        address: if name == "web" {
            "10.42.0.2".to_string()
        } else {
            "10.42.1.2".to_string()
        },
        mac: if name == "web" {
            "ba:c9:e6:00:00:01".to_string()
        } else {
            String::new()
        },
        vcpu: resources.0,
        memory_mib: resources.1,
        disk_gib: resources.2,
        nested: false,
        ksm: true,
        http_port: 80,
        visibility: Visibility::Off,
        created_at: OffsetDateTime::UNIX_EPOCH,
        last_seen_at: None,
    }
}

struct Fixture {
    store: Arc<FakeStore>,
    lifecycle: Arc<FakeLifecycle>,
    auth: Arc<FakeAuth>,
    app: Router,
    alice: User,
    bob: User,
}

fn fixture() -> Fixture {
    let alice = user(1, "alice", "alice@example.com", 1000);
    let bob = user(2, "bob", "bob@example.com", 0);
    let store = Arc::new(FakeStore::default());
    {
        let mut data = store.data.lock().unwrap();
        data.users.insert(alice.id, alice.clone());
        data.users.insert(bob.id, bob.clone());
        data.instances.insert(
            "uuid-web".to_string(),
            instance(
                "uuid-web",
                "web",
                alice.id,
                InstanceState::Running,
                DesiredState::Running,
                (2, 2048, 20),
            ),
        );
        data.instances.insert(
            "uuid-db".to_string(),
            instance(
                "uuid-db",
                "db",
                bob.id,
                InstanceState::Stopped,
                DesiredState::Stopped,
                (1, 1024, 10),
            ),
        );
        data.shares.insert(
            "uuid-db".to_string(),
            vec![Share {
                instance_uuid: "uuid-db".to_string(),
                user_id: alice.id,
                created_at: OffsetDateTime::UNIX_EPOCH,
            }],
        );
    }
    let lifecycle = Arc::new(FakeLifecycle::new(store.clone()));
    let auth = Arc::new(FakeAuth(Mutex::new(Some(alice.clone()))));
    let app = router(Config {
        store: store.clone(),
        lifecycle: lifecycle.clone(),
        auth: auth.clone(),
        is_operator: Some(Arc::new(|user| user.id == 1)),
        db_path: "/var/lib/bento/bento.db".to_string(),
    });
    Fixture {
        store,
        lifecycle,
        auth,
        app,
        alice,
        bob,
    }
}

struct TestResponse {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: Vec<u8>,
}

async fn request(app: &Router, method: Method, path: &str, body: &str) -> TestResponse {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    TestResponse {
        status,
        headers,
        body,
    }
}

fn decode<T: for<'de> Deserialize<'de>>(response: &TestResponse) -> T {
    serde_json::from_slice(&response.body).unwrap_or_else(|error| {
        panic!(
            "decoding {:?}: {error}",
            String::from_utf8_lossy(&response.body)
        )
    })
}

#[tokio::test]
async fn authentication_is_required_on_every_route() {
    let fixture = fixture();
    *fixture.auth.0.lock().unwrap() = None;
    let routes = [
        (Method::GET, "/api/whoami"),
        (Method::GET, "/api/instances"),
        (Method::POST, "/api/instances"),
        (Method::GET, "/api/instances/uuid-web"),
        (Method::DELETE, "/api/instances/uuid-web"),
        (Method::POST, "/api/instances/uuid-web/start"),
        (Method::POST, "/api/instances/uuid-web/stop"),
        (Method::POST, "/api/instances/uuid-web/restart"),
        (Method::POST, "/api/instances/uuid-web/rename"),
        (Method::POST, "/api/instances/uuid-web/resize"),
        (Method::POST, "/api/instances/uuid-web/port"),
        (Method::POST, "/api/instances/uuid-web/visibility"),
        (Method::GET, "/api/instances/uuid-web/shares"),
        (Method::POST, "/api/instances/uuid-web/shares"),
        (Method::DELETE, "/api/instances/uuid-web/shares/bob"),
        (Method::GET, "/api/images"),
        (Method::GET, "/api/ssh-keys"),
        (Method::POST, "/api/ssh-keys"),
        (Method::DELETE, "/api/ssh-keys/1"),
        (Method::GET, "/api/db.sqlite"),
    ];
    for (method, path) in routes {
        let response = request(&fixture.app, method, path, "").await;
        assert_eq!(response.status, StatusCode::UNAUTHORIZED, "{path}");
        assert!(
            response
                .headers
                .get(header::CONTENT_TYPE)
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("application/json"),
            "{path}"
        );
    }
    assert!(fixture.lifecycle.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn unknown_api_route_is_a_json_404() {
    let fixture = fixture();
    let response = request(&fixture.app, Method::GET, "/api/nope", "").await;
    assert_eq!(response.status, StatusCode::NOT_FOUND);
    let body: ErrorBody = decode(&response);
    assert!(!body.error.is_empty());
}

#[tokio::test]
async fn whoami_reports_identity_quota_usage_and_operator_fields() {
    let fixture = fixture();
    fixture.store.data.lock().unwrap().quotas.insert(
        fixture.alice.id,
        Quota {
            user_id: 1,
            max_instances: 5,
            max_vcpu: 8,
            max_memory_mib: 8192,
            max_disk_gib: 100,
        },
    );
    let response = request(&fixture.app, Method::GET, "/api/whoami", "").await;
    assert_eq!(response.status, StatusCode::OK);
    let body: WhoamiResponse = decode(&response);
    assert_eq!(body.user.name, "alice");
    assert_eq!(body.user.email, "alice@example.com");
    assert_eq!(body.quota.as_ref().unwrap().max_instances, 5);
    assert_eq!(body.quota.as_ref().unwrap().max_disk_gib, 100);
    assert_eq!(body.usage.instances, 1);
    assert_eq!(body.usage.vcpu, 2);
    assert_eq!(body.usage.memory_mib, 2048);
    assert_eq!(body.usage.disk_gib, 20);
    assert!(body.operator);
    assert_eq!(body.db_path, "/var/lib/bento/bento.db");

    *fixture.auth.0.lock().unwrap() = Some(fixture.bob.clone());
    let response = request(&fixture.app, Method::GET, "/api/whoami", "").await;
    let body: WhoamiResponse = decode(&response);
    assert!(!body.operator);
    assert!(body.db_path.is_empty());
    assert!(body.quota.is_none());
    assert!(!String::from_utf8_lossy(&response.body).contains("db_path"));
}

#[tokio::test]
async fn instance_list_combines_sorts_and_marks_owned_and_shared_rows() {
    let fixture = fixture();
    fixture.store.data.lock().unwrap().quotas.insert(
        fixture.alice.id,
        Quota {
            user_id: 1,
            max_instances: 5,
            max_vcpu: 8,
            max_memory_mib: 8192,
            max_disk_gib: 100,
        },
    );
    let response = request(&fixture.app, Method::GET, "/api/instances", "").await;
    assert_eq!(response.status, StatusCode::OK);
    let body: InstanceListResponse = decode(&response);
    assert_eq!(body.instances.len(), 2);
    assert_eq!(body.instances[0].name, "db");
    assert_eq!(body.instances[1].name, "web");
    assert!(body.instances[0].shared_with_me);
    assert_eq!(body.instances[0].owner, "bob");
    assert!(!body.instances[1].shared_with_me);
    assert_eq!(body.instances[1].owner, "alice");
    assert_eq!(body.quota.unwrap().max_instances, 5);
    assert_eq!(body.usage.instances, 1);
}

#[tokio::test]
async fn create_validates_input_defaults_ksm_and_maps_typed_errors() {
    let cases = [
        (
            r#"{"name":"api","image":"debian-13","vcpu":2,"memory_mib":2048,"disk_gib":20}"#,
            StatusCode::CREATED,
            true,
        ),
        (
            r#"{"name":"noksm","image":"debian-13","ksm":false}"#,
            StatusCode::CREATED,
            true,
        ),
        (
            r#"{"name":"Web","image":"debian-13"}"#,
            StatusCode::BAD_REQUEST,
            false,
        ),
        (
            r#"{"name":"-web","image":"debian-13"}"#,
            StatusCode::BAD_REQUEST,
            false,
        ),
        (
            r#"{"name":"","image":"debian-13"}"#,
            StatusCode::BAD_REQUEST,
            false,
        ),
        (r#"{"name":"api"}"#, StatusCode::BAD_REQUEST, false),
        (
            r#"{"name":"api","image":"debian-13","memory_mib":-1}"#,
            StatusCode::BAD_REQUEST,
            false,
        ),
        (
            r#"{"name":"api","image":"debian-13","bogus":1}"#,
            StatusCode::BAD_REQUEST,
            false,
        ),
    ];
    for (body, expected, called) in cases {
        let fixture = fixture();
        let response = request(&fixture.app, Method::POST, "/api/instances", body).await;
        assert_eq!(response.status, expected, "{body}");
        assert_eq!(
            !fixture.lifecycle.calls.lock().unwrap().is_empty(),
            called,
            "{body}"
        );
    }

    let fixture = fixture();
    let response = request(
        &fixture.app,
        Method::POST,
        "/api/instances",
        r#"{"name":"api","image":"debian-13"}"#,
    )
    .await;
    let instance: InstanceJson = decode(&response);
    assert!(instance.ksm);

    for (failure, status) in [
        (Failure::Quota, StatusCode::CONFLICT),
        (Failure::Cooldown, StatusCode::CONFLICT),
        (Failure::NameTaken, StatusCode::CONFLICT),
        (Failure::Teapot, StatusCode::IM_A_TEAPOT),
    ] {
        let fixture = self::fixture();
        *fixture.lifecycle.failure.lock().unwrap() = Some(failure.clone());
        let response = request(
            &fixture.app,
            Method::POST,
            "/api/instances",
            r#"{"name":"api","image":"debian-13"}"#,
        )
        .await;
        assert_eq!(response.status, status);
        let body: ErrorBody = decode(&response);
        match failure {
            Failure::Quota => {
                let detail = body.quota.unwrap();
                assert_eq!(detail.limit, "memory");
                assert_eq!(detail.max, 8192);
            }
            Failure::Cooldown => {
                assert_eq!(body.cooldown_seconds, 3 * 3600);
                assert!(body.error.contains("cooldown"));
            }
            Failure::Teapot => assert_eq!(body.error, "short and stout"),
            Failure::NameTaken => {}
        }
    }
}

#[tokio::test]
async fn mutation_authorization_uses_uuid_and_hides_strangers() {
    for (uuid, expected) in [
        ("uuid-web", StatusCode::ACCEPTED),
        ("uuid-db", StatusCode::FORBIDDEN),
        ("uuid-nope", StatusCode::NOT_FOUND),
    ] {
        let fixture = fixture();
        let response = request(
            &fixture.app,
            Method::POST,
            &format!("/api/instances/{uuid}/start"),
            "",
        )
        .await;
        assert_eq!(response.status, expected, "{uuid}");
    }
    let fixture = fixture();
    fixture.store.data.lock().unwrap().instances.insert(
        "uuid-secret".to_string(),
        instance(
            "uuid-secret",
            "secret",
            fixture.bob.id,
            InstanceState::Stopped,
            DesiredState::Stopped,
            (1, 512, 5),
        ),
    );
    let response = request(
        &fixture.app,
        Method::POST,
        "/api/instances/uuid-secret/stop",
        "",
    )
    .await;
    assert_eq!(response.status, StatusCode::NOT_FOUND);

    let response = request(&fixture.app, Method::GET, "/api/instances/uuid-db", "").await;
    assert_eq!(response.status, StatusCode::OK);
    let body: InstanceJson = decode(&response);
    assert!(body.shared_with_me);
    assert_eq!(body.owner, "bob");
}

#[tokio::test]
async fn lifecycle_actions_return_accepted_and_call_the_requested_action() {
    for action in ["start", "stop", "restart"] {
        let fixture = fixture();
        let response = request(
            &fixture.app,
            Method::POST,
            &format!("/api/instances/uuid-web/{action}"),
            "",
        )
        .await;
        assert_eq!(response.status, StatusCode::ACCEPTED);
        assert_eq!(
            fixture.lifecycle.calls.lock().unwrap().as_slice(),
            &[format!("{action} uuid-web")]
        );
        let body: serde_json::Value = decode(&response);
        assert_eq!(body, serde_json::json!({"uuid":"uuid-web","action":action}));
    }
}

#[tokio::test]
async fn delete_is_explicit_and_removes_only_the_named_uuid() {
    let fixture = fixture();
    let response = request(&fixture.app, Method::DELETE, "/api/instances/uuid-web", "").await;
    assert_eq!(response.status, StatusCode::NO_CONTENT);
    assert_eq!(
        fixture.lifecycle.calls.lock().unwrap().as_slice(),
        &["delete uuid-web"]
    );
    let data = fixture.store.data.lock().unwrap();
    assert!(!data.instances.contains_key("uuid-web"));
    assert!(data.instances.contains_key("uuid-db"));
}

#[tokio::test]
async fn rename_returns_the_refreshed_row_and_rejects_bad_names() {
    let fixture = fixture();
    let response = request(
        &fixture.app,
        Method::POST,
        "/api/instances/uuid-web/rename",
        r#"{"new_name":"web2"}"#,
    )
    .await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(decode::<InstanceJson>(&response).name, "web2");
    let response = request(
        &fixture.app,
        Method::POST,
        "/api/instances/uuid-web/rename",
        r#"{"new_name":"Bad_Name"}"#,
    )
    .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn resize_fills_partial_specs_rejects_shrink_and_accepts_nested_toggle() {
    let fixture = fixture();
    let response = request(
        &fixture.app,
        Method::POST,
        "/api/instances/uuid-web/resize",
        r#"{"memory_mib":4096}"#,
    )
    .await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(
        fixture.lifecycle.calls.lock().unwrap().as_slice(),
        &["resize uuid-web vcpu=2 mem=4096 disk=20 nested=false"]
    );

    let fixture = self::fixture();
    let response = request(
        &fixture.app,
        Method::POST,
        "/api/instances/uuid-web/resize",
        r#"{"disk_gib":10}"#,
    )
    .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert!(fixture.lifecycle.calls.lock().unwrap().is_empty());

    let fixture = self::fixture();
    let response = request(
        &fixture.app,
        Method::POST,
        "/api/instances/uuid-web/resize",
        r#"{"nested":true}"#,
    )
    .await;
    assert_eq!(response.status, StatusCode::OK);
    assert!(fixture.lifecycle.calls.lock().unwrap()[0].contains("nested=true"));
}

#[tokio::test]
async fn port_range_is_checked_and_the_lifecycle_updates_the_row() {
    for (body, expected) in [
        (r#"{"port":8080}"#, StatusCode::OK),
        (r#"{"port":0}"#, StatusCode::BAD_REQUEST),
        (r#"{"port":70000}"#, StatusCode::BAD_REQUEST),
    ] {
        let fixture = fixture();
        let response = request(
            &fixture.app,
            Method::POST,
            "/api/instances/uuid-web/port",
            body,
        )
        .await;
        assert_eq!(response.status, expected);
        if expected == StatusCode::OK {
            assert_eq!(decode::<InstanceJson>(&response).http_port, 8080);
            assert_eq!(
                fixture.lifecycle.calls.lock().unwrap().as_slice(),
                &["port uuid-web 8080"]
            );
        }
    }
}

#[tokio::test]
async fn visibility_is_validated_and_changed_through_the_lifecycle() {
    let fixture = fixture();
    let response = request(
        &fixture.app,
        Method::POST,
        "/api/instances/uuid-web/visibility",
        r#"{"visibility":"public"}"#,
    )
    .await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(
        fixture
            .store
            .data
            .lock()
            .unwrap()
            .instances
            .get("uuid-web")
            .unwrap()
            .visibility,
        Visibility::Public
    );
    assert_eq!(
        fixture.lifecycle.calls.lock().unwrap().as_slice(),
        &["visibility uuid-web public"]
    );
    let response = request(
        &fixture.app,
        Method::POST,
        "/api/instances/uuid-web/visibility",
        r#"{"visibility":"hidden"}"#,
    )
    .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn shares_can_be_added_listed_and_removed_only_by_the_owner() {
    let fixture = fixture();
    let response = request(
        &fixture.app,
        Method::POST,
        "/api/instances/uuid-web/shares",
        r#"{"user":"bob"}"#,
    )
    .await;
    assert_eq!(response.status, StatusCode::CREATED);
    let response = request(
        &fixture.app,
        Method::GET,
        "/api/instances/uuid-web/shares",
        "",
    )
    .await;
    let shares: Vec<ShareJson> = decode(&response);
    assert_eq!(shares.len(), 1);
    assert_eq!(shares[0].user, "bob");
    let response = request(
        &fixture.app,
        Method::DELETE,
        "/api/instances/uuid-web/shares/bob",
        "",
    )
    .await;
    assert_eq!(response.status, StatusCode::NO_CONTENT);
    let response = request(
        &fixture.app,
        Method::GET,
        "/api/instances/uuid-web/shares",
        "",
    )
    .await;
    assert!(decode::<Vec<ShareJson>>(&response).is_empty());

    let fixture = self::fixture();
    let response = request(
        &fixture.app,
        Method::POST,
        "/api/instances/uuid-web/shares",
        r#"{"user":"mallory"}"#,
    )
    .await;
    assert_eq!(response.status, StatusCode::NOT_FOUND);
    let response = request(
        &fixture.app,
        Method::POST,
        "/api/instances/uuid-web/shares",
        r#"{"user":"alice"}"#,
    )
    .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    let response = request(
        &fixture.app,
        Method::POST,
        "/api/instances/uuid-db/shares",
        r#"{"user":"alice"}"#,
    )
    .await;
    assert_eq!(response.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn images_count_instances_on_old_versions() {
    let fixture = fixture();
    fixture.store.data.lock().unwrap().images = vec![
        Image {
            name: "debian-13".to_string(),
            url: "https://example.com/d13.qcow2".to_string(),
            pinned_checksum: None,
            current_checksum: Some("bbb".to_string()),
        },
        Image {
            name: "fedora-42".to_string(),
            url: "https://example.com/f42.qcow2".to_string(),
            pinned_checksum: Some("ccc".to_string()),
            current_checksum: Some("ccc".to_string()),
        },
    ];
    let response = request(&fixture.app, Method::GET, "/api/images", "").await;
    assert_eq!(response.status, StatusCode::OK);
    let images: Vec<ImageJson> = decode(&response);
    let by_name: HashMap<_, _> = images
        .into_iter()
        .map(|image| (image.name.clone(), image))
        .collect();
    assert_eq!(by_name["debian-13"].instances_on_older_versions, 2);
    assert_eq!(by_name["fedora-42"].instances_on_older_versions, 0);
    assert_eq!(by_name["debian-13"].pinned_checksum, "");
}

const TEST_PUBLIC_KEY: &str = concat!(
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILM+rvN+ot98qgEN796jTiQfZfG1KaT0PtFDJ/",
    "XFSqti alice@laptop"
);

#[tokio::test]
async fn ssh_keys_are_validated_fingerprinted_listed_and_scoped_on_delete() {
    let fixture = fixture();
    let response = request(
        &fixture.app,
        Method::POST,
        "/api/ssh-keys",
        &serde_json::json!({"public_key": TEST_PUBLIC_KEY}).to_string(),
    )
    .await;
    assert_eq!(response.status, StatusCode::CREATED);
    let key: SshKeyJson = decode(&response);
    assert!(key.fingerprint.starts_with("SHA256:"));
    assert_eq!(key.comment, "alice@laptop");

    let response = request(&fixture.app, Method::GET, "/api/ssh-keys", "").await;
    let keys: Vec<SshKeyJson> = decode(&response);
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].id, key.id);
    let response = request(
        &fixture.app,
        Method::DELETE,
        &format!("/api/ssh-keys/{}", key.id),
        "",
    )
    .await;
    assert_eq!(response.status, StatusCode::NO_CONTENT);

    let fixture = self::fixture();
    let response = request(
        &fixture.app,
        Method::POST,
        "/api/ssh-keys",
        r#"{"public_key":"not a key"}"#,
    )
    .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    let response = request(
        &fixture.app,
        Method::POST,
        "/api/ssh-keys",
        &serde_json::json!({"public_key": TEST_PUBLIC_KEY, "comment": "work"}).to_string(),
    )
    .await;
    assert_eq!(decode::<SshKeyJson>(&response).comment, "work");
    let response = request(
        &fixture.app,
        Method::DELETE,
        "/api/ssh-keys/not-a-number",
        "",
    )
    .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn database_download_is_a_consistent_operator_only_snapshot() {
    let fixture = fixture();
    let response = request(&fixture.app, Method::GET, "/api/db.sqlite", "").await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(
        response.headers.get(header::CONTENT_TYPE).unwrap(),
        "application/vnd.sqlite3"
    );
    assert!(
        response
            .headers
            .get(header::CONTENT_DISPOSITION)
            .unwrap()
            .to_str()
            .unwrap()
            .contains("attachment")
    );
    assert_eq!(response.body, fixture.store.dump_bytes);

    *fixture.auth.0.lock().unwrap() = Some(fixture.bob.clone());
    let response = request(&fixture.app, Method::GET, "/api/db.sqlite", "").await;
    assert_eq!(response.status, StatusCode::FORBIDDEN);

    let app = router(Config {
        store: fixture.store.clone(),
        lifecycle: fixture.lifecycle.clone(),
        auth: fixture.auth.clone(),
        is_operator: None,
        db_path: String::new(),
    });
    *fixture.auth.0.lock().unwrap() = Some(fixture.alice.clone());
    let response = request(&app, Method::GET, "/api/db.sqlite", "").await;
    assert_eq!(response.status, StatusCode::FORBIDDEN);
}

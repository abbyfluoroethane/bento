use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bento_types::{Pairing, Token, User};
use time::{Duration, OffsetDateTime, macros::datetime};

use crate::{
    AccessStore, BoxError, Claims, Exchanger, NewAccount, PairingStore, Provisioner, Service,
    TokenLookup, TokenStore, UserStore, Verifier,
};

pub(crate) const TEST_EPOCH: OffsetDateTime = datetime!(2026-08-10 12:00 UTC);

#[derive(Clone)]
pub(crate) struct FakeClock(Arc<Mutex<OffsetDateTime>>);

impl FakeClock {
    pub(crate) fn new(now: OffsetDateTime) -> Self {
        Self(Arc::new(Mutex::new(now)))
    }

    pub(crate) fn now(&self) -> OffsetDateTime {
        *self.0.lock().unwrap()
    }

    pub(crate) fn advance(&self, duration: Duration) {
        let mut now = self.0.lock().unwrap();
        *now += duration;
    }
}

pub(crate) struct FakeUserStore {
    users: Mutex<HashMap<String, User>>,
    failed: Mutex<bool>,
}

impl FakeUserStore {
    fn new() -> Self {
        Self {
            users: Mutex::new(HashMap::new()),
            failed: Mutex::new(false),
        }
    }

    pub(crate) fn insert(&self, subject: &str, id: i64, name: &str) {
        self.users.lock().unwrap().insert(
            subject.into(),
            User {
                id,
                name: name.into(),
                email: format!("{name}@example.org"),
                oidc_subject: Some(subject.into()),
                subnet: "10.100.0.0/24".into(),
                created_at: TEST_EPOCH,
            },
        );
    }
}

#[async_trait]
impl UserStore for FakeUserStore {
    async fn user_by_oidc_subject(
        &self,
        subject: &str,
    ) -> std::result::Result<Option<User>, BoxError> {
        if *self.failed.lock().unwrap() {
            return Err(Box::new(io::Error::other("db is on fire")));
        }
        Ok(self.users.lock().unwrap().get(subject).cloned())
    }

    async fn user_by_id(&self, id: i64) -> std::result::Result<Option<User>, BoxError> {
        if *self.failed.lock().unwrap() {
            return Err(Box::new(io::Error::other("db is on fire")));
        }
        Ok(self
            .users
            .lock()
            .unwrap()
            .values()
            .find(|user| user.id == id)
            .cloned())
    }
}

/// Records what it was asked to create and hands back a plausible user.
pub(crate) struct FakeProvisioner {
    created: Mutex<Vec<NewAccount>>,
    fail: Mutex<bool>,
    users: Arc<FakeUserStore>,
}

impl FakeProvisioner {
    pub(crate) fn new(users: Arc<FakeUserStore>) -> Self {
        Self {
            created: Mutex::new(Vec::new()),
            fail: Mutex::new(false),
            users,
        }
    }

    pub(crate) fn created(&self) -> Vec<NewAccount> {
        self.created.lock().unwrap().clone()
    }

    pub(crate) fn fail(&self) {
        *self.fail.lock().unwrap() = true;
    }
}

#[async_trait]
impl Provisioner for FakeProvisioner {
    async fn provision(&self, account: NewAccount) -> std::result::Result<User, BoxError> {
        if *self.fail.lock().unwrap() {
            return Err(Box::new(io::Error::other("no subnets left")));
        }
        let id = 100 + self.created.lock().unwrap().len() as i64;
        self.created.lock().unwrap().push(account.clone());
        self.users
            .insert(&account.oidc_subject, id, &account.preferred_name);
        Ok(self
            .users
            .user_by_oidc_subject(&account.oidc_subject)
            .await?
            .expect("just inserted"))
    }
}

#[derive(Default)]
pub(crate) struct FakePairingStore {
    pairings: Mutex<HashMap<String, Pairing>>,
    linked: Mutex<Vec<(i64, i64)>>,
}

impl FakePairingStore {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Adds a live pairing whose link token is `token`.
    pub(crate) fn insert(&self, token: &str, expires_at: OffsetDateTime) -> Pairing {
        let mut pairings = self.pairings.lock().unwrap();
        let pairing = Pairing {
            id: pairings.len() as i64 + 1,
            token_hash: crate::hash_token(token),
            public_key: "ssh-ed25519 AAAAC3Nz key".into(),
            fingerprint: format!("SHA256:fingerprint-of-{token}"),
            comment: "riley@laptop".into(),
            created_at: TEST_EPOCH,
            expires_at,
            linked_user_id: None,
        };
        pairings.insert(pairing.token_hash.clone(), pairing.clone());
        pairing
    }

    pub(crate) fn linked(&self) -> Vec<(i64, i64)> {
        self.linked.lock().unwrap().clone()
    }
}

#[async_trait]
impl PairingStore for FakePairingStore {
    async fn pairing_by_token_hash(
        &self,
        token_hash: &str,
    ) -> std::result::Result<Option<Pairing>, BoxError> {
        Ok(self.pairings.lock().unwrap().get(token_hash).cloned())
    }

    async fn link_pairing(&self, id: i64, user_id: i64) -> std::result::Result<bool, BoxError> {
        let mut pairings = self.pairings.lock().unwrap();
        let Some(pairing) = pairings.values_mut().find(|pairing| pairing.id == id) else {
            return Ok(false);
        };
        if pairing.linked_user_id.is_some() {
            return Ok(false);
        }
        pairing.linked_user_id = Some(user_id);
        self.linked.lock().unwrap().push((id, user_id));
        Ok(true)
    }
}

pub(crate) struct FakeAccessStore {
    allowed: Mutex<HashMap<String, HashSet<i64>>>,
    failed: Mutex<bool>,
}

impl FakeAccessStore {
    fn new() -> Self {
        Self {
            allowed: Mutex::new(HashMap::new()),
            failed: Mutex::new(false),
        }
    }

    pub(crate) fn grant(&self, instance_uuid: &str, user_id: i64) {
        self.allowed
            .lock()
            .unwrap()
            .entry(instance_uuid.into())
            .or_default()
            .insert(user_id);
    }

    pub(crate) fn revoke(&self, instance_uuid: &str, user_id: i64) {
        if let Some(users) = self.allowed.lock().unwrap().get_mut(instance_uuid) {
            users.remove(&user_id);
        }
    }

    pub(crate) fn fail(&self) {
        *self.failed.lock().unwrap() = true;
    }
}

#[async_trait]
impl AccessStore for FakeAccessStore {
    async fn has_access(
        &self,
        instance_uuid: &str,
        user_id: i64,
    ) -> std::result::Result<bool, BoxError> {
        if *self.failed.lock().unwrap() {
            return Err(Box::new(io::Error::other("db is on fire")));
        }
        Ok(self
            .allowed
            .lock()
            .unwrap()
            .get(instance_uuid)
            .is_some_and(|users| users.contains(&user_id)))
    }
}

pub(crate) struct FakeTokenStore {
    state: Mutex<FakeTokenState>,
}

#[derive(Default)]
struct FakeTokenState {
    next_id: i64,
    by_hash: HashMap<String, Token>,
    created: Vec<Token>,
    reported_expired: HashSet<i64>,
}

impl FakeTokenStore {
    fn new() -> Self {
        Self {
            state: Mutex::new(FakeTokenState::default()),
        }
    }

    pub(crate) fn created(&self) -> Vec<Token> {
        self.state.lock().unwrap().created.clone()
    }

    pub(crate) fn report_expired(&self, id: i64) {
        self.state.lock().unwrap().reported_expired.insert(id);
    }
}

#[async_trait]
impl TokenStore for FakeTokenStore {
    async fn create_token(
        &self,
        user_id: i64,
        hash: &str,
        expires_at: OffsetDateTime,
    ) -> std::result::Result<Token, BoxError> {
        let mut state = self.state.lock().unwrap();
        state.next_id += 1;
        let token = Token {
            id: state.next_id,
            user_id,
            hash: hash.into(),
            expires_at,
        };
        state.by_hash.insert(hash.into(), token.clone());
        state.created.push(token.clone());
        Ok(token)
    }

    async fn token_by_hash(&self, hash: &str) -> std::result::Result<TokenLookup, BoxError> {
        let state = self.state.lock().unwrap();
        let Some(token) = state.by_hash.get(hash).cloned() else {
            return Ok(TokenLookup::NotFound);
        };
        if state.reported_expired.contains(&token.id) {
            Ok(TokenLookup::Expired(token))
        } else {
            Ok(TokenLookup::Found(token))
        }
    }

    async fn delete_token(&self, id: i64) -> std::result::Result<(), BoxError> {
        let mut state = self.state.lock().unwrap();
        let hash = state
            .by_hash
            .iter()
            .find_map(|(hash, token)| (token.id == id).then(|| hash.clone()))
            .ok_or_else(|| Box::new(io::Error::other("no such token")) as BoxError)?;
        state.by_hash.remove(&hash);
        Ok(())
    }
}

pub(crate) fn new_test_service(
    clock: &FakeClock,
) -> (
    Service,
    Arc<FakeUserStore>,
    Arc<FakeAccessStore>,
    Arc<FakeTokenStore>,
) {
    let users = Arc::new(FakeUserStore::new());
    let access = Arc::new(FakeAccessStore::new());
    let tokens = Arc::new(FakeTokenStore::new());
    let service = Service::new(
        "bento.example.org",
        users.clone(),
        access.clone(),
        tokens.clone(),
    );
    let service_clock = clock.clone();
    let service = service.with_clock(move || service_clock.now());
    (service, users, access, tokens)
}

pub(crate) struct FakeExchanger {
    seen: Mutex<(String, String)>,
}

impl FakeExchanger {
    fn new() -> Self {
        Self {
            seen: Mutex::new((String::new(), String::new())),
        }
    }

    pub(crate) fn seen(&self) -> (String, String) {
        self.seen.lock().unwrap().clone()
    }
}

#[async_trait]
impl Exchanger for FakeExchanger {
    fn auth_code_url(&self, state: &str, nonce: &str) -> String {
        *self.seen.lock().unwrap() = (state.into(), nonce.into());
        format!("https://id.example.org/authorize?state={state}")
    }

    async fn exchange(&self, code: &str) -> std::result::Result<String, BoxError> {
        if code == "good-code" {
            Ok("raw-token".into())
        } else {
            Err(Box::new(io::Error::other("unknown code")))
        }
    }
}

pub(crate) struct FakeVerifier {
    claims: Mutex<HashMap<String, Claims>>,
}

impl FakeVerifier {
    fn new() -> Self {
        Self {
            claims: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn allow(&self, raw: &str, claims: Claims) {
        self.claims.lock().unwrap().insert(raw.into(), claims);
    }

    pub(crate) fn clear(&self) {
        self.claims.lock().unwrap().clear();
    }
}

#[async_trait]
impl Verifier for FakeVerifier {
    async fn verify(&self, raw_id_token: &str) -> std::result::Result<Claims, BoxError> {
        self.claims
            .lock()
            .unwrap()
            .get(raw_id_token)
            .cloned()
            .ok_or_else(|| Box::new(io::Error::other("bad token")) as BoxError)
    }
}

pub(crate) struct TestOidc {
    pub(crate) service: Service,
    pub(crate) exchanger: Arc<FakeExchanger>,
    pub(crate) verifier: Arc<FakeVerifier>,
    pub(crate) provisioner: Arc<FakeProvisioner>,
}

/// An OIDC service with signups closed: no provisioner is wired, so only
/// `subject-1` can log in.
pub(crate) fn new_oidc_service() -> TestOidc {
    build_oidc_service(false)
}

/// An OIDC service with signups open, which is the deployed default.
pub(crate) fn new_oidc_service_with_signups() -> TestOidc {
    build_oidc_service(true)
}

fn build_oidc_service(signups: bool) -> TestOidc {
    let clock = FakeClock::new(TEST_EPOCH);
    let (service, users, _, _) = new_test_service(&clock);
    users.insert("subject-1", 42, "shaun");
    let exchanger = Arc::new(FakeExchanger::new());
    let verifier = Arc::new(FakeVerifier::new());
    let provisioner = Arc::new(FakeProvisioner::new(users));
    let mut service = service.with_oidc(exchanger.clone(), verifier.clone());
    if signups {
        service = service.with_provisioner(provisioner.clone());
    }
    TestOidc {
        service,
        exchanger,
        verifier,
        provisioner,
    }
}

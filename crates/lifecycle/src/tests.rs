use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bento_cloudinit::Seed;
use bento_hypervisor::{
    AutostartClearer, Definer, DomainInfo, Error as HypervisorError, Fake, FakeDomain, Hypervisor,
    StopResult,
};
use bento_network::Plan;
use bento_types::{
    Capacity, Deployment, DesiredState, Image, ImageKind, ImageVersion, Instance, Placing, Slot,
    SlotState, State, User,
};
use tempfile::TempDir;
use time::macros::datetime;

use super::*;

fn boxed(message: &str) -> DynError {
    Box::new(io::Error::other(message.to_string()))
}

struct StoreData {
    /// The deployment slot division and who owns each slot
    /// (MULTI-NODE 7.1 and 7.2). The default matches a single-machine
    /// deployment: one `/24`-wide slot owned by host 1.
    runner_prefix: u8,
    slots: Vec<Slot>,
    /// How many machines could take an instance. One keeps the
    /// single-machine behaviour, where placement is not consulted.
    placeable_hosts: usize,
    /// What `choose_host` answers. `None` makes it refuse.
    chosen_host: Option<i64>,
    /// The ceiling of one machine, when a test sets one.
    capacities: HashMap<i64, Capacity>,
    instances: Vec<Instance>,
    users: HashMap<i64, User>,
    images: HashMap<String, Image>,
    versions: HashMap<String, ImageVersion>,
    active_hosts: Vec<String>,
    host_images: HashSet<(String, String, String)>,
    mutations: Vec<String>,
    released: Vec<String>,
    create_error: Option<String>,
    resize_error: Option<String>,
    rename_error: Option<String>,
    /// The capacity the manager handed to the last create or resize. The
    /// store is the layer that enforces it, so the manager's whole job
    /// here is to pass its configured value through unchanged.
    seen_capacity: Option<Capacity>,
}

impl Default for StoreData {
    fn default() -> Self {
        Self {
            runner_prefix: 24,
            placeable_hosts: 1,
            chosen_host: None,
            capacities: HashMap::new(),
            slots: vec![Slot {
                slot: 0,
                state: SlotState::Active,
                owner_host_id: 1,
                ownership_epoch: 0,
                source_host_id: None,
                destination_host_id: None,
                operation_id: None,
            }],
            instances: Vec::new(),
            users: HashMap::new(),
            images: HashMap::new(),
            versions: HashMap::new(),
            active_hosts: Vec::new(),
            host_images: HashSet::new(),
            mutations: Vec::new(),
            released: Vec::new(),
            create_error: None,
            resize_error: None,
            rename_error: None,
            seen_capacity: None,
        }
    }
}

#[derive(Default)]
struct TestStore(Mutex<StoreData>);

impl TestStore {
    fn data(&self) -> std::sync::MutexGuard<'_, StoreData> {
        self.0.lock().unwrap()
    }
    fn add_user(&self, id: i64, name: &str, subnet: &str) -> User {
        let user = User {
            id,
            name: name.into(),
            email: format!("{name}@example.test"),
            oidc_subject: None,
            subnet: subnet.into(),
            created_at: datetime!(2026-08-10 12:00 UTC),
        };
        self.data().users.insert(id, user.clone());
        user
    }
    fn add_image(&self, checksum: Option<&str>) {
        self.add_image_kind(checksum, ImageKind::Qcow2);
    }
    fn add_image_kind(&self, checksum: Option<&str>, kind: ImageKind) {
        let mut data = self.data();
        data.images.insert(
            "debian-13".into(),
            Image {
                name: "debian-13".into(),
                url: "https://example.test/debian".into(),
                kind,
                pinned_checksum: None,
                current_checksum: checksum.map(str::to_string),
            },
        );
        if let Some(checksum) = checksum {
            data.versions.insert(
                checksum.to_owned(),
                ImageVersion {
                    checksum: checksum.to_owned(),
                    image_name: "debian-13".into(),
                    path: format!("/images/{checksum}.qcow2"),
                    size: 1,
                    kind,
                    source_digest: None,
                    fetched_at: datetime!(2026-08-10 12:00 UTC),
                },
            );
        }
    }
    fn add_host(&self, name: &str) {
        self.data().active_hosts.push(name.into());
    }
    fn hold_image(&self, host: &str, image: &str, checksum: &str) {
        self.data()
            .host_images
            .insert((host.into(), image.into(), checksum.into()));
    }
}

#[async_trait]
impl Store for TestStore {
    async fn create_instance(
        &self,
        instance: Instance,
        _: Duration,
        capacity: Capacity,
    ) -> std::result::Result<(), DynError> {
        let mut data = self.data();
        data.seen_capacity = Some(capacity);
        data.mutations.push(format!("create {}", instance.name));
        if let Some(error) = &data.create_error {
            return Err(boxed(error));
        }
        data.instances.push(instance);
        Ok(())
    }
    async fn delete_instance(&self, uuid: &str) -> std::result::Result<Instance, DynError> {
        let mut data = self.data();
        data.mutations.push(format!("delete {uuid}"));
        let index = data
            .instances
            .iter()
            .position(|item| item.uuid == uuid)
            .ok_or_else(|| boxed("not found"))?;
        let instance = data.instances.remove(index);
        data.released.push(instance.name.clone());
        Ok(instance)
    }
    async fn instance(&self, uuid: &str) -> std::result::Result<Instance, DynError> {
        self.data()
            .instances
            .iter()
            .find(|item| item.uuid == uuid)
            .cloned()
            .ok_or_else(|| boxed("not found"))
    }
    async fn instances(&self) -> std::result::Result<Vec<Instance>, DynError> {
        Ok(self.data().instances.clone())
    }
    async fn instances_on_host(
        &self,
        host_id: i64,
    ) -> std::result::Result<Vec<Instance>, DynError> {
        Ok(self
            .data()
            .instances
            .iter()
            .filter(|item| item.host_id == host_id)
            .cloned()
            .collect())
    }
    async fn instances_to_restore(
        &self,
        host_id: i64,
    ) -> std::result::Result<Vec<Instance>, DynError> {
        Ok(self
            .data()
            .instances
            .iter()
            .filter(|item| {
                item.host_id == host_id
                    && item.desired_state == DesiredState::Running
                    && item.state == State::Stopped
            })
            .cloned()
            .collect())
    }
    async fn image(&self, name: &str) -> std::result::Result<Image, DynError> {
        self.data()
            .images
            .get(name)
            .cloned()
            .ok_or_else(|| boxed(&format!("no image {name}")))
    }
    async fn image_version(&self, checksum: &str) -> std::result::Result<ImageVersion, DynError> {
        self.data()
            .versions
            .get(checksum)
            .cloned()
            .ok_or_else(|| boxed(&format!("no image version {checksum}")))
    }
    async fn hosts_missing_image(
        &self,
        image_name: &str,
        checksum: &str,
    ) -> std::result::Result<Vec<String>, DynError> {
        let data = self.data();
        Ok(data
            .active_hosts
            .iter()
            .filter(|host| {
                !data.host_images.contains(&(
                    (*host).clone(),
                    image_name.to_owned(),
                    checksum.to_owned(),
                ))
            })
            .cloned()
            .collect())
    }
    async fn host_capacity(&self, host_id: i64) -> std::result::Result<Capacity, DynError> {
        // Every machine has the same generous ceiling unless a test says
        // otherwise, so only a test about capacity sees one.
        Ok(self
            .data()
            .capacities
            .get(&host_id)
            .copied()
            .unwrap_or(TEST_CAPACITY))
    }

    async fn choose_host(&self, _want: Placing) -> std::result::Result<i64, DynError> {
        let data = self.data();
        data.chosen_host
            .ok_or_else(|| boxed("no machine can take this instance"))
    }

    async fn placeable_hosts(&self) -> std::result::Result<usize, DynError> {
        Ok(self.data().placeable_hosts)
    }

    async fn deployment(&self) -> std::result::Result<Deployment, DynError> {
        Ok(Deployment {
            runner_prefix: self.data().runner_prefix,
        })
    }

    async fn slots(&self) -> std::result::Result<Vec<Slot>, DynError> {
        Ok(self.data().slots.clone())
    }

    async fn user_by_id(&self, id: i64) -> std::result::Result<User, DynError> {
        self.data()
            .users
            .get(&id)
            .cloned()
            .ok_or_else(|| boxed("no user"))
    }
    async fn rename_instance(
        &self,
        uuid: &str,
        name: &str,
        _: Duration,
    ) -> std::result::Result<(), DynError> {
        let mut data = self.data();
        data.mutations.push(format!("rename {uuid} {name}"));
        if let Some(error) = &data.rename_error {
            return Err(boxed(error));
        }
        let item = data
            .instances
            .iter_mut()
            .find(|item| item.uuid == uuid)
            .ok_or_else(|| boxed("not found"))?;
        let old = std::mem::replace(&mut item.name, name.into());
        data.released.push(old);
        Ok(())
    }
    async fn resize(
        &self,
        uuid: &str,
        vcpu: u32,
        memory: i64,
        disk: i64,
        nested: bool,
        capacity: Capacity,
    ) -> std::result::Result<(), DynError> {
        let mut data = self.data();
        data.seen_capacity = Some(capacity);
        data.mutations.push(format!("resize {uuid}"));
        if let Some(error) = &data.resize_error {
            return Err(boxed(error));
        }
        let item = data
            .instances
            .iter_mut()
            .find(|item| item.uuid == uuid)
            .ok_or_else(|| boxed("not found"))?;
        item.vcpu = vcpu;
        item.memory_mib = memory;
        item.disk_gib = disk;
        item.nested = nested;
        Ok(())
    }
    async fn set_desired_state(
        &self,
        uuid: &str,
        state: DesiredState,
    ) -> std::result::Result<(), DynError> {
        let mut data = self.data();
        data.mutations.push(format!("desired {uuid} {state}"));
        data.instances
            .iter_mut()
            .find(|item| item.uuid == uuid)
            .ok_or_else(|| boxed("not found"))?
            .desired_state = state;
        Ok(())
    }
    async fn set_observed_state(
        &self,
        uuid: &str,
        state: State,
    ) -> std::result::Result<(), DynError> {
        let mut data = self.data();
        data.mutations.push(format!("observed {uuid} {state}"));
        data.instances
            .iter_mut()
            .find(|item| item.uuid == uuid)
            .ok_or_else(|| boxed("not found"))?
            .state = state;
        Ok(())
    }
    async fn update_observed_states(
        &self,
        states: HashMap<String, State>,
    ) -> std::result::Result<(), DynError> {
        let mut data = self.data();
        data.mutations
            .push(format!("observed-batch {}", states.len()));
        for item in &mut data.instances {
            if let Some(state) = states.get(&item.uuid) {
                item.state = *state;
            }
        }
        Ok(())
    }
}

/// Stands in for the machine that builds an instance.
///
/// It defines the domain in the same fake hypervisor the manager reads,
/// which is what a real machine does, and it records which machine was
/// asked so a placement test can assert where the work went.
struct TestImages {
    calls: Mutex<Vec<String>>,
    error: Mutex<Option<String>>,
    /// The `host_id` of every provision request, in order.
    placed_on: Mutex<Vec<i64>>,
    /// The `host_id` of every hypervisor lookup, in order. An action on
    /// an instance asks for the machine that runs it.
    acted_on: Mutex<Vec<i64>>,
    /// Machines that cannot be reached, by id.
    unreachable: Mutex<Vec<i64>>,
    /// The machine this fake stands for. Only work aimed here reaches
    /// the hypervisor, as with the real one.
    fake: Arc<Fake>,
    /// The seed image is written by the machine that builds the
    /// instance, so this fake writes it (SPEC 5.2).
    iso: Arc<TestIso>,
    storage_dir: PathBuf,
}

impl TestImages {
    fn new(fake: Arc<Fake>, iso: Arc<TestIso>, storage_dir: PathBuf) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            error: Mutex::new(None),
            placed_on: Mutex::new(Vec::new()),
            acted_on: Mutex::new(Vec::new()),
            unreachable: Mutex::new(Vec::new()),
            fake,
            iso,
            storage_dir,
        }
    }
}

#[async_trait]
impl Fleet for TestImages {
    async fn provision(&self, spec: &ProvisionSpec) -> std::result::Result<State, DynError> {
        self.placed_on.lock().unwrap().push(spec.host_id);
        self.calls.lock().unwrap().push(format!(
            "{} {} {}",
            spec.instance.base_checksum, spec.instance.uuid, spec.instance.disk_gib
        ));
        let overlay = self
            .storage_dir
            .join(format!("{}.qcow2", spec.instance.uuid));
        let seed_iso = self
            .storage_dir
            .join(format!("{}-seed.iso", spec.instance.uuid));
        if let Some(error) = self.error.lock().unwrap().clone() {
            return Err(boxed(&error));
        }
        tokio::fs::write(&overlay, b"overlay").await?;
        if spec.with_seed_iso
            && let Err(error) = self.iso.build(&spec.seed, &seed_iso).await
        {
            self.unwind(&spec.instance.uuid).await;
            return Err(error);
        }
        let xml = bento_hypervisor::domain_xml(&bento_hypervisor::DomainSpec {
            name: spec.instance.name.clone(),
            uuid: spec.instance.uuid.clone(),
            vcpu: spec.instance.vcpu,
            memory_mib: spec.instance.memory_mib,
            disk_path: self
                .storage_dir
                .join(format!("{}.qcow2", spec.instance.uuid))
                .display()
                .to_string(),
            iso_path: if spec.with_seed_iso {
                self.storage_dir
                    .join(format!("{}-seed.iso", spec.instance.uuid))
                    .display()
                    .to_string()
            } else {
                String::new()
            },
            network: spec.network.clone(),
            mac: spec.instance.mac.clone(),
            nested: spec.instance.nested,
            ksm: spec.instance.ksm,
            arch: String::new(),
        })
        .map_err(|error| boxed(&error.to_string()))?;
        if let Err(error) = bento_hypervisor::Hypervisor::create(self.fake.as_ref(), &xml).await {
            self.unwind(&spec.instance.uuid).await;
            return Err(boxed(&error.to_string()));
        }
        Ok(if spec.start {
            State::Running
        } else {
            State::Stopped
        })
    }

    async fn deprovision(
        &self,
        _host_id: i64,
        instance: &Instance,
    ) -> std::result::Result<(), DynError> {
        self.unwind(&instance.uuid).await;
        Ok(())
    }

    async fn hypervisor(&self, host_id: i64) -> Option<Arc<dyn bento_hypervisor::Hypervisor>> {
        self.acted_on.lock().unwrap().push(host_id);
        if self.unreachable.lock().unwrap().contains(&host_id) {
            return None;
        }
        // One fake machine stands for the whole fleet here, so an action
        // aimed anywhere reaches the same domains the manager reads.
        Some(self.fake.clone())
    }
}

impl TestImages {
    /// A real machine removes its own partial work before it reports a
    /// failure, so this one does too.
    async fn unwind(&self, uuid: &str) {
        let seed_iso = self.storage_dir.join(format!("{uuid}-seed.iso"));
        self.iso.seeds.lock().unwrap().remove(&seed_iso);
        let _ = tokio::fs::remove_file(&seed_iso).await;
        let _ = tokio::fs::remove_file(self.storage_dir.join(format!("{uuid}.qcow2"))).await;
    }
}

#[derive(Default)]
struct TestIso {
    seeds: Mutex<HashMap<PathBuf, Seed>>,
    error: Mutex<Option<String>>,
}
#[async_trait]
impl ISOBuilder for TestIso {
    async fn build(&self, seed: &Seed, path: &Path) -> std::result::Result<(), DynError> {
        if let Some(error) = self.error.lock().unwrap().clone() {
            return Err(boxed(&error));
        }
        self.seeds
            .lock()
            .unwrap()
            .insert(path.to_path_buf(), seed.clone());
        Ok(())
    }
}

#[derive(Default)]
struct TestResizer {
    calls: Mutex<Vec<(PathBuf, i64)>>,
    error: Mutex<Option<String>>,
}
#[async_trait]
impl OverlayResizer for TestResizer {
    async fn resize_overlay(&self, path: &Path, disk: i64) -> std::result::Result<(), DynError> {
        self.calls.lock().unwrap().push((path.to_path_buf(), disk));
        if let Some(error) = self.error.lock().unwrap().clone() {
            Err(boxed(&error))
        } else {
            Ok(())
        }
    }
}

#[derive(Default)]
struct TestDefiner {
    xml: Mutex<Vec<String>>,
    fail: Mutex<bool>,
}
#[async_trait]
impl Definer for TestDefiner {
    async fn define(&self, xml: &str) -> std::result::Result<(), HypervisorError> {
        self.xml.lock().unwrap().push(xml.into());
        if *self.fail.lock().unwrap() {
            Err(HypervisorError::Operation("define exploded".into()))
        } else {
            Ok(())
        }
    }
}

#[derive(Default)]
struct TestClearer(Mutex<Vec<String>>);
#[async_trait]
impl AutostartClearer for TestClearer {
    async fn clear_autostart(&self, name: &str) -> std::result::Result<(), HypervisorError> {
        self.0.lock().unwrap().push(name.into());
        Ok(())
    }
}

#[derive(Default)]
struct TestLog(Mutex<Vec<String>>);
impl LifecycleLogger for TestLog {
    fn info(&self, message: &str) {
        self.0.lock().unwrap().push(format!("INFO {message}"));
    }
    fn warn(&self, message: &str) {
        self.0.lock().unwrap().push(format!("WARN {message}"));
    }
    fn error(&self, message: &str) {
        self.0.lock().unwrap().push(format!("ERROR {message}"));
    }
}

#[derive(Default)]
struct TestSleep(Mutex<Vec<Duration>>);
#[async_trait]
impl Sleep for TestSleep {
    async fn sleep(&self, duration: Duration) {
        self.0.lock().unwrap().push(duration);
    }
}

/// A capacity no test instance comes near, so that only a test that
/// looks for it notices it.
const TEST_CAPACITY: Capacity = Capacity {
    memory_mib: 262_144,
    disk_gib: 4096,
};

struct Fixture {
    manager: Manager,
    fake: Arc<Fake>,
    store: Arc<TestStore>,
    images: Arc<TestImages>,
    iso: Arc<TestIso>,
    resizer: Arc<TestResizer>,
    definer: Arc<TestDefiner>,
    clearer: Arc<TestClearer>,
    log: Arc<TestLog>,
    sleep: Arc<TestSleep>,
    _temp: TempDir,
}

fn fixture(with_definer: bool, with_clearer: bool) -> Fixture {
    let fake = Arc::new(Fake::default());
    let store = Arc::new(TestStore::default());
    let iso = Arc::new(TestIso::default());
    let resizer = Arc::new(TestResizer::default());
    let definer = Arc::new(TestDefiner::default());
    let clearer = Arc::new(TestClearer::default());
    let log = Arc::new(TestLog::default());
    let sleep = Arc::new(TestSleep::default());
    let temp = tempfile::tempdir().unwrap();
    let images = Arc::new(TestImages::new(
        fake.clone(),
        iso.clone(),
        temp.path().to_path_buf(),
    ));
    let iso_for_exists = iso.clone();
    let iso_for_delete = iso.clone();
    let manager = Manager::new(Config {
        hypervisor: Some(fake.clone()),
        definer: with_definer.then(|| definer.clone() as Arc<dyn Definer>),
        autostart_clearer: with_clearer.then(|| clearer.clone() as Arc<dyn AutostartClearer>),
        store: Some(store.clone()),
        host_id: 1,
        fleet: Some(images.clone()),
        iso: Some(iso.clone()),
        resizer: Some(resizer.clone()),
        plan: Some(Plan::new("10.77.0.0/16").unwrap()),
        storage_dir: temp.path().to_path_buf(),
        logger: Some(log.clone()),
        nested_enabled: Some(Arc::new(|| (false, "kvm_intel nested is N".into()))),
        sleep: Some(sleep.clone()),
        new_uuid: Some(Arc::new({
            let next = Mutex::new(0_u32);
            move || {
                let mut next = next.lock().unwrap();
                *next += 1;
                format!("{next:08}-1111-4111-8111-111111111111")
            }
        })),
        now: Some(Arc::new(|| datetime!(2026-08-10 12:00 UTC))),
        iso_exists: Some(Arc::new(move |path| {
            iso_for_exists.seeds.lock().unwrap().contains_key(path)
        })),
        delete_iso: Some(Arc::new(move |path| {
            let iso = iso_for_delete.clone();
            Box::pin(async move {
                iso.seeds.lock().unwrap().remove(&path);
                Ok(())
            })
        })),
        ..Config::default()
    })
    .unwrap();
    Fixture {
        manager,
        fake,
        store,
        images,
        iso,
        resizer,
        definer,
        clearer,
        log,
        sleep,
        _temp: temp,
    }
}

fn request(owner: User, name: &str) -> NewRequest {
    NewRequest {
        name: name.into(),
        owner,
        host_id: 1,
        ssh_keys: vec!["ssh-ed25519 AAAA test@key".into()],
        image_name: "debian-13".into(),
        vcpu: 2,
        memory_mib: 2048,
        disk_gib: 20,
        nested: false,
        disable_ksm: false,
        http_port: 0,
    }
}

async fn setup(fixture: &Fixture) -> Instance {
    let owner = fixture.store.add_user(1, "amber", "10.77.0.0/24");
    fixture.store.add_image(Some("aa11"));
    fixture.manager.create(request(owner, "web")).await.unwrap()
}

#[tokio::test]
async fn new_instance() {
    let f = fixture(false, false);
    let instance = setup(&f).await;
    assert_eq!(
        (instance.state, instance.desired_state),
        (State::Running, DesiredState::Running)
    );
    assert_eq!(instance.address, "10.77.0.2");
    assert_eq!(instance.base_checksum, "aa11");
    assert!(instance.ksm);
    let seed = f.iso.seeds.lock().unwrap()[&f.manager.seed_iso_path(&instance.uuid)].clone();
    assert_eq!(
        (seed.hostname.as_str(), seed.user_name.as_str()),
        ("web", GUEST_USER)
    );
    assert_eq!(
        (seed.address_cidr.as_str(), seed.gateway.as_str()),
        ("10.77.0.2/24", "10.77.0.1")
    );
    let domain = f.fake.domain("web").unwrap();
    assert_eq!(domain.state, State::Running);
    assert!(domain.xml.contains("bento-user-0"));
    assert!(domain.xml.contains(&instance.mac));
}

#[tokio::test]
async fn a_new_instance_records_the_slot_its_address_came_from() {
    // Slot ownership is deployment-wide, so the address alone says which
    // machine runs the instance (MULTI-NODE 7.2). At /24 there is one
    // slot and the guest still gets a /24 address.
    let f = fixture(false, false);
    let instance = setup(&f).await;
    assert_eq!(instance.address, "10.77.0.2");
    assert_eq!(instance.slot, Some(0));
}

#[tokio::test]
async fn a_new_instance_allocates_inside_its_machines_slot() {
    // At /25 the machine owning slot 1 hands out .129 upwards, never an
    // address from slot 0, and never the .128 subprefix boundary
    // (MULTI-NODE 7.1).
    let f = fixture(false, false);
    {
        let mut data = f.store.data();
        data.runner_prefix = 25;
        data.slots[0].owner_host_id = 2;
        data.slots.push(Slot {
            slot: 1,
            state: SlotState::Active,
            owner_host_id: 1,
            ownership_epoch: 0,
            source_host_id: None,
            destination_host_id: None,
            operation_id: None,
        });
    }
    let owner = f.store.add_user(1, "amber", "10.77.0.0/24");
    f.store.add_image(Some("aa11"));

    let instance = f.manager.create(request(owner, "web")).await.unwrap();
    assert_eq!(instance.address, "10.77.0.129");
    assert_eq!(instance.slot, Some(1));

    // The guest is still configured /24 and keeps the .1 gateway, so it
    // treats the whole user network as on-link (MULTI-NODE 8.1).
    let seed = f.iso.seeds.lock().unwrap()[&f.manager.seed_iso_path(&instance.uuid)].clone();
    assert_eq!(
        (seed.address_cidr.as_str(), seed.gateway.as_str()),
        ("10.77.0.129/24", "10.77.0.1")
    );
}

#[tokio::test]
async fn one_machine_keeps_using_itself_without_asking_placement() {
    // A single-machine deployment need not run a runner service, so it
    // has no health observation to place against. Asking a placement
    // question with one possible answer would make every create depend on
    // a poll that deployment does not run (MULTI-NODE 12).
    let f = fixture(false, false);
    f.store.data().placeable_hosts = 1;
    f.store.data().chosen_host = None; // placement would refuse if asked
    let instance = setup(&f).await;

    assert_eq!(instance.host_id, 1);
    assert_eq!(f.images.placed_on.lock().unwrap().as_slice(), &[1]);
}

#[tokio::test]
async fn a_second_machine_receives_the_instance_placement_chose() {
    // The whole point of a fleet: the create names no machine, and the
    // instance is built on the one placement picked (MULTI-NODE 12).
    let f = fixture(false, false);
    {
        let mut data = f.store.data();
        data.placeable_hosts = 2;
        data.chosen_host = Some(2);
        data.runner_prefix = 25;
        data.slots.push(Slot {
            slot: 1,
            state: SlotState::Active,
            owner_host_id: 2,
            ownership_epoch: 0,
            source_host_id: None,
            destination_host_id: None,
            operation_id: None,
        });
    }
    let owner = f.store.add_user(1, "amber", "10.77.0.0/24");
    f.store.add_image(Some("aa11"));

    let instance = f.manager.create(request(owner, "web")).await.unwrap();

    assert_eq!(
        instance.host_id, 2,
        "the instance went to the wrong machine"
    );
    assert_eq!(
        f.images.placed_on.lock().unwrap().as_slice(),
        &[2],
        "the work went to the wrong machine"
    );
    // The address came from the slot that machine owns, so the address
    // alone says where the instance runs (MULTI-NODE 7.2).
    assert_eq!(instance.address, "10.77.0.129");
    assert_eq!(instance.slot, Some(1));
}

#[tokio::test]
async fn a_full_machine_does_not_refuse_a_create_bound_for_an_empty_one() {
    // Capacity belongs to a host (MULTI-NODE 12). Weighing a create
    // against the controller's ceiling, or against the whole
    // deployment's usage, refuses work that the machine it is bound for
    // has ample room for. That reads to a user as "the host has no room"
    // while a machine sits empty.
    let f = fixture(false, false);
    {
        let mut data = f.store.data();
        data.placeable_hosts = 2;
        data.chosen_host = Some(2);
        data.slots[0].owner_host_id = 2;
        // Host 1 is nearly full; host 2 is empty and large.
        data.capacities.insert(
            1,
            Capacity {
                memory_mib: 1024,
                disk_gib: 4,
            },
        );
        data.capacities.insert(
            2,
            Capacity {
                memory_mib: 65_536,
                disk_gib: 512,
            },
        );
    }
    let owner = f.store.add_user(1, "amber", "10.77.0.0/24");
    f.store.add_image(Some("aa11"));

    let mut request = request(owner, "web");
    request.memory_mib = 8192;
    request.disk_gib = 40;
    let instance = f.manager.create(request).await.unwrap();

    assert_eq!(instance.host_id, 2);
    // The ceiling that travelled to the store is the one of the machine
    // that will run it.
    assert_eq!(
        f.store.data().seen_capacity,
        Some(Capacity {
            memory_mib: 65_536,
            disk_gib: 512,
        }),
        "the create was weighed against the wrong machine"
    );
}

#[tokio::test]
async fn a_create_is_still_refused_when_its_own_machine_is_full() {
    // The check is not weakened, only moved to the right machine.
    let f = fixture(false, false);
    f.store.data().capacities.insert(
        1,
        Capacity {
            memory_mib: 1024,
            disk_gib: 4,
        },
    );
    f.store.data().create_error = Some("the host has no room".into());
    let owner = f.store.add_user(1, "amber", "10.77.0.0/24");
    f.store.add_image(Some("aa11"));

    let mut request = request(owner, "web");
    request.disk_gib = 40;
    let error = f.manager.create(request).await.unwrap_err();

    assert!(error.to_string().contains("no room"), "{error}");
    assert_eq!(
        f.store.data().seen_capacity,
        Some(Capacity {
            memory_mib: 1024,
            disk_gib: 4,
        })
    );
}

#[tokio::test]
async fn a_create_that_no_machine_can_take_is_refused() {
    // Placement refuses and names why. Nothing is written: no row, no
    // address, and no work sent anywhere.
    let f = fixture(false, false);
    {
        let mut data = f.store.data();
        data.placeable_hosts = 2;
        data.chosen_host = None;
    }
    let owner = f.store.add_user(1, "amber", "10.77.0.0/24");
    f.store.add_image(Some("aa11"));

    let error = f.manager.create(request(owner, "web")).await.unwrap_err();

    assert!(error.to_string().contains("no machine"), "{error}");
    assert!(f.store.data().instances.is_empty());
    assert!(f.images.placed_on.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_failure_on_the_far_machine_leaves_no_row_behind() {
    // The machine cleans up its own files and reports why. This side
    // removes the row, which is the only thing it can reach
    // (MULTI-NODE 13.1).
    let f = fixture(false, false);
    {
        let mut data = f.store.data();
        data.placeable_hosts = 2;
        data.chosen_host = Some(2);
        data.slots[0].owner_host_id = 2;
    }
    *f.images.error.lock().unwrap() = Some("no room on that machine".into());
    let owner = f.store.add_user(1, "amber", "10.77.0.0/24");
    f.store.add_image(Some("aa11"));

    let error = f.manager.create(request(owner, "web")).await.unwrap_err();

    assert!(
        error.to_string().contains("no room on that machine"),
        "{error}"
    );
    assert!(f.store.data().instances.is_empty(), "the row survived");
    assert!(f.fake.domain("web").is_none());
}

#[tokio::test]
async fn an_action_goes_to_the_machine_that_runs_the_instance() {
    // Starting, stopping, rebooting, and removing all act on the domain,
    // and the domain is on one machine (MULTI-NODE 13.1). The local
    // connection is used only for an instance that is local.
    let f = fixture(false, false);
    let instance = setup(&f).await;
    // The manager runs on host 1; move the instance to host 2.
    f.store
        .data()
        .instances
        .iter_mut()
        .for_each(|row| row.host_id = 2);

    f.manager.stop(&instance.uuid).await.unwrap();
    f.manager.start(&instance.uuid).await.unwrap();
    f.manager.restart(&instance.uuid).await.unwrap();

    assert_eq!(
        f.images.acted_on.lock().unwrap().as_slice(),
        &[2, 2, 2],
        "an action reached the wrong machine"
    );
}

#[tokio::test]
async fn a_local_instance_never_leaves_this_machine() {
    // The fleet is not asked at all for an instance that runs here, so a
    // single-machine deployment needs no way to reach another machine.
    let f = fixture(false, false);
    let instance = setup(&f).await;

    f.manager.stop(&instance.uuid).await.unwrap();
    f.manager.start(&instance.uuid).await.unwrap();

    assert!(
        f.images.acted_on.lock().unwrap().is_empty(),
        "a local action asked the fleet for a machine"
    );
}

#[tokio::test]
async fn an_action_on_an_unreachable_machine_says_so() {
    // A deployment with no runner endpoints cannot act on another
    // machine. The refusal names the instance rather than reporting a
    // missing domain, which would read as an instance that is gone.
    let f = fixture(false, false);
    let instance = setup(&f).await;
    f.store
        .data()
        .instances
        .iter_mut()
        .for_each(|row| row.host_id = 2);
    f.images.unreachable.lock().unwrap().push(2);

    let error = f.manager.stop(&instance.uuid).await.unwrap_err();

    assert!(
        error
            .to_string()
            .contains("another machine that cannot be reached"),
        "{error}"
    );
}

#[tokio::test]
async fn a_copy_of_an_instance_on_another_machine_is_refused() {
    // A copy is a local file copy of the source's overlay
    // (MULTI-NODE 13.3). Copying between machines is a transfer with its
    // own verification, which is the slot-move workflow of section 17.
    let f = fixture(false, false);
    let source = setup(&f).await;
    f.manager.stop(&source.uuid).await.unwrap();
    f.store
        .data()
        .instances
        .iter_mut()
        .for_each(|instance| instance.host_id = 2);
    let owner = f.store.user_by_id(1).await.unwrap();

    let error = f
        .manager
        .copy(&source.uuid, copy_request(owner, "clone"))
        .await
        .unwrap_err();

    assert!(error.to_string().contains("another machine"), "{error}");
}

#[tokio::test]
async fn a_machine_with_no_slot_cannot_take_an_instance() {
    // A machine that has joined the fleet but has not been given a slot
    // has no address range, so placement onto it is refused rather than
    // silently borrowing another machine's addresses (MULTI-NODE 7.2).
    let f = fixture(false, false);
    f.store.data().slots[0].owner_host_id = 2;
    let owner = f.store.add_user(1, "amber", "10.77.0.0/24");
    f.store.add_image(Some("aa11"));

    let error = f.manager.create(request(owner, "web")).await.unwrap_err();
    assert!(
        matches!(error, Error::NoSlotForHost { host_id: 1 }),
        "{error:?}"
    );
    assert!(f.store.data().mutations.is_empty());
}

#[tokio::test]
async fn a_draining_slot_takes_no_new_address() {
    // Draining keeps the owner authoritative but permits no new
    // allocation (MULTI-NODE 17).
    let f = fixture(false, false);
    f.store.data().slots[0].state = SlotState::Draining;
    let owner = f.store.add_user(1, "amber", "10.77.0.0/24");
    f.store.add_image(Some("aa11"));

    let error = f.manager.create(request(owner, "web")).await.unwrap_err();
    assert!(
        matches!(error, Error::NoSlotForHost { host_id: 1 }),
        "{error:?}"
    );
}

#[tokio::test]
async fn new_uses_immutable_version_kind_not_mutable_allowlist_kind() {
    let f = fixture(false, false);
    let owner = f.store.add_user(1, "amber", "10.77.0.0/24");
    f.store.add_image_kind(Some("bootc-disk"), ImageKind::Oci);
    f.store.data().images.get_mut("debian-13").unwrap().kind = ImageKind::Qcow2;

    let instance = f.manager.create(request(owner, "bootc")).await.unwrap();
    let seed = f.iso.seeds.lock().unwrap()[&f.manager.seed_iso_path(&instance.uuid)].clone();
    assert!(
        !seed.install_guest_agent,
        "bootc disks must not mutate /usr"
    );
}

#[tokio::test]
async fn new_addresses_advance() {
    let f = fixture(false, false);
    let owner = f.store.add_user(1, "amber", "10.77.0.0/24");
    f.store.add_image(Some("aa"));
    let first = f
        .manager
        .create(request(owner.clone(), "one"))
        .await
        .unwrap();
    let second = f.manager.create(request(owner, "two")).await.unwrap();
    assert_eq!(
        (first.address.as_str(), second.address.as_str()),
        ("10.77.0.2", "10.77.0.3")
    );
    assert_ne!(first.mac, second.mac);
}

#[tokio::test]
async fn new_validation() {
    let f = fixture(false, false);
    let owner = f.store.add_user(1, "amber", "10.77.0.0/24");
    f.store.add_image(Some("aa"));
    let mut cases = Vec::new();
    let mut r = request(owner.clone(), "");
    cases.push(r);
    r = request(owner.clone(), "x");
    r.image_name.clear();
    cases.push(r);
    r = request(owner.clone(), "x");
    r.vcpu = 0;
    cases.push(r);
    r = request(owner, "x");
    r.disk_gib = 0;
    cases.push(r);
    for case in cases {
        assert!(f.manager.create(case).await.is_err());
    }
    assert!(f.store.data().instances.is_empty());
}

#[tokio::test]
async fn new_nested_rejected_when_host_off() {
    let f = fixture(false, false);
    let owner = f.store.add_user(1, "a", "10.77.0.0/24");
    f.store.add_image(Some("aa"));
    let mut r = request(owner, "lab");
    r.nested = true;
    let error = f.manager.create(r).await.unwrap_err();
    assert!(matches!(error, Error::NestedUnavailable(_)));
    assert!(error.to_string().contains("kvm_intel.nested=1"));
}

#[tokio::test]
async fn new_nested_allowed_when_host_on() {
    let mut f = fixture(false, false);
    f.manager.nested = Arc::new(|| (true, String::new()));
    let owner = f.store.add_user(1, "a", "10.77.0.0/24");
    f.store.add_image(Some("aa"));
    let mut r = request(owner, "lab");
    r.nested = true;
    f.manager.create(r).await.unwrap();
    assert!(
        f.fake
            .domain("lab")
            .unwrap()
            .xml
            .contains("host-passthrough")
    );
}

#[tokio::test]
async fn new_no_image_version() {
    let f = fixture(false, false);
    let owner = f.store.add_user(1, "a", "10.77.0.0/24");
    f.store.add_image(None);
    assert!(matches!(
        f.manager.create(request(owner, "web")).await,
        Err(Error::NoImageVersion(_))
    ));
}

#[tokio::test]
async fn new_waits_for_every_active_host_to_hold_the_current_image() {
    let f = fixture(false, false);
    let owner = f.store.add_user(1, "a", "10.77.0.0/24");
    f.store.add_image(Some("aa"));
    f.store.add_host("runner-a.example.org");
    f.store.add_host("runner-b.example.org");
    f.store
        .hold_image("runner-a.example.org", "debian-13", "aa");

    let error = f
        .manager
        .create(request(owner.clone(), "web"))
        .await
        .unwrap_err();

    assert!(matches!(error, Error::FleetImageNotReady { .. }));
    assert_eq!(
        error.to_string(),
        "the fleet is still fetching debian-13: runner-b.example.org is not ready"
    );
    assert!(f.store.data().instances.is_empty());
    assert!(f.images.calls.lock().unwrap().is_empty());

    f.store
        .hold_image("runner-b.example.org", "debian-13", "aa");
    f.manager.create(request(owner, "web")).await.unwrap();
}

#[tokio::test]
async fn new_passes_the_image_gate_on_one_ready_host() {
    let f = fixture(false, false);
    let owner = f.store.add_user(1, "a", "10.77.0.0/24");
    f.store.add_image(Some("aa"));
    f.store.add_host("runner-a.example.org");
    f.store
        .hold_image("runner-a.example.org", "debian-13", "aa");

    let instance = f.manager.create(request(owner, "web")).await.unwrap();

    assert_eq!(instance.base_checksum, "aa");
}

#[tokio::test]
async fn new_capacity_error_stops_everything() {
    let f = fixture(false, false);
    let owner = f.store.add_user(1, "a", "10.77.0.0/24");
    f.store.add_image(Some("aa"));
    f.store.data().create_error = Some("the host has no room".into());
    assert!(f.manager.create(request(owner, "web")).await.is_err());
    assert!(f.images.calls.lock().unwrap().is_empty());
    assert!(f.fake.calls().is_empty());
}

#[tokio::test]
async fn create_and_resize_hand_the_store_the_configured_capacity() {
    // The store runs the check inside its own transaction, so the only
    // thing the manager can get wrong is the number it passes (SPEC 6.1).
    let f = fixture(false, false);
    let owner = f.store.add_user(1, "a", "10.77.0.0/24");
    f.store.add_image(Some("aa"));
    f.manager.create(request(owner, "web")).await.unwrap();
    assert_eq!(f.store.data().seen_capacity, Some(TEST_CAPACITY));

    f.store.data().seen_capacity = None;
    let uuid = f.store.data().instances[0].uuid.clone();
    f.manager
        .resize(ResizeRequest {
            uuid,
            vcpu: 2,
            memory_mib: 2048,
            disk_gib: 20,
            nested: false,
        })
        .await
        .unwrap();
    assert_eq!(f.store.data().seen_capacity, Some(TEST_CAPACITY));
}

#[tokio::test]
async fn new_unwind() {
    for point in ["overlay", "iso", "domain"] {
        let f = fixture(false, false);
        let owner = f.store.add_user(1, "a", "10.77.0.0/24");
        f.store.add_image(Some("aa"));
        match point {
            "overlay" => *f.images.error.lock().unwrap() = Some("exploded".into()),
            "iso" => *f.iso.error.lock().unwrap() = Some("exploded".into()),
            _ => f.fake.set_hook(|operation, _| {
                if operation == "create" {
                    Err(HypervisorError::Operation("exploded".into()))
                } else {
                    Ok(())
                }
            }),
        }
        assert!(f.manager.create(request(owner, "web")).await.is_err());
        assert!(f.store.data().instances.is_empty());
        assert!(f.iso.seeds.lock().unwrap().is_empty());
        assert!(f.fake.domain("web").is_none());
    }
}

#[tokio::test]
async fn stop_paths() {
    for forced in [false, true] {
        let f = fixture(false, false);
        let instance = setup(&f).await;
        f.fake.set_force_stop(forced);
        let got = f.manager.stop(&instance.uuid).await.unwrap();
        assert_eq!(
            got,
            if forced {
                StopResult::Forced
            } else {
                StopResult::Graceful
            }
        );
    }
    let f = fixture(false, false);
    let instance = setup(&f).await;
    f.manager.stop(&instance.uuid).await.unwrap();
    assert_eq!(
        f.manager.stop(&instance.uuid).await.unwrap(),
        StopResult::AlreadyStopped
    );
}

#[tokio::test]
async fn stop_records_desired_before_wait() {
    let f = fixture(false, false);
    let instance = setup(&f).await;
    f.fake.set_hook(|operation, _| {
        if operation == "stop" {
            Err(HypervisorError::Operation("lost".into()))
        } else {
            Ok(())
        }
    });
    assert!(f.manager.stop(&instance.uuid).await.is_err());
    let stored = f.store.instance(&instance.uuid).await.unwrap();
    assert_eq!(
        (stored.desired_state, stored.state),
        (DesiredState::Stopped, State::Running)
    );
}

#[tokio::test]
async fn start_after_stop() {
    let f = fixture(false, false);
    let instance = setup(&f).await;
    f.manager.stop(&instance.uuid).await.unwrap();
    f.manager.start(&instance.uuid).await.unwrap();
    let stored = f.store.instance(&instance.uuid).await.unwrap();
    assert_eq!(
        (stored.state, stored.desired_state),
        (State::Running, DesiredState::Running)
    );
}

#[tokio::test]
async fn restart() {
    let f = fixture(false, false);
    let instance = setup(&f).await;
    f.manager.restart(&instance.uuid).await.unwrap();
    assert!(f.fake.calls().contains(&"reboot web".into()));
}

#[tokio::test]
async fn remove() {
    let f = fixture(false, false);
    let instance = setup(&f).await;
    let overlay = f.manager.overlay_path(&instance.uuid);
    f.manager.remove(&instance.uuid).await.unwrap();
    assert!(f.fake.domain("web").is_none());
    assert!(!overlay.exists());
    let data = f.store.data();
    assert!(data.instances.is_empty());
    assert_eq!(data.released, ["web"]);
}

#[tokio::test]
async fn remove_tolerates_missing_domain() {
    let f = fixture(false, false);
    let instance = setup(&f).await;
    f.fake.remove("web").await.unwrap();
    f.manager.remove(&instance.uuid).await.unwrap();
    assert!(f.store.data().instances.is_empty());
}

#[tokio::test]
async fn remove_is_only_user_driven() {
    let f = fixture(false, false);
    let instance = setup(&f).await;
    f.manager.poll_once().await.unwrap();
    f.manager
        .handle_event(&instance.uuid, State::Stopped)
        .await
        .unwrap();
    f.manager.restore().await.unwrap();
    f.manager.reconcile().await.unwrap();
    assert_eq!(f.store.data().instances.len(), 1);
    assert!(
        !f.fake
            .calls()
            .iter()
            .any(|call| call.starts_with("remove "))
    );
}

#[tokio::test]
async fn resize_disk_growth() {
    let f = fixture(false, false);
    let instance = setup(&f).await;
    let result = f
        .manager
        .resize(ResizeRequest {
            uuid: instance.uuid.clone(),
            vcpu: 2,
            memory_mib: 2048,
            disk_gib: 30,
            nested: false,
        })
        .await
        .unwrap();
    assert_eq!(
        result,
        ResizeResult {
            restart_required: false,
            disk_grown: true
        }
    );
    assert_eq!(f.resizer.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn resize_shrink_rejected() {
    let f = fixture(false, false);
    let instance = setup(&f).await;
    let error = f
        .manager
        .resize(ResizeRequest {
            uuid: instance.uuid,
            vcpu: 2,
            memory_mib: 2048,
            disk_gib: 10,
            nested: false,
        })
        .await
        .unwrap_err();
    assert!(matches!(error, Error::DiskShrink(_)));
    assert!(f.resizer.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn resize_memory_edits_xml_and_requires_restart() {
    let f = fixture(true, false);
    let instance = setup(&f).await;
    let result = f
        .manager
        .resize(ResizeRequest {
            uuid: instance.uuid.clone(),
            vcpu: 4,
            memory_mib: 4096,
            disk_gib: 20,
            nested: false,
        })
        .await
        .unwrap();
    assert!(result.restart_required);
    let xml = f.definer.xml.lock().unwrap()[0].clone();
    assert!(xml.contains(">4096<"));
    assert!(xml.contains(">4<"));
}

#[tokio::test]
async fn resize_nested_rejected_when_host_off() {
    let f = fixture(false, false);
    let instance = setup(&f).await;
    assert!(matches!(
        f.manager
            .resize(ResizeRequest {
                uuid: instance.uuid,
                vcpu: 2,
                memory_mib: 2048,
                disk_gib: 20,
                nested: true
            })
            .await,
        Err(Error::NestedUnavailable(_))
    ));
}

#[tokio::test]
async fn resize_capacity_failure_changes_nothing() {
    let f = fixture(false, false);
    let instance = setup(&f).await;
    f.store.data().resize_error = Some("the host has no room".into());
    assert!(
        f.manager
            .resize(ResizeRequest {
                uuid: instance.uuid,
                vcpu: 8,
                memory_mib: 8192,
                disk_gib: 40,
                nested: false
            })
            .await
            .is_err()
    );
    assert!(f.resizer.calls.lock().unwrap().is_empty());
}

fn copy_request(owner: User, name: &str) -> NewRequest {
    request(owner, name)
}

#[tokio::test]
async fn copy_instance() {
    let f = fixture(false, false);
    let source = setup(&f).await;
    f.manager.stop(&source.uuid).await.unwrap();
    let owner = f.store.user_by_id(1).await.unwrap();
    f.store.data().images.get_mut("debian-13").unwrap().kind = ImageKind::Oci;
    let clone = f
        .manager
        .copy(&source.uuid, copy_request(owner, "clone"))
        .await
        .unwrap();
    assert_ne!(clone.uuid, source.uuid);
    assert_eq!(clone.base_checksum, source.base_checksum);
    assert_eq!(
        tokio::fs::read(f.manager.overlay_path(&clone.uuid))
            .await
            .unwrap(),
        b"overlay"
    );
    let seed = f.iso.seeds.lock().unwrap()[&f.manager.seed_iso_path(&clone.uuid)].clone();
    assert_eq!(seed.instance_id, clone.uuid);
    assert_eq!(seed.hostname, "clone");
    assert!(
        seed.install_guest_agent,
        "copy must retain the source version's qcow2 behavior"
    );
}

#[tokio::test]
async fn copy_running_source_refused() {
    let f = fixture(false, false);
    let source = setup(&f).await;
    let owner = f.store.user_by_id(1).await.unwrap();
    assert!(matches!(
        f.manager
            .copy(&source.uuid, copy_request(owner, "clone"))
            .await,
        Err(Error::CopySourceRunning(_))
    ));
}

#[tokio::test]
async fn copy_disk_shrink_refused() {
    let f = fixture(false, false);
    let source = setup(&f).await;
    f.manager.stop(&source.uuid).await.unwrap();
    let owner = f.store.user_by_id(1).await.unwrap();
    let mut request = copy_request(owner, "clone");
    request.disk_gib = 10;
    assert!(matches!(
        f.manager.copy(&source.uuid, request).await,
        Err(Error::DiskShrink(_))
    ));
}

#[tokio::test]
async fn copy_grows_disk() {
    let f = fixture(false, false);
    let source = setup(&f).await;
    f.manager.stop(&source.uuid).await.unwrap();
    let owner = f.store.user_by_id(1).await.unwrap();
    let mut request = copy_request(owner, "clone");
    request.disk_gib = 30;
    let clone = f.manager.copy(&source.uuid, request).await.unwrap();
    assert_eq!(
        f.resizer.calls.lock().unwrap().as_slice(),
        &[(f.manager.overlay_path(&clone.uuid), 30)]
    );
}

#[tokio::test]
async fn copy_unwind_on_create_failure() {
    let f = fixture(false, false);
    let source = setup(&f).await;
    f.manager.stop(&source.uuid).await.unwrap();
    f.fake.set_hook(|operation, name| {
        if operation == "create" && name == "clone" {
            Err(HypervisorError::Operation("create failed".into()))
        } else {
            Ok(())
        }
    });
    let owner = f.store.user_by_id(1).await.unwrap();
    assert!(
        f.manager
            .copy(&source.uuid, copy_request(owner, "clone"))
            .await
            .is_err()
    );
    assert_eq!(f.store.data().instances.len(), 1);
    assert!(
        !f.manager
            .overlay_path("00000002-1111-4111-8111-111111111111")
            .exists()
    );
}

#[tokio::test]
async fn rename_stopped_instance() {
    let f = fixture(true, false);
    let instance = setup(&f).await;
    f.manager.stop(&instance.uuid).await.unwrap();
    f.manager.rename(&instance.uuid, "api").await.unwrap();
    assert_eq!(f.store.instance(&instance.uuid).await.unwrap().name, "api");
    assert!(
        f.definer
            .xml
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .contains("<name>api</name>")
    );
    assert_eq!(
        f.manager.overlay_path(&instance.uuid),
        f.manager
            .storage_dir
            .join(format!("{}.qcow2", instance.uuid))
    );
}

#[tokio::test]
async fn rename_running_instance_refused() {
    let f = fixture(true, false);
    let instance = setup(&f).await;
    assert!(matches!(
        f.manager.rename(&instance.uuid, "api").await,
        Err(Error::RenameNeedsStop(_))
    ));
}

#[tokio::test]
async fn rename_same_name_is_noop() {
    let f = fixture(false, false);
    let instance = setup(&f).await;
    f.manager.rename(&instance.uuid, "web").await.unwrap();
    assert!(f.definer.xml.lock().unwrap().is_empty());
}

#[tokio::test]
async fn rename_row_without_domain() {
    let f = fixture(false, false);
    let instance = setup(&f).await;
    f.fake.remove("web").await.unwrap();
    f.manager.rename(&instance.uuid, "api").await.unwrap();
    assert_eq!(f.store.instance(&instance.uuid).await.unwrap().name, "api");
}

#[tokio::test]
async fn rename_without_definer_refused() {
    let f = fixture(false, false);
    let instance = setup(&f).await;
    f.manager.stop(&instance.uuid).await.unwrap();
    assert!(
        f.manager
            .rename(&instance.uuid, "api")
            .await
            .unwrap_err()
            .to_string()
            .contains("cannot redefine")
    );
}

#[tokio::test]
async fn rename_define_failure_reverts_row() {
    let f = fixture(true, false);
    let instance = setup(&f).await;
    f.manager.stop(&instance.uuid).await.unwrap();
    *f.definer.fail.lock().unwrap() = true;
    assert!(f.manager.rename(&instance.uuid, "api").await.is_err());
    assert_eq!(f.store.instance(&instance.uuid).await.unwrap().name, "web");
}

#[tokio::test]
async fn rename_store_failure_leaves_domain() {
    let f = fixture(true, false);
    let instance = setup(&f).await;
    f.manager.stop(&instance.uuid).await.unwrap();
    f.store.data().rename_error = Some("cooldown".into());
    assert!(f.manager.rename(&instance.uuid, "api").await.is_err());
    assert!(f.fake.domain("web").is_some());
}

#[tokio::test]
async fn reconcile() {
    let f = fixture(false, false);
    let instance = setup(&f).await;
    f.fake.set_domain(FakeDomain {
        name: "orphan-domain".into(),
        uuid: "domain-only".into(),
        xml: String::new(),
        state: State::Stopped,
        autostart: false,
    });
    let mut row = instance;
    row.uuid = "row-only".into();
    row.name = "orphan-row".into();
    f.store.data().instances.push(row.clone());
    let report = f.manager.reconcile().await.unwrap();
    assert_eq!(report.domains_without_rows[0].uuid, "domain-only");
    assert!(
        report
            .rows_without_domains
            .iter()
            .any(|item| item.uuid == "row-only")
    );
}

#[tokio::test]
async fn reconcile_reports_and_never_mutates() {
    let f = fixture(false, false);
    setup(&f).await;
    let before = f.store.data().mutations.clone();
    f.manager.reconcile().await.unwrap();
    assert_eq!(f.store.data().mutations, before);
    assert!(
        !f.fake
            .calls()
            .iter()
            .any(|call| call.starts_with("remove "))
    );
}

#[tokio::test]
async fn reconcile_empty() {
    let f = fixture(false, false);
    setup(&f).await;
    assert!(f.manager.reconcile().await.unwrap().is_empty());
}

#[tokio::test]
async fn reconcile_ignores_instances_on_other_hosts() {
    let f = fixture(false, false);
    let local = setup(&f).await;
    let mut remote = local;
    remote.uuid = "remote-uuid".into();
    remote.name = "remote".into();
    remote.host_id = 2;
    f.store.data().instances.push(remote);

    let report = f.manager.reconcile().await.unwrap();
    assert!(report.rows_without_domains.is_empty());
}

#[tokio::test]
async fn poll_once_updates_observed_state() {
    let f = fixture(false, false);
    let instance = setup(&f).await;
    f.fake.stop("web").await.unwrap();
    f.manager.poll_once().await.unwrap();
    assert_eq!(
        f.store.instance(&instance.uuid).await.unwrap().state,
        State::Stopped
    );
}

#[tokio::test]
async fn poll_once_finishes_first_boot() {
    let f = fixture(true, false);
    let instance = setup(&f).await;
    assert!((f.manager.iso_exists)(
        &f.manager.seed_iso_path(&instance.uuid)
    ));
    f.manager.poll_once().await.unwrap();
    assert!(!(f.manager.iso_exists)(
        &f.manager.seed_iso_path(&instance.uuid)
    ));
    assert!(!f.definer.xml.lock().unwrap()[0].contains("seed.iso"));
}

#[tokio::test]
async fn poll_once_skips_first_boot_while_stopped() {
    let f = fixture(true, false);
    let instance = setup(&f).await;
    f.fake.stop("web").await.unwrap();
    f.manager.poll_once().await.unwrap();
    assert!((f.manager.iso_exists)(
        &f.manager.seed_iso_path(&instance.uuid)
    ));
}

#[tokio::test]
async fn handle_event() {
    let f = fixture(false, false);
    let instance = setup(&f).await;
    f.manager
        .handle_event(&instance.uuid, State::Stopped)
        .await
        .unwrap();
    assert_eq!(
        f.store.instance(&instance.uuid).await.unwrap().state,
        State::Stopped
    );
    f.manager
        .handle_event("unknown", State::Running)
        .await
        .unwrap();
}

#[tokio::test]
async fn handle_event_running_finishes_first_boot() {
    let f = fixture(true, false);
    let instance = setup(&f).await;
    f.manager
        .handle_event(&instance.uuid, State::Running)
        .await
        .unwrap();
    assert!(!(f.manager.iso_exists)(
        &f.manager.seed_iso_path(&instance.uuid)
    ));
}

#[tokio::test]
async fn first_boot_without_definer_still_deletes_iso() {
    let f = fixture(false, false);
    let instance = setup(&f).await;
    f.manager.finish_first_boot(&instance).await.unwrap();
    assert!(!(f.manager.iso_exists)(
        &f.manager.seed_iso_path(&instance.uuid)
    ));
    assert!(
        f.log
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|line| line.contains("cannot redefine"))
    );
}

#[tokio::test]
async fn run_poller_polls_on_interval() {
    let mut f = fixture(false, false);
    f.manager.poll_every = Duration::from_millis(10);
    setup(&f).await;
    let (send, receive) = tokio::sync::oneshot::channel();
    let poller = f.manager.run_poller(async {
        let _ = receive.await;
    });
    tokio::pin!(poller);
    tokio::select! { _ = &mut poller => panic!("stopped early"), _ = tokio::time::sleep(Duration::from_millis(35)) => {} }
    send.send(()).unwrap();
    poller.await.unwrap();
    assert!(
        f.fake
            .calls()
            .iter()
            .filter(|call| call.as_str() == "list ")
            .count()
            >= 1
    );
}

async fn seed_restore(f: &Fixture, count: usize) -> Vec<Instance> {
    let owner = f.store.add_user(1, "a", "10.77.0.0/24");
    f.store.add_image(Some("aa"));
    let mut result = Vec::new();
    for index in 0..count {
        let instance = f
            .manager
            .create(request(owner.clone(), &format!("vm{index}")))
            .await
            .unwrap();
        f.fake.stop(&instance.name).await.unwrap();
        f.store
            .set_observed_state(&instance.uuid, State::Stopped)
            .await
            .unwrap();
        result.push(instance);
    }
    result
}

#[tokio::test]
async fn restore_batches() {
    let mut f = fixture(false, false);
    f.manager.batch_size = 2;
    seed_restore(&f, 5).await;
    f.manager.restore().await.unwrap();
    let calls = f.fake.calls();
    assert_eq!(
        calls
            .iter()
            .filter(|call| call.starts_with("start "))
            .count(),
        5
    );
}

#[tokio::test]
async fn restore_only_starts_desired_running_observed_stopped() {
    let f = fixture(false, false);
    let instances = seed_restore(&f, 2).await;
    f.store
        .set_desired_state(&instances[1].uuid, DesiredState::Stopped)
        .await
        .unwrap();
    f.manager.restore().await.unwrap();
    assert!(f.fake.calls().contains(&"start vm0".into()));
    assert!(!f.fake.calls().contains(&"start vm1".into()));
}

struct SlowHypervisor {
    fake: Arc<Fake>,
    never_running: bool,
    checks: Mutex<usize>,
}
#[async_trait]
impl Hypervisor for SlowHypervisor {
    async fn create(&self, xml: &str) -> std::result::Result<(), HypervisorError> {
        self.fake.create(xml).await
    }
    async fn start(&self, name: &str) -> std::result::Result<(), HypervisorError> {
        if self.never_running {
            Ok(())
        } else {
            self.fake.start(name).await
        }
    }
    async fn stop(&self, name: &str) -> std::result::Result<StopResult, HypervisorError> {
        self.fake.stop(name).await
    }
    async fn reboot(&self, name: &str) -> std::result::Result<(), HypervisorError> {
        self.fake.reboot(name).await
    }
    async fn remove(&self, name: &str) -> std::result::Result<(), HypervisorError> {
        self.fake.remove(name).await
    }
    async fn list(&self) -> std::result::Result<Vec<DomainInfo>, HypervisorError> {
        self.fake.list().await
    }
    async fn state(&self, name: &str) -> std::result::Result<State, HypervisorError> {
        *self.checks.lock().unwrap() += 1;
        self.fake.state(name).await
    }
}

#[tokio::test]
async fn restore_waits_for_each_batch() {
    let mut f = fixture(false, false);
    seed_restore(&f, 2).await;
    let slow = Arc::new(SlowHypervisor {
        fake: f.fake.clone(),
        never_running: false,
        checks: Mutex::new(0),
    });
    f.manager.hyp = slow.clone();
    f.manager.restore().await.unwrap();
    assert!(*slow.checks.lock().unwrap() >= 2);
}

#[tokio::test]
async fn restore_timeout_moves_on() {
    let mut f = fixture(false, false);
    seed_restore(&f, 2).await;
    f.manager.start_poll = Duration::from_millis(10);
    f.manager.start_wait = Duration::from_millis(20);
    let slow = Arc::new(SlowHypervisor {
        fake: f.fake.clone(),
        never_running: true,
        checks: Mutex::new(0),
    });
    f.manager.hyp = slow;
    f.manager.restore().await.unwrap();
    assert_eq!(f.store.data().instances.len(), 2);
    assert!(!f.sleep.0.lock().unwrap().is_empty());
}

#[tokio::test]
async fn restore_clears_autostart() {
    let f = fixture(false, true);
    seed_restore(&f, 2).await;
    f.manager.restore().await.unwrap();
    assert_eq!(f.clearer.0.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn restore_logs_progress() {
    let f = fixture(false, false);
    seed_restore(&f, 1).await;
    f.manager.restore().await.unwrap();
    let log = f.log.0.lock().unwrap().join("\n");
    assert!(log.contains("batch 1"));
    assert!(log.contains("complete"));
}

#[tokio::test]
async fn restore_nothing_to_do() {
    let f = fixture(false, false);
    f.manager.restore().await.unwrap();
    assert!(
        f.log
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|line| line.contains("nothing to start"))
    );
}

#[derive(Default)]
struct RecordingRunner {
    calls: Mutex<Vec<Vec<String>>>,
    fail: Mutex<bool>,
}
#[async_trait]
impl CommandRunner for RecordingRunner {
    async fn run(&self, name: &OsStr, args: &[OsString]) -> std::result::Result<Vec<u8>, RunError> {
        let mut call = vec![name.to_string_lossy().into_owned()];
        call.extend(args.iter().map(|arg| arg.to_string_lossy().into_owned()));
        self.calls.lock().unwrap().push(call);
        if *self.fail.lock().unwrap() {
            Err(RunError::new(boxed("exit 1"), b"image is locked".to_vec()))
        } else {
            Ok(Vec::new())
        }
    }
}

#[tokio::test]
async fn qemu_img_resizer() {
    let runner = Arc::new(RecordingRunner::default());
    let resizer = QemuImgResizer::default().with_runner(runner.clone());
    resizer
        .resize_overlay(Path::new("/tmp/test.qcow2"), 30)
        .await
        .unwrap();
    assert_eq!(
        runner.calls.lock().unwrap()[0],
        ["qemu-img", "resize", "/tmp/test.qcow2", "30G"]
    );
}

#[tokio::test]
async fn qemu_img_resizer_custom_binary() {
    let runner = Arc::new(RecordingRunner::default());
    let resizer = QemuImgResizer::default()
        .with_runner(runner.clone())
        .with_qemu_img("/opt/qemu-img");
    resizer.resize_overlay(Path::new("disk"), 5).await.unwrap();
    assert_eq!(runner.calls.lock().unwrap()[0][0], "/opt/qemu-img");
}

#[tokio::test]
async fn qemu_img_resizer_failure() {
    let runner = Arc::new(RecordingRunner::default());
    *runner.fail.lock().unwrap() = true;
    let error = QemuImgResizer::default()
        .with_runner(runner)
        .resize_overlay(Path::new("disk"), 5)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("qemu-img resize"));
    assert!(error.to_string().contains("image is locked"));
}

#[tokio::test]
async fn qemu_img_resizer_rejects_non_positive() {
    let runner = Arc::new(RecordingRunner::default());
    let resizer = QemuImgResizer::default().with_runner(runner.clone());
    assert!(resizer.resize_overlay(Path::new("disk"), 0).await.is_err());
    assert!(runner.calls.lock().unwrap().is_empty());
}

#[test]
fn new_manager_validation() {
    let valid = || {
        let temp = tempfile::tempdir().unwrap();
        let store = Arc::new(TestStore::default());
        (
            temp,
            Config {
                hypervisor: Some(Arc::new(Fake::default())),
                store: Some(store),
                fleet: Some(Arc::new(TestImages::new(
                    Arc::new(Fake::default()),
                    Arc::new(TestIso::default()),
                    PathBuf::new(),
                ))),
                iso: Some(Arc::new(TestIso::default())),
                plan: Some(Plan::new("10.77.0.0/16").unwrap()),
                ..Config::default()
            },
        )
    };
    for missing in 0..6 {
        let (temp, mut config) = valid();
        config.storage_dir = temp.path().into();
        match missing {
            0 => config.hypervisor = None,
            1 => config.store = None,
            2 => config.fleet = None,
            3 => config.iso = None,
            4 => config.plan = None,
            _ => config.storage_dir = PathBuf::new(),
        }
        assert!(Manager::new(config).is_err());
    }
}

#[test]
fn manager_defaults() {
    let f = fixture(false, false);
    assert_eq!(f.manager.batch_size, 4);
    assert_eq!(f.manager.cooldown, Duration::from_secs(86400));
    assert_eq!(f.manager.poll_every, Duration::from_secs(30));
    assert!(f.manager.overlay_path("u").ends_with("u.qcow2"));
}

#[test]
fn random_uuid() {
    let mut seen = std::collections::HashSet::new();
    for _ in 0..100 {
        let id = super::manager::random_uuid();
        assert_eq!(id.len(), 36);
        assert_eq!(&id[14..15], "4");
        assert!(seen.insert(id));
    }
}

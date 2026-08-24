use std::ops::Deref;
use std::sync::{Arc, Mutex};

use bento_config::Ipv4Prefix;
use bento_types::{DesiredState, Host, Image, ImageVersion, Instance, State, User, Visibility};
use rusqlite::Connection;
use time::macros::datetime;
use time::{Duration, OffsetDateTime};

use crate::{SCHEMA_SQL, Store};

#[derive(Clone)]
pub(crate) struct FakeClock(Arc<Mutex<OffsetDateTime>>);

impl FakeClock {
    pub(crate) fn new() -> Self {
        Self(Arc::new(Mutex::new(datetime!(2026-01-01 0:00 UTC))))
    }

    pub(crate) fn now(&self) -> OffsetDateTime {
        *self.0.lock().unwrap()
    }

    pub(crate) fn advance(&self, duration: Duration) {
        let mut value = self.0.lock().unwrap();
        *value += duration;
    }
}

pub(crate) struct TestStore {
    pub(crate) store: Store,
    _directory: tempfile::TempDir,
}

impl Deref for TestStore {
    type Target = Store;

    fn deref(&self) -> &Self::Target {
        &self.store
    }
}

pub(crate) async fn new_test_store() -> TestStore {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join("bento.db"))
        .await
        .unwrap();
    TestStore {
        store,
        _directory: directory,
    }
}

pub(crate) async fn new_test_store_with_clock(clock: FakeClock) -> TestStore {
    let directory = tempfile::tempdir().unwrap();
    let clock_for_store = clock.clone();
    let store = Store::open_with_clock(directory.path().join("bento.db"), move || {
        clock_for_store.now()
    })
    .await
    .unwrap();
    TestStore {
        store,
        _directory: directory,
    }
}

pub(crate) fn test_range() -> Ipv4Prefix {
    Ipv4Prefix {
        addr: "10.100.0.0".parse().unwrap(),
        bits: 16,
    }
}

pub(crate) async fn seed_store(store: &Store) -> (User, Host) {
    let user = store
        .register_user("alice", "alice@example.org", None, test_range())
        .await
        .unwrap();
    let host = store.ensure_host("host1", "qemu:///system").await.unwrap();
    store
        .upsert_image(Image {
            name: "debian-13".into(),
            url: "https://example.org/d13.qcow2".into(),
            kind: Default::default(),
            pinned_checksum: None,
            current_checksum: None,
        })
        .await
        .unwrap();
    store
        .add_image_version(ImageVersion {
            checksum: "sha256-aa".into(),
            image_name: "debian-13".into(),
            path: "/var/lib/bento/images/sha256-aa.qcow2".into(),
            size: 1,
            kind: Default::default(),
            source_digest: None,
            fetched_at: datetime!(2026-01-01 0:00 UTC),
        })
        .await
        .unwrap();
    (user, host)
}

pub(crate) fn test_instance(number: usize, name: &str, owner: &User, host: &Host) -> Instance {
    Instance {
        uuid: format!("uuid-{number:03}"),
        name: name.into(),
        owner_id: owner.id,
        host_id: host.id,
        image_name: "debian-13".into(),
        base_checksum: "sha256-aa".into(),
        state: State::Stopped,
        desired_state: DesiredState::Running,
        address: format!("10.100.0.{}", number + 2),
        mac: format!("52:54:00:00:00:{:02x}", number + 1),
        vcpu: 1,
        memory_mib: 512,
        disk_gib: 10,
        nested: false,
        ksm: true,
        http_port: 80,
        visibility: Visibility::Off,
        created_at: OffsetDateTime::UNIX_EPOCH,
        last_seen_at: None,
    }
}

fn open_test_db() -> (tempfile::TempDir, Connection) {
    let directory = tempfile::tempdir().unwrap();
    let connection = Connection::open(directory.path().join("test.db")).unwrap();
    connection
        .pragma_update(None, "journal_mode", "WAL")
        .unwrap();
    connection
        .pragma_update(None, "foreign_keys", true)
        .unwrap();
    connection
        .busy_timeout(std::time::Duration::from_millis(5000))
        .unwrap();
    (directory, connection)
}

fn seed(connection: &Connection) {
    connection
        .execute_batch(
            "INSERT INTO users (id, name, email, subnet, created_at) \
             VALUES (1, 'alice', 'alice@example.org', '10.100.1.0/24', \
                     '2026-01-01T00:00:00Z'); \
             INSERT INTO hosts (id, name, libvirt_uri, created_at) \
             VALUES (1, 'host1', 'qemu:///system', '2026-01-01T00:00:00Z'); \
             INSERT INTO images (name, url) \
             VALUES ('debian-13', 'https://example.org/d13.qcow2'); \
             INSERT INTO image_versions (checksum, image_name, path, size, fetched_at) \
             VALUES ('sha256-aa', 'debian-13', \
                     '/var/lib/bento/images/sha256-aa.qcow2', 1, \
                     '2026-01-01T00:00:00Z');",
        )
        .unwrap();
}

#[tokio::test]
async fn schema_executes() {
    let (_directory, connection) = open_test_db();
    connection.execute_batch(SCHEMA_SQL).unwrap();

    for table in [
        "users",
        "quotas",
        "ssh_keys",
        "hosts",
        "images",
        "image_versions",
        "image_source_versions",
        "instances",
        "shares",
        "released_names",
        "tokens",
    ] {
        let found: String = connection
            .query_row(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name = ?",
                [table],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(found, table);
    }
    for index in ["idx_ssh_keys_fingerprint", "idx_instances_name"] {
        let found: String = connection
            .query_row(
                "SELECT name FROM sqlite_master WHERE type = 'index' AND name = ?",
                [index],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(found, index);
    }
    connection.execute_batch(SCHEMA_SQL).unwrap();
}

#[tokio::test]
async fn instance_name_unique() {
    let (_directory, connection) = open_test_db();
    connection.execute_batch(SCHEMA_SQL).unwrap();
    seed(&connection);
    let insert = |uuid: &str, name: &str, address: &str, mac: &str| {
        connection.execute(
            "INSERT INTO instances \
             (uuid, name, owner_id, host_id, image_name, base_checksum, \
              address, mac, vcpu, memory, disk, created_at) \
             VALUES (?, ?, 1, 1, 'debian-13', 'sha256-aa', ?, ?, 1, 512, 10, \
                     '2026-01-01T00:00:00Z')",
            rusqlite::params![uuid, name, address, mac],
        )
    };
    insert("uuid-1", "web", "10.100.1.2", "52:54:00:00:00:01").unwrap();
    assert!(insert("uuid-2", "web", "10.100.1.3", "52:54:00:00:00:02").is_err());
    assert!(insert("uuid-1", "other", "10.100.1.4", "52:54:00:00:00:03").is_err());
}

#[tokio::test]
async fn shares_cascade_on_instance_delete() {
    let (_directory, connection) = open_test_db();
    connection.execute_batch(SCHEMA_SQL).unwrap();
    seed(&connection);
    connection
        .execute_batch(
            "INSERT INTO instances \
             (uuid, name, owner_id, host_id, image_name, base_checksum, address, mac, \
              vcpu, memory, disk, created_at) \
             VALUES ('uuid-1', 'web', 1, 1, 'debian-13', 'sha256-aa', \
                     '10.100.1.2', '52:54:00:00:00:01', 1, 512, 10, \
                     '2026-01-01T00:00:00Z'); \
             INSERT INTO users (id, name, email, subnet, created_at) \
             VALUES (2, 'bob', 'bob@example.org', '10.100.2.0/24', \
                     '2026-01-01T00:00:00Z'); \
             INSERT INTO shares (instance_uuid, user_id, created_at) \
             VALUES ('uuid-1', 2, '2026-01-01T00:00:00Z'); \
             DELETE FROM instances WHERE uuid = 'uuid-1';",
        )
        .unwrap();
    let count: i64 = connection
        .query_row("SELECT COUNT(*) FROM shares", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn opens_and_migrates_a_pre_oci_database() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("legacy.db");
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE images (
                name TEXT PRIMARY KEY,
                url TEXT NOT NULL,
                pinned_checksum TEXT,
                current_checksum TEXT REFERENCES image_versions(checksum)
             );
             CREATE TABLE image_versions (
                checksum TEXT PRIMARY KEY,
                image_name TEXT NOT NULL REFERENCES images(name),
                path TEXT NOT NULL UNIQUE,
                size INTEGER NOT NULL,
                fetched_at TEXT NOT NULL
             );
             INSERT INTO images (name, url) VALUES ('legacy', 'https://example/legacy.qcow2');",
        )
        .unwrap();
    drop(connection);

    let store = Store::open(&path).await.unwrap();
    let image = store.image("legacy").await.unwrap();
    assert_eq!(image.kind, bento_types::ImageKind::Qcow2);
    store
        .add_image_version(ImageVersion {
            checksum: "legacy-checksum".into(),
            image_name: "legacy".into(),
            path: "/tmp/legacy-checksum.qcow2".into(),
            size: 1,
            kind: Default::default(),
            source_digest: Some("sha256:source".into()),
            fetched_at: datetime!(2026-01-01 0:00 UTC),
        })
        .await
        .unwrap();
    assert_eq!(
        store.image_versions("legacy").await.unwrap()[0]
            .source_digest
            .as_deref(),
        Some("sha256:source")
    );
    assert_eq!(
        store.image_versions("legacy").await.unwrap()[0].kind,
        bento_types::ImageKind::Qcow2
    );
}

#[tokio::test]
async fn migrates_bootc_version_provenance_from_the_initial_oci_schema() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("initial-oci.db");
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE images (
                name TEXT PRIMARY KEY,
                url TEXT NOT NULL,
                kind TEXT NOT NULL DEFAULT 'qcow2',
                pinned_checksum TEXT,
                current_checksum TEXT REFERENCES image_versions(checksum)
             );
             CREATE TABLE image_versions (
                checksum TEXT PRIMARY KEY,
                image_name TEXT NOT NULL REFERENCES images(name),
                path TEXT NOT NULL UNIQUE,
                size INTEGER NOT NULL,
                source_digest TEXT,
                fetched_at TEXT NOT NULL
             );
             INSERT INTO images (name, url, kind)
             VALUES ('bootc', 'quay.io/example/os:latest', 'oci');
             INSERT INTO image_versions
                 (checksum, image_name, path, size, source_digest, fetched_at)
             VALUES ('disk-checksum', 'bootc', '/images/disk.qcow2', 1,
                     'sha256:source', '2026-01-01T00:00:00Z');",
        )
        .unwrap();
    drop(connection);

    let store = Store::open(&path).await.unwrap();
    let version = store.image_version("disk-checksum").await.unwrap();
    assert_eq!(version.kind, bento_types::ImageKind::Oci);
    assert_eq!(
        store
            .image_version_for_source("bootc", "sha256:source")
            .await
            .unwrap()
            .unwrap()
            .checksum,
        "disk-checksum"
    );
}

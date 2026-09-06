-- Bento SQLite schema, SPEC.md section 12.
--
-- Pragmas are connection settings, not schema, so the store applies them
-- at open time, before this file runs:
--
--   PRAGMA journal_mode = WAL;   -- SPEC 12: write-ahead logging.
--   PRAGMA foreign_keys = ON;    -- SQLite does not enforce FKs by default.
--   PRAGMA busy_timeout = 5000;
--
-- Times are stored as RFC 3339 UTC text. Memory is MiB, disk is GiB.

CREATE TABLE IF NOT EXISTS users (
    id           INTEGER PRIMARY KEY,
    name         TEXT    NOT NULL UNIQUE,
    email        TEXT    NOT NULL,
    oidc_subject TEXT    UNIQUE,
    subnet       TEXT    NOT NULL UNIQUE, -- the /24 of the user, SPEC 6.2
    created_at   TEXT    NOT NULL
);

-- Bento had a per-user quota table here. Section 6.1 replaced it with a
-- host capacity check, so the table holds nothing that any code reads.
-- This statement predates the numbered migrations and stays where it is:
-- it is safe on a database that never had the table, and moving it now
-- would only give the same drop a second home.
DROP TABLE IF EXISTS quotas;

CREATE TABLE IF NOT EXISTS ssh_keys (
    id          INTEGER PRIMARY KEY,
    user_id     INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    public_key  TEXT    NOT NULL,
    fingerprint TEXT    NOT NULL,
    comment     TEXT    NOT NULL DEFAULT '',
    created_at  TEXT    NOT NULL
);

-- The SSH frontend reads this column on every connection (SPEC 12).
CREATE INDEX IF NOT EXISTS idx_ssh_keys_fingerprint ON ssh_keys(fingerprint);

-- The machine ID is the identity of a host and the name is a label,
-- the relation the instance UUID and the instance name already have
-- (MULTI-NODE 16). Its unique index is created by migration 2, not here:
-- this file runs before the migrations, so on a database that predates
-- the column an index over it would name a column that does not exist.
CREATE TABLE IF NOT EXISTS hosts (
    id          INTEGER PRIMARY KEY,
    machine_id  TEXT,
    name        TEXT    NOT NULL UNIQUE,
    libvirt_uri TEXT    NOT NULL,
    created_at  TEXT    NOT NULL
);

-- Settings that belong to the deployment, not to one host
-- (MULTI-NODE 19). One row, id 1.
CREATE TABLE IF NOT EXISTS deployment (
    id            INTEGER PRIMARY KEY CHECK (id = 1),
    -- 24, 25, 26, or 27: how a user /24 divides into runner slots
    -- (MULTI-NODE 7.1). A /24 is one slot, which is what version 1 was.
    -- Setup persists the real value on first initialization.
    runner_prefix INTEGER NOT NULL DEFAULT 24
                  CHECK (runner_prefix BETWEEN 24 AND 27)
);
INSERT OR IGNORE INTO deployment (id) VALUES (1);

-- The controller lease and its epoch (MULTI-NODE 11). One row, id 1.
-- Only the holder may dispatch to a runner.
CREATE TABLE IF NOT EXISTS controller_lease (
    id         INTEGER PRIMARY KEY CHECK (id = 1),
    -- Random, one for each controller process. NULL when free.
    holder_id  TEXT,
    -- Durable and strictly increasing. A runner records the highest
    -- epoch it has accepted and refuses every lower one after that.
    epoch      INTEGER NOT NULL DEFAULT 0,
    expires_at TEXT
);
INSERT OR IGNORE INTO controller_lease (id) VALUES (1);

-- Slot ownership is global, not per user: the runner that owns slot 1
-- owns slot 1 of every user /24 (MULTI-NODE 7.2). The slot number is the
-- primary key, and that is what permits one active owner for a slot
-- (MULTI-NODE 16).
CREATE TABLE IF NOT EXISTS runner_slots (
    slot                INTEGER PRIMARY KEY,
    state               TEXT    NOT NULL DEFAULT 'active'
                        CHECK (state IN ('active', 'draining', 'moving')),
    owner_host_id       INTEGER NOT NULL REFERENCES hosts(id),
    -- Increases every time the slot changes hands. A runner refuses an
    -- ownership claim older than the one it holds.
    ownership_epoch     INTEGER NOT NULL DEFAULT 0,
    -- Set only while `state` is 'moving' (MULTI-NODE 17).
    source_host_id      INTEGER REFERENCES hosts(id),
    destination_host_id INTEGER REFERENCES hosts(id),
    operation_id        TEXT
);

-- What the controller last saw when it called a runner (MULTI-NODE 16).
-- These are observations, not configuration, so they live apart from the
-- `hosts` row. A row appears on first contact and is replaced on each
-- later one.
CREATE TABLE IF NOT EXISTS host_observations (
    host_id               INTEGER PRIMARY KEY REFERENCES hosts(id) ON DELETE CASCADE,
    health                TEXT    NOT NULL DEFAULT 'unknown'
                          CHECK (health IN ('unknown', 'ok', 'unreachable', 'mismatched')),
    last_contact_at       TEXT,
    -- The highest controller epoch the runner says it has accepted.
    accepted_epoch        INTEGER NOT NULL DEFAULT 0,
    arch                  TEXT,
    cpu_count             INTEGER,
    memory_total_mib      INTEGER,
    storage_total_gib     INTEGER,
    storage_available_gib INTEGER,
    hypervisor_version    TEXT,
    -- Why the last call failed, for the operator to read.
    last_error            TEXT
);

-- Which image versions each machine holds (MULTI-NODE 13.2).
--
-- A machine holds a set, not one version. Every instance is backed by
-- the version it was built from, and that file is the overlay's backing
-- file, so a machine keeps a version while any of its instances needs it
-- (SPEC 5.1). A newer build lands beside the older one.
CREATE TABLE IF NOT EXISTS host_images (
    host_id     INTEGER NOT NULL REFERENCES hosts(id) ON DELETE CASCADE,
    image_name  TEXT    NOT NULL,
    checksum    TEXT    NOT NULL,
    verified_at TEXT    NOT NULL,
    PRIMARY KEY (host_id, image_name, checksum)
);

CREATE TABLE IF NOT EXISTS images (
    name             TEXT PRIMARY KEY,
    url              TEXT NOT NULL,
    kind             TEXT NOT NULL DEFAULT 'qcow2'
                     CHECK (kind IN ('qcow2', 'oci')),
    pinned_checksum  TEXT,
    current_checksum TEXT REFERENCES image_versions(checksum)
);

CREATE TABLE IF NOT EXISTS image_versions (
    checksum   TEXT    PRIMARY KEY, -- content address, "sha256-<hex>" (SPEC 5.1)
    image_name TEXT    NOT NULL REFERENCES images(name),
    path       TEXT    NOT NULL UNIQUE,
    size       INTEGER NOT NULL,
    kind       TEXT    NOT NULL DEFAULT 'qcow2'
               CHECK (kind IN ('qcow2', 'oci')),
    source_digest TEXT,
    fetched_at TEXT    NOT NULL
);

-- One physical disk can be selected by more than one allowlist name. Keep
-- source-digest cache keys separately from the content-addressed file row.
CREATE TABLE IF NOT EXISTS image_source_versions (
    image_name    TEXT NOT NULL REFERENCES images(name) ON DELETE CASCADE,
    source_digest TEXT NOT NULL,
    checksum      TEXT NOT NULL REFERENCES image_versions(checksum) ON DELETE CASCADE,
    PRIMARY KEY (image_name, source_digest)
);

-- The uuid is the identifier; the name is a label (SPEC 7.2).
CREATE TABLE IF NOT EXISTS instances (
    uuid          TEXT    PRIMARY KEY, -- libvirt domain UUID
    name          TEXT    NOT NULL,
    owner_id      INTEGER NOT NULL REFERENCES users(id),
    host_id       INTEGER NOT NULL REFERENCES hosts(id),
    image_name    TEXT    NOT NULL REFERENCES images(name),
    base_checksum TEXT    NOT NULL REFERENCES image_versions(checksum),
    state         TEXT    NOT NULL DEFAULT 'stopped'
                  CHECK (state IN ('running', 'stopped', 'starting')),
    desired_state TEXT    NOT NULL DEFAULT 'running'
                  CHECK (desired_state IN ('running', 'stopped')),
    address       TEXT    NOT NULL UNIQUE, -- assigned before boot, SPEC 6.2
    mac           TEXT    NOT NULL UNIQUE, -- locally administered, SPEC 6.2
    vcpu          INTEGER NOT NULL,
    memory        INTEGER NOT NULL, -- MiB
    disk          INTEGER NOT NULL, -- GiB, virtual size
    nested        INTEGER NOT NULL DEFAULT 0, -- boolean, SPEC 5.5
    ksm           INTEGER NOT NULL DEFAULT 1, -- boolean, SPEC 5.4
    http_port     INTEGER NOT NULL DEFAULT 80, -- SPEC 9.1
    visibility    TEXT    NOT NULL DEFAULT 'off'
                  CHECK (visibility IN ('off', 'private', 'public')),
    created_at    TEXT    NOT NULL,
    last_seen_at  TEXT
);

-- Unique across the deployment, not per user (SPEC 7.2).
CREATE UNIQUE INDEX IF NOT EXISTS idx_instances_name ON instances(name);
CREATE INDEX IF NOT EXISTS idx_instances_owner ON instances(owner_id);

-- Shares key on the UUID, never on the name (SPEC 7.2, 12).
CREATE TABLE IF NOT EXISTS shares (
    instance_uuid TEXT    NOT NULL REFERENCES instances(uuid) ON DELETE CASCADE,
    user_id       INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at    TEXT    NOT NULL,
    PRIMARY KEY (instance_uuid, user_id)
);

-- Cooldown record (SPEC 7.2). Rows are kept after the cooldown expires;
-- readers compare released_at against the configured cooldown.
CREATE TABLE IF NOT EXISTS released_names (
    name              TEXT    PRIMARY KEY,
    previous_owner_id INTEGER NOT NULL REFERENCES users(id),
    released_at       TEXT    NOT NULL
);

-- Pending SSH key links (SPEC 13). An unknown key presented to the SSH
-- frontend creates one of these and nothing else; no account exists until
-- a browser session confirms the fingerprint. Only the hash of the link
-- token is stored, as for tokens.hash.
CREATE TABLE IF NOT EXISTS pairings (
    id             INTEGER PRIMARY KEY,
    token_hash     TEXT    NOT NULL UNIQUE,
    public_key     TEXT    NOT NULL,
    fingerprint    TEXT    NOT NULL,
    comment        TEXT    NOT NULL DEFAULT '',
    created_at     TEXT    NOT NULL,
    expires_at     TEXT    NOT NULL,
    -- Set when the link is used. A pairing is single-use.
    linked_user_id INTEGER REFERENCES users(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS tokens (
    id         INTEGER PRIMARY KEY,
    user_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    hash       TEXT    NOT NULL UNIQUE, -- only the hash is stored, SPEC 13
    expires_at TEXT    NOT NULL
);

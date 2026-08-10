package store

import (
	"database/sql"
	"path/filepath"
	"testing"

	_ "modernc.org/sqlite"
)

// TestSchemaExecutes validates that schema.sql is accepted by the SQLite
// build Bento ships with, under the pragmas the store applies at open time.
func TestSchemaExecutes(t *testing.T) {
	db := openTestDB(t)
	if _, err := db.Exec(SchemaSQL); err != nil {
		t.Fatalf("schema.sql failed to execute: %v", err)
	}

	wantTables := []string{
		"users", "quotas", "ssh_keys", "hosts", "images",
		"image_versions", "instances", "shares", "released_names", "tokens",
	}
	for _, table := range wantTables {
		var name string
		err := db.QueryRow(
			`SELECT name FROM sqlite_master WHERE type = 'table' AND name = ?`, table,
		).Scan(&name)
		if err != nil {
			t.Errorf("table %q missing: %v", table, err)
		}
	}

	wantIndexes := []string{"idx_ssh_keys_fingerprint", "idx_instances_name"}
	for _, index := range wantIndexes {
		var name string
		err := db.QueryRow(
			`SELECT name FROM sqlite_master WHERE type = 'index' AND name = ?`, index,
		).Scan(&name)
		if err != nil {
			t.Errorf("index %q missing: %v", index, err)
		}
	}

	// The schema must be idempotent: a second run is a no-op.
	if _, err := db.Exec(SchemaSQL); err != nil {
		t.Fatalf("schema.sql is not idempotent: %v", err)
	}
}

// TestInstanceNameUnique checks the deployment-wide unique name index
// (SPEC 7.2) and the uuid primary key.
func TestInstanceNameUnique(t *testing.T) {
	db := openTestDB(t)
	if _, err := db.Exec(SchemaSQL); err != nil {
		t.Fatal(err)
	}
	seed(t, db)

	insert := func(uuid, name, address, mac string) error {
		_, err := db.Exec(`INSERT INTO instances
			(uuid, name, owner_id, host_id, image_name, base_checksum,
			 address, mac, vcpu, memory, disk, created_at)
			VALUES (?, ?, 1, 1, 'debian-13', 'sha256-aa', ?, ?, 1, 512, 10, '2026-01-01T00:00:00Z')`,
			uuid, name, address, mac)
		return err
	}
	if err := insert("uuid-1", "web", "10.100.1.2", "52:54:00:00:00:01"); err != nil {
		t.Fatalf("first insert: %v", err)
	}
	if err := insert("uuid-2", "web", "10.100.1.3", "52:54:00:00:00:02"); err == nil {
		t.Error("duplicate name accepted, want unique index violation")
	}
	if err := insert("uuid-1", "other", "10.100.1.4", "52:54:00:00:00:03"); err == nil {
		t.Error("duplicate uuid accepted, want primary key violation")
	}
}

// TestSharesCascadeOnInstanceDelete checks that deleting an instance row
// removes its shares (SPEC 7.2: a share must never outlive the UUID).
func TestSharesCascadeOnInstanceDelete(t *testing.T) {
	db := openTestDB(t)
	if _, err := db.Exec(SchemaSQL); err != nil {
		t.Fatal(err)
	}
	seed(t, db)

	mustExec(t, db, `INSERT INTO instances
		(uuid, name, owner_id, host_id, image_name, base_checksum,
		 address, mac, vcpu, memory, disk, created_at)
		VALUES ('uuid-1', 'web', 1, 1, 'debian-13', 'sha256-aa',
		 '10.100.1.2', '52:54:00:00:00:01', 1, 512, 10, '2026-01-01T00:00:00Z')`)
	mustExec(t, db, `INSERT INTO users (id, name, email, subnet, created_at)
		VALUES (2, 'bob', 'bob@example.org', '10.100.2.0/24', '2026-01-01T00:00:00Z')`)
	mustExec(t, db, `INSERT INTO shares (instance_uuid, user_id, created_at)
		VALUES ('uuid-1', 2, '2026-01-01T00:00:00Z')`)

	mustExec(t, db, `DELETE FROM instances WHERE uuid = 'uuid-1'`)

	var n int
	if err := db.QueryRow(`SELECT COUNT(*) FROM shares`).Scan(&n); err != nil {
		t.Fatal(err)
	}
	if n != 0 {
		t.Errorf("shares remaining after instance delete = %d, want 0", n)
	}
}

func openTestDB(t *testing.T) *sql.DB {
	t.Helper()
	db, err := sql.Open("sqlite", filepath.Join(t.TempDir(), "test.db"))
	if err != nil {
		t.Fatalf("open: %v", err)
	}
	t.Cleanup(func() { db.Close() })
	for _, pragma := range []string{
		`PRAGMA journal_mode = WAL`,
		`PRAGMA foreign_keys = ON`,
		`PRAGMA busy_timeout = 5000`,
	} {
		if _, err := db.Exec(pragma); err != nil {
			t.Fatalf("%s: %v", pragma, err)
		}
	}
	return db
}

// seed inserts the reference rows the instances table needs.
func seed(t *testing.T, db *sql.DB) {
	t.Helper()
	mustExec(t, db, `INSERT INTO users (id, name, email, subnet, created_at)
		VALUES (1, 'alice', 'alice@example.org', '10.100.1.0/24', '2026-01-01T00:00:00Z')`)
	mustExec(t, db, `INSERT INTO hosts (id, name, libvirt_uri, created_at)
		VALUES (1, 'host1', 'qemu:///system', '2026-01-01T00:00:00Z')`)
	mustExec(t, db, `INSERT INTO images (name, url) VALUES ('debian-13', 'https://example.org/d13.qcow2')`)
	mustExec(t, db, `INSERT INTO image_versions (checksum, image_name, path, size, fetched_at)
		VALUES ('sha256-aa', 'debian-13', '/var/lib/bento/images/sha256-aa.qcow2', 1, '2026-01-01T00:00:00Z')`)
}

func mustExec(t *testing.T, db *sql.DB, query string, args ...any) {
	t.Helper()
	if _, err := db.Exec(query, args...); err != nil {
		t.Fatalf("%s: %v", query, err)
	}
}

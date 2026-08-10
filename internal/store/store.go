// Package store persists Bento state in SQLite (SPEC section 12).
//
// The store is the data layer of the control plane. The control plane is
// the only writer (SPEC section 4); the store enforces that discipline in
// process by holding a single database connection, so every transaction is
// serialized and a check-then-insert pair can never interleave with another
// writer.
//
// The driver is modernc.org/sqlite: pure Go, no cgo (SPEC 4.1).
package store

import (
	"database/sql"
	"errors"
	"fmt"
	"net/url"
	"time"

	_ "embed"

	_ "modernc.org/sqlite"
)

// SchemaSQL is the full database schema from SPEC section 12.
//
//go:embed schema.sql
var SchemaSQL string

// ErrNotFound is returned when a lookup matches no row.
var ErrNotFound = errors.New("store: not found")

// ErrNameTaken is returned when a live instance already holds a name
// (the deployment-wide unique index of SPEC 7.2).
var ErrNameTaken = errors.New("store: name is taken by an existing instance")

// ErrTokenExpired is returned by TokenByHash for a token past its expiry.
var ErrTokenExpired = errors.New("store: token expired")

// QuotaError reports which of the four limits (SPEC 6.1) a create or
// resize would exceed.
type QuotaError struct {
	Limit     string // "instances", "vcpu", "memory", or "disk"
	Used      int64  // current use, before the request
	Requested int64  // what the request adds
	Max       int64  // the limit
}

func (e *QuotaError) Error() string {
	return fmt.Sprintf("store: quota exceeded: %s limit is %d, %d in use, %d requested",
		e.Limit, e.Max, e.Used, e.Requested)
}

// NameCooldownError reports that a released name still belongs to another
// user's cooldown window (SPEC 7.2). Remaining feeds the error message the
// CLI shows (SPEC 15).
type NameCooldownError struct {
	Name      string
	Remaining time.Duration
}

func (e *NameCooldownError) Error() string {
	return fmt.Sprintf("store: name %q was released by another user and is in cooldown for another %s",
		e.Name, e.Remaining.Round(time.Second))
}

// Store is the Bento data layer. All methods are safe for concurrent use;
// the single underlying connection serializes them.
type Store struct {
	db  *sql.DB
	now func() time.Time
}

// Option configures a Store at open time.
type Option func(*Store)

// WithClock injects the time source. Tests use it to drive the name
// cooldown and token expiry deterministically.
func WithClock(now func() time.Time) Option {
	return func(s *Store) { s.now = now }
}

// Open opens (creating if needed) the database at path, applies the
// connection pragmas (WAL, foreign keys, busy timeout), and applies the
// schema. Transactions begin with BEGIN IMMEDIATE so a write transaction
// holds the write lock from its first statement.
func Open(path string, opts ...Option) (*Store, error) {
	dsn := "file:" + url.PathEscape(path) + "?_txlock=immediate" +
		"&_pragma=journal_mode(WAL)" +
		"&_pragma=foreign_keys(1)" +
		"&_pragma=busy_timeout(5000)"
	db, err := sql.Open("sqlite", dsn)
	if err != nil {
		return nil, fmt.Errorf("store: open %s: %w", path, err)
	}
	// Single writer discipline (SPEC 4): one connection, so every
	// transaction in this process is fully serialized.
	db.SetMaxOpenConns(1)
	if _, err := db.Exec(SchemaSQL); err != nil {
		db.Close()
		return nil, fmt.Errorf("store: apply schema: %w", err)
	}
	s := &Store{db: db, now: time.Now}
	for _, opt := range opts {
		opt(s)
	}
	return s, nil
}

// Close closes the database.
func (s *Store) Close() error { return s.db.Close() }

// inTx runs fn inside one transaction, committing on nil and rolling back
// on error.
func (s *Store) inTx(fn func(tx *sql.Tx) error) error {
	tx, err := s.db.Begin()
	if err != nil {
		return err
	}
	defer tx.Rollback()
	if err := fn(tx); err != nil {
		return err
	}
	return tx.Commit()
}

// Times are stored as RFC 3339 UTC text (schema.sql).

func fmtTime(t time.Time) string { return t.UTC().Format(time.RFC3339Nano) }

func parseTime(s string) (time.Time, error) {
	if s == "" {
		return time.Time{}, nil
	}
	return time.Parse(time.RFC3339Nano, s)
}

func parseNullTime(s sql.NullString) (time.Time, error) {
	if !s.Valid {
		return time.Time{}, nil
	}
	return parseTime(s.String)
}

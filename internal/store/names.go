package store

import (
	"database/sql"
	"errors"
	"time"

	"github.com/abbyfluoroethane/bento/internal/types"
)

// ClaimName checks whether userID may take name under the cooldown rules
// of SPEC 7.2:
//
//  1. The previous owner of a released name may retake it at once.
//  2. Any other user must wait out the cooldown; the error carries the
//     remaining time for the CLI message (SPEC 15).
//  3. A name held by a live instance is never available (ErrNameTaken).
//
// Released rows are kept after expiry (SPEC 12); the check compares the
// timestamp instead of deleting.
//
// The check alone is advisory: CreateInstance and RenameInstance rerun it
// inside their own transactions.
func (s *Store) ClaimName(name string, userID int64, cooldown time.Duration) error {
	return s.inTx(func(tx *sql.Tx) error {
		return s.claimNameTx(tx, name, userID, cooldown)
	})
}

// claimNameTx is ClaimName inside an existing transaction.
func (s *Store) claimNameTx(tx *sql.Tx, name string, userID int64, cooldown time.Duration) error {
	var one int
	err := tx.QueryRow(`SELECT 1 FROM instances WHERE name = ?`, name).Scan(&one)
	if err == nil {
		return ErrNameTaken
	}
	if !errors.Is(err, sql.ErrNoRows) {
		return err
	}

	var (
		prevOwner  int64
		releasedAt string
	)
	err = tx.QueryRow(`SELECT previous_owner_id, released_at FROM released_names WHERE name = ?`,
		name).Scan(&prevOwner, &releasedAt)
	if errors.Is(err, sql.ErrNoRows) {
		return nil // never released, free to take
	}
	if err != nil {
		return err
	}
	if prevOwner == userID {
		return nil // rule 1: the releaser retakes at once
	}
	released, err := parseTime(releasedAt)
	if err != nil {
		return err
	}
	if remaining := cooldown - s.now().Sub(released); remaining > 0 {
		return &NameCooldownError{Name: name, Remaining: remaining} // rule 2
	}
	return nil // cooldown expired; the row stays for the record
}

// releaseNameTx records a name release from a delete or a rename
// (SPEC 7.2). A rerelease of the same name replaces the old row, so the
// cooldown restarts from the newest release.
func (s *Store) releaseNameTx(tx *sql.Tx, name string, previousOwnerID int64) error {
	_, err := tx.Exec(`INSERT INTO released_names (name, previous_owner_id, released_at)
		VALUES (?, ?, ?)
		ON CONFLICT(name) DO UPDATE SET
			previous_owner_id = excluded.previous_owner_id,
			released_at       = excluded.released_at`,
		name, previousOwnerID, fmtTime(s.now()))
	return err
}

// ReleasedName returns the release record of a name, kept even after the
// cooldown expires (SPEC 12).
func (s *Store) ReleasedName(name string) (types.ReleasedName, error) {
	row := s.db.QueryRow(`SELECT name, previous_owner_id, released_at
		FROM released_names WHERE name = ?`, name)
	var (
		r        types.ReleasedName
		released string
	)
	err := row.Scan(&r.Name, &r.PreviousOwnerID, &released)
	if errors.Is(err, sql.ErrNoRows) {
		return types.ReleasedName{}, ErrNotFound
	}
	if err != nil {
		return types.ReleasedName{}, err
	}
	r.ReleasedAt, err = parseTime(released)
	return r, err
}

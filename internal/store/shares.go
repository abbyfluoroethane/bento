package store

import (
	"database/sql"
	"errors"

	"github.com/abbyfluoroethane/bento/internal/types"
)

// AddShare grants userID access to the instance. Shares key on the UUID,
// never on the name (SPEC 7.2, 12). Adding an existing share is a no-op.
func (s *Store) AddShare(instanceUUID string, userID int64) error {
	_, err := s.db.Exec(`INSERT INTO shares (instance_uuid, user_id, created_at)
		VALUES (?, ?, ?)
		ON CONFLICT(instance_uuid, user_id) DO NOTHING`,
		instanceUUID, userID, fmtTime(s.now()))
	return err
}

// RemoveShare revokes a share. Removing a share that does not exist
// returns ErrNotFound.
func (s *Store) RemoveShare(instanceUUID string, userID int64) error {
	res, err := s.db.Exec(`DELETE FROM shares WHERE instance_uuid = ? AND user_id = ?`,
		instanceUUID, userID)
	if err != nil {
		return err
	}
	n, err := res.RowsAffected()
	if err != nil {
		return err
	}
	if n == 0 {
		return ErrNotFound
	}
	return nil
}

// SharesFor lists the shares of one instance.
func (s *Store) SharesFor(instanceUUID string) ([]types.Share, error) {
	rows, err := s.db.Query(`SELECT instance_uuid, user_id, created_at
		FROM shares WHERE instance_uuid = ? ORDER BY user_id`, instanceUUID)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var out []types.Share
	for rows.Next() {
		var (
			share   types.Share
			created string
		)
		if err := rows.Scan(&share.InstanceUUID, &share.UserID, &created); err != nil {
			return nil, err
		}
		if share.CreatedAt, err = parseTime(created); err != nil {
			return nil, err
		}
		out = append(out, share)
	}
	return out, rows.Err()
}

// InstancesSharedWith lists the instances shared with one user, oldest
// first.
func (s *Store) InstancesSharedWith(userID int64) ([]types.Instance, error) {
	return s.listInstances(`WHERE uuid IN (SELECT instance_uuid FROM shares WHERE user_id = ?)
		ORDER BY created_at, uuid`, userID)
}

// HasAccess reports whether userID owns the instance or holds a share on
// it. Authorization runs on every request against this check (SPEC 13).
func (s *Store) HasAccess(instanceUUID string, userID int64) (bool, error) {
	var one int
	err := s.db.QueryRow(`SELECT 1 FROM instances WHERE uuid = ? AND owner_id = ?
		UNION SELECT 1 FROM shares WHERE instance_uuid = ? AND user_id = ?`,
		instanceUUID, userID, instanceUUID, userID).Scan(&one)
	if errors.Is(err, sql.ErrNoRows) {
		return false, nil
	}
	if err != nil {
		return false, err
	}
	return true, nil
}

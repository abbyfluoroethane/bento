package store

import (
	"database/sql"
	"errors"

	"github.com/abbyfluoroethane/bento/internal/types"
)

// AddSSHKey registers a public key for a user and returns its id.
func (s *Store) AddSSHKey(userID int64, publicKey, fingerprint, comment string) (int64, error) {
	res, err := s.db.Exec(`INSERT INTO ssh_keys (user_id, public_key, fingerprint, comment, created_at)
		VALUES (?, ?, ?, ?, ?)`,
		userID, publicKey, fingerprint, comment, fmtTime(s.now()))
	if err != nil {
		return 0, err
	}
	return res.LastInsertId()
}

// SSHKeyByFingerprint is the hot-path lookup the SSH frontend runs on
// every connection; idx_ssh_keys_fingerprint backs it (SPEC 12).
func (s *Store) SSHKeyByFingerprint(fingerprint string) (types.SSHKey, error) {
	row := s.db.QueryRow(`SELECT id, user_id, public_key, fingerprint, comment, created_at
		FROM ssh_keys WHERE fingerprint = ? ORDER BY id LIMIT 1`, fingerprint)
	return scanSSHKey(row)
}

// SSHKeysForUser lists the keys of one user.
func (s *Store) SSHKeysForUser(userID int64) ([]types.SSHKey, error) {
	rows, err := s.db.Query(`SELECT id, user_id, public_key, fingerprint, comment, created_at
		FROM ssh_keys WHERE user_id = ? ORDER BY id`, userID)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var out []types.SSHKey
	for rows.Next() {
		key, err := scanSSHKey(rows)
		if err != nil {
			return nil, err
		}
		out = append(out, key)
	}
	return out, rows.Err()
}

// DeleteSSHKey removes one key of one user. The user scope stops a user
// from deleting another user's key by id.
func (s *Store) DeleteSSHKey(userID, keyID int64) error {
	res, err := s.db.Exec(`DELETE FROM ssh_keys WHERE id = ? AND user_id = ?`, keyID, userID)
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

func scanSSHKey(row rowScanner) (types.SSHKey, error) {
	var (
		key     types.SSHKey
		created string
	)
	err := row.Scan(&key.ID, &key.UserID, &key.PublicKey, &key.Fingerprint,
		&key.Comment, &created)
	if errors.Is(err, sql.ErrNoRows) {
		return types.SSHKey{}, ErrNotFound
	}
	if err != nil {
		return types.SSHKey{}, err
	}
	if key.CreatedAt, err = parseTime(created); err != nil {
		return types.SSHKey{}, err
	}
	return key, nil
}

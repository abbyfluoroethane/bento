package store

import (
	"database/sql"
	"errors"
	"time"

	"github.com/abbyfluoroethane/bento/internal/types"
)

// CreateToken stores a programmatic access token (SPEC 13). Only the hash
// arrives here; the store never sees the token itself. The caller hashes
// the secret and hands out the plain text once.
func (s *Store) CreateToken(userID int64, hash string, expiresAt time.Time) (int64, error) {
	res, err := s.db.Exec(`INSERT INTO tokens (user_id, hash, expires_at)
		VALUES (?, ?, ?)`, userID, hash, fmtTime(expiresAt))
	if err != nil {
		return 0, err
	}
	return res.LastInsertId()
}

// TokenByHash looks a token up by its hash. It returns ErrNotFound for an
// unknown hash and ErrTokenExpired (with the row) for a token past its
// expiry.
func (s *Store) TokenByHash(hash string) (types.Token, error) {
	row := s.db.QueryRow(`SELECT id, user_id, hash, expires_at FROM tokens
		WHERE hash = ?`, hash)
	var (
		token   types.Token
		expires string
	)
	err := row.Scan(&token.ID, &token.UserID, &token.Hash, &expires)
	if errors.Is(err, sql.ErrNoRows) {
		return types.Token{}, ErrNotFound
	}
	if err != nil {
		return types.Token{}, err
	}
	if token.ExpiresAt, err = parseTime(expires); err != nil {
		return types.Token{}, err
	}
	if !s.now().Before(token.ExpiresAt) {
		return token, ErrTokenExpired
	}
	return token, nil
}

// DeleteToken revokes one token of one user.
func (s *Store) DeleteToken(userID, tokenID int64) error {
	res, err := s.db.Exec(`DELETE FROM tokens WHERE id = ? AND user_id = ?`, tokenID, userID)
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

// DeleteTokenByID revokes one token regardless of owner. The dashboard
// revoke path (internal/auth's TokenStore) identifies tokens by row id
// after it has already authorized the caller.
func (s *Store) DeleteTokenByID(tokenID int64) error {
	res, err := s.db.Exec(`DELETE FROM tokens WHERE id = ?`, tokenID)
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

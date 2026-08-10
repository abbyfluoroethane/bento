package auth

import (
	"crypto/sha256"
	"encoding/hex"
	"fmt"
	"net/http"
	"strings"
	"time"

	"github.com/abbyfluoroethane/bento/internal/types"
)

// TokenPrefix starts every Bento API token. The prefix makes a leaked
// token recognizable in logs and secret scanners.
const TokenPrefix = "bento_"

// HashToken returns the hex SHA-256 of a plaintext token. Only this hash
// is stored (SPEC 13).
func HashToken(plaintext string) string {
	sum := sha256.Sum256([]byte(plaintext))
	return hex.EncodeToString(sum[:])
}

// MintToken creates an API token for the user and returns the plaintext
// exactly once. Only the hash reaches the store; the plaintext cannot be
// recovered later. A ttl of zero or less mints a token that does not
// expire.
func (s *Service) MintToken(userID int64, ttl time.Duration) (plaintext string, tok types.Token, err error) {
	plaintext = TokenPrefix + randomToken()
	var expiresAt time.Time
	if ttl > 0 {
		expiresAt = s.now().Add(ttl)
	}
	tok, err = s.tokens.CreateToken(userID, HashToken(plaintext), expiresAt)
	if err != nil {
		return "", types.Token{}, fmt.Errorf("store token: %w", err)
	}
	return plaintext, tok, nil
}

// AuthenticateToken checks a plaintext bearer token and returns its row.
// It returns ErrUnauthenticated for an unknown token and ErrTokenExpired
// for a known token past its expiry.
func (s *Service) AuthenticateToken(plaintext string) (types.Token, error) {
	if plaintext == "" {
		return types.Token{}, ErrUnauthenticated
	}
	tok, ok, err := s.tokens.TokenByHash(HashToken(plaintext))
	if err != nil {
		return types.Token{}, fmt.Errorf("token lookup: %w", err)
	}
	if !ok {
		return types.Token{}, ErrUnauthenticated
	}
	if !tok.ExpiresAt.IsZero() && !tok.ExpiresAt.After(s.now()) {
		return types.Token{}, ErrTokenExpired
	}
	return tok, nil
}

// RevokeToken deletes a token row. The plaintext stops working at once.
func (s *Service) RevokeToken(id int64) error {
	return s.tokens.DeleteToken(id)
}

// BearerToken extracts the token from an Authorization: Bearer header.
// It returns "" when the header is absent or not a bearer credential.
func BearerToken(r *http.Request) string {
	h := r.Header.Get("Authorization")
	const scheme = "bearer "
	if len(h) <= len(scheme) || !strings.EqualFold(h[:len(scheme)], scheme) {
		return ""
	}
	return strings.TrimSpace(h[len(scheme):])
}

package auth

import (
	"context"
	"errors"
	"sync"
	"time"

	"github.com/abbyfluoroethane/bento/internal/types"
)

// fakeClock is an injectable time source.
type fakeClock struct {
	mu  sync.Mutex
	now time.Time
}

func newFakeClock(t time.Time) *fakeClock { return &fakeClock{now: t} }

func (c *fakeClock) Now() time.Time {
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.now
}

func (c *fakeClock) Advance(d time.Duration) {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.now = c.now.Add(d)
}

// fakeUserStore maps OIDC subjects to users.
type fakeUserStore struct {
	users map[string]types.User // by oidc_subject
	err   error
}

func (f *fakeUserStore) UserByOIDCSubject(subject string) (types.User, bool, error) {
	if f.err != nil {
		return types.User{}, false, f.err
	}
	u, ok := f.users[subject]
	return u, ok, nil
}

// fakeAccessStore answers HasAccess from an explicit allow set keyed on
// (instanceUUID, userID), the same shape as owner+shares by UUID.
type fakeAccessStore struct {
	allow map[string]map[int64]bool // instanceUUID -> userID -> ok
	err   error
}

func (f *fakeAccessStore) grant(instanceUUID string, userID int64) {
	if f.allow == nil {
		f.allow = make(map[string]map[int64]bool)
	}
	if f.allow[instanceUUID] == nil {
		f.allow[instanceUUID] = make(map[int64]bool)
	}
	f.allow[instanceUUID][userID] = true
}

func (f *fakeAccessStore) revoke(instanceUUID string, userID int64) {
	delete(f.allow[instanceUUID], userID)
}

func (f *fakeAccessStore) HasAccess(instanceUUID string, userID int64) (bool, error) {
	if f.err != nil {
		return false, f.err
	}
	return f.allow[instanceUUID][userID], nil
}

// fakeTokenStore stores tokens by hash.
type fakeTokenStore struct {
	nextID  int64
	byHash  map[string]types.Token
	created []types.Token
}

func newFakeTokenStore() *fakeTokenStore {
	return &fakeTokenStore{byHash: make(map[string]types.Token)}
}

func (f *fakeTokenStore) CreateToken(userID int64, hash string, expiresAt time.Time) (types.Token, error) {
	f.nextID++
	t := types.Token{ID: f.nextID, UserID: userID, Hash: hash, ExpiresAt: expiresAt}
	f.byHash[hash] = t
	f.created = append(f.created, t)
	return t, nil
}

func (f *fakeTokenStore) TokenByHash(hash string) (types.Token, bool, error) {
	t, ok := f.byHash[hash]
	return t, ok, nil
}

func (f *fakeTokenStore) DeleteToken(id int64) error {
	for h, t := range f.byHash {
		if t.ID == id {
			delete(f.byHash, h)
			return nil
		}
	}
	return errors.New("no such token")
}

// fakeExchanger records the state and nonce given to AuthCodeURL and
// redeems one known code for one raw ID token.
type fakeExchanger struct {
	authURL     string
	lastState   string
	lastNonce   string
	code        string
	rawIDToken  string
	exchangeErr error
}

func (f *fakeExchanger) AuthCodeURL(state, nonce string) string {
	f.lastState = state
	f.lastNonce = nonce
	return f.authURL + "?state=" + state
}

func (f *fakeExchanger) Exchange(_ context.Context, code string) (string, error) {
	if f.exchangeErr != nil {
		return "", f.exchangeErr
	}
	if code != f.code {
		return "", errors.New("unknown code")
	}
	return f.rawIDToken, nil
}

// fakeVerifier maps raw ID tokens to claims.
type fakeVerifier struct {
	claims map[string]Claims // by raw token
}

func (f *fakeVerifier) Verify(_ context.Context, raw string) (Claims, error) {
	c, ok := f.claims[raw]
	if !ok {
		return Claims{}, errors.New("bad token")
	}
	return c, nil
}

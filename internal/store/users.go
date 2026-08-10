package store

import (
	"database/sql"
	"errors"
	"fmt"
	"net/netip"

	"github.com/abbyfluoroethane/bento/internal/types"
)

// ErrSubnetsExhausted is returned by RegisterUser when every /24 in the
// private range is allocated.
var ErrSubnetsExhausted = errors.New("store: no free /24 left in the private range")

// RegisterUser creates a user and allocates the next free /24 out of
// privateRange (SPEC 6.2, 13). The scan and the insert run in one
// transaction, and the unique index on users.subnet backs the allocation.
// oidcSubject may be empty; it is stored as NULL then.
func (s *Store) RegisterUser(name, email, oidcSubject string, privateRange netip.Prefix) (types.User, error) {
	var user types.User
	err := s.inTx(func(tx *sql.Tx) error {
		rows, err := tx.Query(`SELECT subnet FROM users`)
		if err != nil {
			return err
		}
		defer rows.Close()
		used := make(map[netip.Addr]bool)
		for rows.Next() {
			var subnet string
			if err := rows.Scan(&subnet); err != nil {
				return err
			}
			prefix, err := netip.ParsePrefix(subnet)
			if err != nil {
				return fmt.Errorf("store: user subnet %q: %w", subnet, err)
			}
			used[prefix.Masked().Addr()] = true
		}
		if err := rows.Err(); err != nil {
			return err
		}
		subnet, err := nextFreeSubnet(privateRange, used)
		if err != nil {
			return err
		}
		now := s.now()
		res, err := tx.Exec(`INSERT INTO users (name, email, oidc_subject, subnet, created_at)
			VALUES (?, ?, ?, ?, ?)`,
			name, email, nullString(oidcSubject), subnet.String(), fmtTime(now))
		if err != nil {
			return err
		}
		id, err := res.LastInsertId()
		if err != nil {
			return err
		}
		user = types.User{
			ID:          id,
			Name:        name,
			Email:       email,
			OIDCSubject: oidcSubject,
			Subnet:      subnet.String(),
			CreatedAt:   now.UTC(),
		}
		return nil
	})
	return user, err
}

// nextFreeSubnet returns the lowest /24 inside privateRange whose base
// address is not in used.
func nextFreeSubnet(privateRange netip.Prefix, used map[netip.Addr]bool) (netip.Prefix, error) {
	if !privateRange.Addr().Is4() {
		return netip.Prefix{}, fmt.Errorf("store: private range %s is not IPv4", privateRange)
	}
	if privateRange.Bits() > 24 {
		return netip.Prefix{}, fmt.Errorf("store: private range %s is narrower than /24", privateRange)
	}
	base := privateRange.Masked().Addr().As4()
	baseVal := uint32(base[0])<<24 | uint32(base[1])<<16 | uint32(base[2])<<8 | uint32(base[3])
	count := 1 << (24 - privateRange.Bits())
	for i := 0; i < count; i++ {
		v := baseVal + uint32(i)<<8
		addr := netip.AddrFrom4([4]byte{byte(v >> 24), byte(v >> 16), byte(v >> 8), byte(v)})
		if !used[addr] {
			return netip.PrefixFrom(addr, 24), nil
		}
	}
	return netip.Prefix{}, ErrSubnetsExhausted
}

// UserByID returns one user by primary key.
func (s *Store) UserByID(id int64) (types.User, error) {
	return s.userBy(`id = ?`, id)
}

// UserByName returns one user by account name.
func (s *Store) UserByName(name string) (types.User, error) {
	return s.userBy(`name = ?`, name)
}

// UserByOIDCSubject returns one user by OIDC subject (SPEC 13).
func (s *Store) UserByOIDCSubject(subject string) (types.User, error) {
	return s.userBy(`oidc_subject = ?`, subject)
}

func (s *Store) userBy(where string, arg any) (types.User, error) {
	row := s.db.QueryRow(
		`SELECT id, name, email, oidc_subject, subnet, created_at FROM users WHERE `+where, arg)
	var (
		u       types.User
		subject sql.NullString
		created string
	)
	err := row.Scan(&u.ID, &u.Name, &u.Email, &subject, &u.Subnet, &created)
	if errors.Is(err, sql.ErrNoRows) {
		return types.User{}, ErrNotFound
	}
	if err != nil {
		return types.User{}, err
	}
	u.OIDCSubject = subject.String
	if u.CreatedAt, err = parseTime(created); err != nil {
		return types.User{}, err
	}
	return u, nil
}

// Users returns every user ordered by name. The control plane walks
// this list at startup to re-ensure the per-user libvirt networks and to
// build the firewall ruleset (SPEC 6.2, 6.3).
func (s *Store) Users() ([]types.User, error) {
	rows, err := s.db.Query(
		`SELECT id, name, email, oidc_subject, subnet, created_at FROM users ORDER BY name`)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var users []types.User
	for rows.Next() {
		var (
			u       types.User
			subject sql.NullString
			created string
		)
		if err := rows.Scan(&u.ID, &u.Name, &u.Email, &subject, &u.Subnet, &created); err != nil {
			return nil, err
		}
		u.OIDCSubject = subject.String
		if u.CreatedAt, err = parseTime(created); err != nil {
			return nil, err
		}
		users = append(users, u)
	}
	return users, rows.Err()
}

// SetQuota inserts or replaces the four limits of a user (SPEC 6.1).
func (s *Store) SetQuota(q types.Quota) error {
	_, err := s.db.Exec(`INSERT INTO quotas (user_id, max_instances, max_vcpu, max_memory, max_disk)
		VALUES (?, ?, ?, ?, ?)
		ON CONFLICT(user_id) DO UPDATE SET
			max_instances = excluded.max_instances,
			max_vcpu      = excluded.max_vcpu,
			max_memory    = excluded.max_memory,
			max_disk      = excluded.max_disk`,
		q.UserID, q.MaxInstances, q.MaxVCPU, q.MaxMemoryMiB, q.MaxDiskGiB)
	return err
}

// QuotaFor returns the limits of a user, or ErrNotFound when the operator
// has not set any.
func (s *Store) QuotaFor(userID int64) (types.Quota, error) {
	row := s.db.QueryRow(`SELECT user_id, max_instances, max_vcpu, max_memory, max_disk
		FROM quotas WHERE user_id = ?`, userID)
	var q types.Quota
	err := row.Scan(&q.UserID, &q.MaxInstances, &q.MaxVCPU, &q.MaxMemoryMiB, &q.MaxDiskGiB)
	if errors.Is(err, sql.ErrNoRows) {
		return types.Quota{}, ErrNotFound
	}
	return q, err
}

// Usage is the current quota consumption of a user, for `ls` and the
// dashboard (SPEC 6.1).
type Usage struct {
	Instances int
	VCPU      int64
	MemoryMiB int64
	DiskGiB   int64
}

// UsageFor sums the instances of a user against the four limits.
func (s *Store) UsageFor(userID int64) (Usage, error) {
	var u Usage
	err := s.db.QueryRow(`SELECT COUNT(*), COALESCE(SUM(vcpu), 0),
		COALESCE(SUM(memory), 0), COALESCE(SUM(disk), 0)
		FROM instances WHERE owner_id = ?`, userID).
		Scan(&u.Instances, &u.VCPU, &u.MemoryMiB, &u.DiskGiB)
	return u, err
}

func nullString(s string) sql.NullString {
	return sql.NullString{String: s, Valid: s != ""}
}

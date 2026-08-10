package store

import (
	"database/sql"
	"errors"

	"github.com/abbyfluoroethane/bento/internal/types"
)

// EnsureHost inserts the host row if absent and returns it. Version 1 runs
// one host, but the column and the row exist from the start (SPEC 12, 17).
// An existing host keeps its id; the URI is updated if it changed.
func (s *Store) EnsureHost(name, libvirtURI string) (types.Host, error) {
	var host types.Host
	err := s.inTx(func(tx *sql.Tx) error {
		_, err := tx.Exec(`INSERT INTO hosts (name, libvirt_uri, created_at)
			VALUES (?, ?, ?)
			ON CONFLICT(name) DO UPDATE SET libvirt_uri = excluded.libvirt_uri`,
			name, libvirtURI, fmtTime(s.now()))
		if err != nil {
			return err
		}
		var created string
		err = tx.QueryRow(`SELECT id, name, libvirt_uri, created_at FROM hosts
			WHERE name = ?`, name).
			Scan(&host.ID, &host.Name, &host.LibvirtURI, &created)
		if err != nil {
			return err
		}
		host.CreatedAt, err = parseTime(created)
		return err
	})
	return host, err
}

// Host returns one host by id.
func (s *Store) Host(id int64) (types.Host, error) {
	row := s.db.QueryRow(`SELECT id, name, libvirt_uri, created_at FROM hosts
		WHERE id = ?`, id)
	var (
		host    types.Host
		created string
	)
	err := row.Scan(&host.ID, &host.Name, &host.LibvirtURI, &created)
	if errors.Is(err, sql.ErrNoRows) {
		return types.Host{}, ErrNotFound
	}
	if err != nil {
		return types.Host{}, err
	}
	host.CreatedAt, err = parseTime(created)
	return host, err
}

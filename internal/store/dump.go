package store

import (
	"fmt"
	"os"
)

// DumpDB writes a consistent snapshot of the database to destPath using
// VACUUM INTO (SPEC 12.1). The database runs with a write-ahead log, so a
// raw file copy is unsafe and is never done here. destPath must not exist;
// SQLite refuses to vacuum into an existing file, and this check turns
// that into a clear error first.
func (s *Store) DumpDB(destPath string) error {
	if _, err := os.Stat(destPath); err == nil {
		return fmt.Errorf("store: dump destination %s already exists", destPath)
	} else if !os.IsNotExist(err) {
		return fmt.Errorf("store: dump destination %s: %w", destPath, err)
	}
	if _, err := s.db.Exec(`VACUUM INTO ?`, destPath); err != nil {
		return fmt.Errorf("store: vacuum into %s: %w", destPath, err)
	}
	return nil
}

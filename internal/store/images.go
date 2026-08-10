package store

import (
	"database/sql"
	"errors"

	"github.com/abbyfluoroethane/bento/internal/types"
)

// UpsertImage inserts or updates an allowlist entry (SPEC 5.1). The
// current checksum is managed by SetCurrentChecksum, not here, so a config
// reload cannot roll an image back.
func (s *Store) UpsertImage(img types.Image) error {
	_, err := s.db.Exec(`INSERT INTO images (name, url, pinned_checksum)
		VALUES (?, ?, ?)
		ON CONFLICT(name) DO UPDATE SET
			url             = excluded.url,
			pinned_checksum = excluded.pinned_checksum`,
		img.Name, img.URL, nullString(img.PinnedChecksum))
	return err
}

// Image returns one allowlist entry by name.
func (s *Store) Image(name string) (types.Image, error) {
	row := s.db.QueryRow(`SELECT name, url, pinned_checksum, current_checksum
		FROM images WHERE name = ?`, name)
	img, err := scanImage(row)
	if errors.Is(err, sql.ErrNoRows) {
		return types.Image{}, ErrNotFound
	}
	return img, err
}

// Images lists the allowlist, for the `images` command (SPEC 15).
func (s *Store) Images() ([]types.Image, error) {
	rows, err := s.db.Query(`SELECT name, url, pinned_checksum, current_checksum
		FROM images ORDER BY name`)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var out []types.Image
	for rows.Next() {
		img, err := scanImage(rows)
		if err != nil {
			return nil, err
		}
		out = append(out, img)
	}
	return out, rows.Err()
}

// AddImageVersion records one downloaded file at its content-addressed
// path (SPEC 5.1).
func (s *Store) AddImageVersion(v types.ImageVersion) error {
	_, err := s.db.Exec(`INSERT INTO image_versions (checksum, image_name, path, size, fetched_at)
		VALUES (?, ?, ?, ?, ?)`,
		v.Checksum, v.ImageName, v.Path, v.Size, fmtTime(v.FetchedAt))
	return err
}

// SetCurrentChecksum points an image at a fetched version.
func (s *Store) SetCurrentChecksum(imageName, checksum string) error {
	res, err := s.db.Exec(`UPDATE images SET current_checksum = ? WHERE name = ?`,
		checksum, imageName)
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

// ImageVersions lists the fetched versions of one image, newest first.
func (s *Store) ImageVersions(imageName string) ([]types.ImageVersion, error) {
	return s.listImageVersions(`WHERE image_name = ? ORDER BY fetched_at DESC, checksum`, imageName)
}

// UnusedImageVersions lists versions that no instance was built from
// (base_checksum, SPEC 5.1) and that no image points at as current. These
// are the versions the image store may delete.
func (s *Store) UnusedImageVersions() ([]types.ImageVersion, error) {
	return s.listImageVersions(`WHERE checksum NOT IN (SELECT base_checksum FROM instances)
		AND checksum NOT IN (SELECT current_checksum FROM images WHERE current_checksum IS NOT NULL)
		ORDER BY fetched_at, checksum`)
}

// DeleteImageVersion removes the record of one fetched file.
func (s *Store) DeleteImageVersion(checksum string) error {
	res, err := s.db.Exec(`DELETE FROM image_versions WHERE checksum = ?`, checksum)
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

func (s *Store) listImageVersions(tail string, args ...any) ([]types.ImageVersion, error) {
	rows, err := s.db.Query(`SELECT checksum, image_name, path, size, fetched_at
		FROM image_versions `+tail, args...)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var out []types.ImageVersion
	for rows.Next() {
		var (
			v       types.ImageVersion
			fetched string
		)
		if err := rows.Scan(&v.Checksum, &v.ImageName, &v.Path, &v.Size, &fetched); err != nil {
			return nil, err
		}
		if v.FetchedAt, err = parseTime(fetched); err != nil {
			return nil, err
		}
		out = append(out, v)
	}
	return out, rows.Err()
}

func scanImage(row rowScanner) (types.Image, error) {
	var (
		img             types.Image
		pinned, current sql.NullString
	)
	if err := row.Scan(&img.Name, &img.URL, &pinned, &current); err != nil {
		return types.Image{}, err
	}
	img.PinnedChecksum = pinned.String
	img.CurrentChecksum = current.String
	return img, nil
}

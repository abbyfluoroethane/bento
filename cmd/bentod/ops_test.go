package main

import (
	"fmt"
	"os"
	"path/filepath"
	"testing"
)

// writeTestConfig writes a minimal valid config rooted in a temp dir
// and returns its path.
func writeTestConfig(t *testing.T) (cfgPath, dir string) {
	t.Helper()
	dir = t.TempDir()
	for _, sub := range []string{"images", "storage"} {
		if err := os.MkdirAll(filepath.Join(dir, sub), 0o755); err != nil {
			t.Fatal(err)
		}
	}
	cfgPath = filepath.Join(dir, "bento.toml")
	body := fmt.Sprintf(`base_domain = "bento.example.org"
db_path = %q
image_dir = %q
storage_dir = %q
key_dir = %q

[[images]]
name = "debian-13"
url = "https://example.test/debian-13.qcow2"
`,
		filepath.Join(dir, "bento.db"),
		filepath.Join(dir, "images"),
		filepath.Join(dir, "storage"),
		filepath.Join(dir, "keys"),
	)
	if err := os.WriteFile(cfgPath, []byte(body), 0o644); err != nil {
		t.Fatal(err)
	}
	return cfgPath, dir
}

func TestDumpDBCommand(t *testing.T) {
	cfgPath, dir := writeTestConfig(t)
	dest := filepath.Join(dir, "backup.db")
	if err := runDumpDB(cfgPath, []string{dest}); err != nil {
		t.Fatalf("dump-db: %v", err)
	}
	info, err := os.Stat(dest)
	if err != nil {
		t.Fatalf("backup file: %v", err)
	}
	if info.Size() == 0 {
		t.Error("backup file is empty")
	}
	// A second dump refuses to overwrite the destination.
	if err := runDumpDB(cfgPath, []string{dest}); err == nil {
		t.Error("second dump-db to the same path: want error")
	}
}

func TestImagesCommand(t *testing.T) {
	cfgPath, _ := writeTestConfig(t)
	// Syncs the allowlist and prints the (unfetched) image without error.
	if err := runImages(cfgPath, nil); err != nil {
		t.Fatalf("images: %v", err)
	}
}

package hypervisor

import (
	"errors"
	"io/fs"
	"strings"
	"testing"
	"time"
)

// fakeFileInfo is a minimal fs.FileInfo for the Stat fake.
type fakeFileInfo struct {
	name string
	dir  bool
}

func (f fakeFileInfo) Name() string       { return f.name }
func (f fakeFileInfo) Size() int64        { return 0 }
func (f fakeFileInfo) Mode() fs.FileMode  { return 0o644 }
func (f fakeFileInfo) ModTime() time.Time { return time.Time{} }
func (f fakeFileInfo) IsDir() bool        { return f.dir }
func (f fakeFileInfo) Sys() any           { return nil }

// healthyDeps returns deps describing a host that passes every check.
func healthyDeps() CheckDeps {
	return CheckDeps{
		Stat: func(path string) (fs.FileInfo, error) {
			switch path {
			case "/dev/kvm":
				return fakeFileInfo{name: "kvm"}, nil
			case "/img", "/store":
				return fakeFileInfo{name: path, dir: true}, nil
			}
			return nil, fs.ErrNotExist
		},
		ReadFile: func(path string) ([]byte, error) {
			switch path {
			case "/sys/kernel/mm/ksm/run":
				return []byte("1\n"), nil
			case "/sys/module/kvm_intel/parameters/nested":
				return []byte("Y\n"), nil
			}
			return nil, fs.ErrNotExist
		},
		LookPath:    func(name string) (string, error) { return "/usr/bin/" + name, nil },
		WriteProbe:  func(string) error { return nil },
		PingLibvirt: func(string) error { return nil },
	}
}

func healthyConfig() CheckConfig {
	return CheckConfig{ImageDir: "/img", StorageDir: "/store"}
}

func result(t *testing.T, r CheckReport, name string) CheckResult {
	t.Helper()
	for _, res := range r.Results {
		if res.Name == name {
			return res
		}
	}
	t.Fatalf("no check named %q in %+v", name, r.Results)
	return CheckResult{}
}

func TestCheckHealthyHost(t *testing.T) {
	r := Check(healthyConfig(), healthyDeps())
	if !r.OK() {
		t.Errorf("healthy host must pass: %+v", r.Results)
	}
	if len(r.Warnings()) != 0 {
		t.Errorf("healthy host must have no warnings: %+v", r.Warnings())
	}
	for _, name := range []string{
		"kvm device", "libvirtd socket", "qemu-img binary", "xorriso binary",
		"image directory", "image directory writable",
		"storage directory", "storage directory writable", "ksm run",
	} {
		if res := result(t, r, name); !res.OK {
			t.Errorf("%s failed: %s", name, res.Detail)
		}
	}
}

func TestCheckFatalFailures(t *testing.T) {
	tests := []struct {
		name      string
		breakDeps func(*CheckDeps)
		failCheck string
	}{
		{
			name: "missing kvm device",
			breakDeps: func(d *CheckDeps) {
				stat := d.Stat
				d.Stat = func(path string) (fs.FileInfo, error) {
					if path == "/dev/kvm" {
						return nil, fs.ErrNotExist
					}
					return stat(path)
				}
			},
			failCheck: "kvm device",
		},
		{
			name: "libvirtd not answering",
			breakDeps: func(d *CheckDeps) {
				d.PingLibvirt = func(string) error { return errors.New("connection refused") }
			},
			failCheck: "libvirtd socket",
		},
		{
			name: "xorriso missing from PATH",
			breakDeps: func(d *CheckDeps) {
				d.LookPath = func(name string) (string, error) {
					if name == "xorriso" {
						return "", errors.New("not found")
					}
					return "/usr/bin/" + name, nil
				}
			},
			failCheck: "xorriso binary",
		},
		{
			name: "storage directory missing",
			breakDeps: func(d *CheckDeps) {
				stat := d.Stat
				d.Stat = func(path string) (fs.FileInfo, error) {
					if path == "/store" {
						return nil, fs.ErrNotExist
					}
					return stat(path)
				}
			},
			failCheck: "storage directory",
		},
		{
			name: "image directory not writable",
			breakDeps: func(d *CheckDeps) {
				d.WriteProbe = func(dir string) error {
					if dir == "/img" {
						return errors.New("permission denied")
					}
					return nil
				}
			},
			failCheck: "image directory writable",
		},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			deps := healthyDeps()
			tt.breakDeps(&deps)
			r := Check(healthyConfig(), deps)
			if r.OK() {
				t.Fatal("report must not be OK")
			}
			res := result(t, r, tt.failCheck)
			if res.OK {
				t.Errorf("%s should have failed", tt.failCheck)
			}
			if !res.Fatal {
				t.Errorf("%s must be fatal (SPEC 4.2)", tt.failCheck)
			}
		})
	}
}

func TestCheckMissingDirSkipsWriteProbe(t *testing.T) {
	deps := healthyDeps()
	stat := deps.Stat
	deps.Stat = func(path string) (fs.FileInfo, error) {
		if path == "/img" {
			return nil, fs.ErrNotExist
		}
		return stat(path)
	}
	probed := false
	deps.WriteProbe = func(dir string) error {
		if dir == "/img" {
			probed = true
		}
		return nil
	}
	Check(healthyConfig(), deps)
	if probed {
		t.Error("write probe must be skipped for a missing directory")
	}
}

func TestCheckKSMOffWarns(t *testing.T) {
	deps := healthyDeps()
	deps.ReadFile = func(path string) ([]byte, error) {
		if path == "/sys/kernel/mm/ksm/run" {
			return []byte("0\n"), nil
		}
		return nil, fs.ErrNotExist
	}
	r := Check(healthyConfig(), deps)
	if !r.OK() {
		t.Error("ksm off is a warning, not fatal (SPEC 4.2 item 6)")
	}
	warnings := r.Warnings()
	if len(warnings) != 1 || warnings[0].Name != "ksm run" {
		t.Errorf("warnings = %+v, want one ksm warning", warnings)
	}
}

func TestCheckNested(t *testing.T) {
	tests := []struct {
		name        string
		wanted      bool
		nestedValue string
		readErr     bool
		wantChecked bool
		wantWarn    bool
	}{
		{"not wanted, not checked", false, "0", false, false, false},
		{"wanted and on", true, "Y", false, true, false},
		{"wanted and numeric on", true, "1", false, true, false},
		{"wanted but off", true, "0", false, true, true},
		{"wanted but no module", true, "", true, true, true},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			deps := healthyDeps()
			deps.ReadFile = func(path string) ([]byte, error) {
				if path == "/sys/kernel/mm/ksm/run" {
					return []byte("1"), nil
				}
				if strings.Contains(path, "nested") {
					if tt.readErr {
						return nil, fs.ErrNotExist
					}
					if strings.Contains(path, "kvm_intel") {
						return []byte(tt.nestedValue + "\n"), nil
					}
					return nil, fs.ErrNotExist
				}
				return nil, fs.ErrNotExist
			}
			cfg := healthyConfig()
			cfg.NestedWanted = tt.wanted
			r := Check(cfg, deps)
			var found *CheckResult
			for i := range r.Results {
				if r.Results[i].Name == "nested virtualization" {
					found = &r.Results[i]
				}
			}
			if (found != nil) != tt.wantChecked {
				t.Fatalf("checked = %v, want %v", found != nil, tt.wantChecked)
			}
			if found == nil {
				return
			}
			if found.Fatal {
				t.Error("nested check must warn, not refuse startup (SPEC 4.2 item 7)")
			}
			if found.OK == tt.wantWarn {
				t.Errorf("OK = %v, wantWarn %v", found.OK, tt.wantWarn)
			}
			if tt.wantWarn && !strings.Contains(found.Detail, "nested=1") {
				t.Errorf("warning must give the module parameter, got %q (SPEC 5.5)", found.Detail)
			}
		})
	}
}

func TestNestedEnabled(t *testing.T) {
	tests := []struct {
		name     string
		files    map[string]string
		wantOn   bool
		wantPath string
	}{
		{"intel on Y", map[string]string{"/sys/module/kvm_intel/parameters/nested": "Y\n"}, true, "/sys/module/kvm_intel/parameters/nested"},
		{"amd on 1", map[string]string{"/sys/module/kvm_amd/parameters/nested": "1"}, true, "/sys/module/kvm_amd/parameters/nested"},
		{"intel off", map[string]string{"/sys/module/kvm_intel/parameters/nested": "0\n"}, false, "/sys/module/kvm_intel/parameters/nested"},
		{"off N", map[string]string{"/sys/module/kvm_amd/parameters/nested": "N"}, false, "/sys/module/kvm_amd/parameters/nested"},
		{"no module", map[string]string{}, false, ""},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			deps := CheckDeps{ReadFile: func(path string) ([]byte, error) {
				if v, ok := tt.files[path]; ok {
					return []byte(v), nil
				}
				return nil, fs.ErrNotExist
			}}
			on, path := NestedEnabled(CheckConfig{}, deps)
			if on != tt.wantOn || path != tt.wantPath {
				t.Errorf("NestedEnabled = (%v, %q), want (%v, %q)", on, path, tt.wantOn, tt.wantPath)
			}
		})
	}
}

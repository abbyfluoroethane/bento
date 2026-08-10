package hypervisor

import (
	"fmt"
	"io/fs"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"time"
)

// CheckConfig names the host paths the requirement checks probe
// (SPEC 4.2). Zero-value fields fall back to the defaults below.
type CheckConfig struct {
	KVMPath     string // default /dev/kvm
	SocketPath  string // default DefaultSocketPath
	ImageDir    string // required
	StorageDir  string // required
	KSMRunPath  string // default /sys/kernel/mm/ksm/run
	NestedPaths []string
	// NestedWanted marks that at least one instance requests nested
	// virtualization; the nested check only warns in that case
	// (SPEC 4.2 item 7).
	NestedWanted bool
}

func (c CheckConfig) withDefaults() CheckConfig {
	if c.KVMPath == "" {
		c.KVMPath = "/dev/kvm"
	}
	if c.SocketPath == "" {
		c.SocketPath = DefaultSocketPath
	}
	if c.KSMRunPath == "" {
		c.KSMRunPath = "/sys/kernel/mm/ksm/run"
	}
	if len(c.NestedPaths) == 0 {
		c.NestedPaths = []string{
			"/sys/module/kvm_intel/parameters/nested",
			"/sys/module/kvm_amd/parameters/nested",
		}
	}
	return c
}

// CheckDeps injects the host-touching operations so the checks are pure
// functions that unit test on any platform.
type CheckDeps struct {
	Stat       func(path string) (fs.FileInfo, error)
	ReadFile   func(path string) ([]byte, error)
	LookPath   func(name string) (string, error)
	WriteProbe func(dir string) error
	// PingLibvirt verifies that libvirtd answers on the socket.
	PingLibvirt func(socketPath string) error
}

// DefaultCheckDeps returns deps backed by the real host.
func DefaultCheckDeps() CheckDeps {
	return CheckDeps{
		Stat:     os.Stat,
		ReadFile: os.ReadFile,
		LookPath: exec.LookPath,
		WriteProbe: func(dir string) error {
			probe := filepath.Join(dir, ".bento-write-probe")
			if err := os.WriteFile(probe, nil, 0o600); err != nil {
				return err
			}
			return os.Remove(probe)
		},
		PingLibvirt: func(socketPath string) error {
			conn, err := net.DialTimeout("unix", socketPath, 2*time.Second)
			if err != nil {
				return err
			}
			return conn.Close()
		},
	}
}

// CheckResult is one line of the host requirement report.
type CheckResult struct {
	Name   string
	OK     bool
	Fatal  bool // a failed fatal check refuses startup (SPEC 4.2)
	Detail string
}

// CheckReport is the outcome of every host requirement check.
type CheckReport struct {
	Results []CheckResult
}

// OK reports whether no fatal check failed. Warnings do not block
// startup.
func (r CheckReport) OK() bool {
	for _, res := range r.Results {
		if res.Fatal && !res.OK {
			return false
		}
	}
	return true
}

// Warnings returns the non-fatal checks that failed.
func (r CheckReport) Warnings() []CheckResult {
	var out []CheckResult
	for _, res := range r.Results {
		if !res.Fatal && !res.OK {
			out = append(out, res)
		}
	}
	return out
}

// Check runs the host requirement checks of SPEC 4.2. Items 1 through 5
// are fatal; the KSM and nested checks warn.
func Check(cfg CheckConfig, deps CheckDeps) CheckReport {
	cfg = cfg.withDefaults()
	var r CheckReport
	add := func(name string, fatal bool, err error, okDetail string) {
		res := CheckResult{Name: name, OK: err == nil, Fatal: fatal, Detail: okDetail}
		if err != nil {
			res.Detail = err.Error()
		}
		r.Results = append(r.Results, res)
	}

	// 1. /dev/kvm exists.
	_, err := deps.Stat(cfg.KVMPath)
	add("kvm device", true, err, cfg.KVMPath)

	// 2. libvirtd answers on the local socket.
	add("libvirtd socket", true, deps.PingLibvirt(cfg.SocketPath), cfg.SocketPath)

	// 3. Required binaries on PATH.
	for _, bin := range []string{"qemu-img", "xorriso"} {
		path, err := deps.LookPath(bin)
		add(bin+" binary", true, err, path)
	}

	// 4 and 5. Directories exist and are writable.
	for _, dir := range []struct{ name, path string }{
		{"image directory", cfg.ImageDir},
		{"storage directory", cfg.StorageDir},
	} {
		info, err := deps.Stat(dir.path)
		if err == nil && !info.IsDir() {
			err = fmt.Errorf("%s is not a directory", dir.path)
		}
		add(dir.name, true, err, dir.path)
		if err != nil {
			// Skip the write probe when the directory is missing;
			// the existence failure already refuses startup.
			continue
		}
		add(dir.name+" writable", true, deps.WriteProbe(dir.path), dir.path)
	}

	// 6. KSM run value (warning, SPEC 5.4).
	r.Results = append(r.Results, checkKSM(cfg, deps))

	// 7. Nested module parameter (warning, only when wanted, SPEC 5.5).
	if cfg.NestedWanted {
		r.Results = append(r.Results, checkNested(cfg, deps))
	}
	return r
}

func checkKSM(cfg CheckConfig, deps CheckDeps) CheckResult {
	res := CheckResult{Name: "ksm run", Fatal: false}
	raw, err := deps.ReadFile(cfg.KSMRunPath)
	if err != nil {
		res.Detail = fmt.Sprintf("read %s: %v", cfg.KSMRunPath, err)
		return res
	}
	value := strings.TrimSpace(string(raw))
	if value == "0" {
		res.Detail = fmt.Sprintf("%s is 0; set it to 1 or run ksmtuned (SPEC 5.4)", cfg.KSMRunPath)
		return res
	}
	res.OK = true
	res.Detail = cfg.KSMRunPath + " = " + value
	return res
}

// NestedEnabled reports whether the KVM module has nesting on, reading
// the first nested parameter file that exists. The lifecycle package
// uses it to reject a new or resize with nested=true (SPEC 5.5).
func NestedEnabled(cfg CheckConfig, deps CheckDeps) (bool, string) {
	cfg = cfg.withDefaults()
	for _, path := range cfg.NestedPaths {
		raw, err := deps.ReadFile(path)
		if err != nil {
			continue
		}
		value := strings.TrimSpace(string(raw))
		on := value == "1" || strings.EqualFold(value, "Y")
		return on, path
	}
	return false, ""
}

func checkNested(cfg CheckConfig, deps CheckDeps) CheckResult {
	res := CheckResult{Name: "nested virtualization", Fatal: false}
	on, path := NestedEnabled(cfg, deps)
	if path == "" {
		res.Detail = "no kvm nested parameter found; load kvm_intel.nested=1 or kvm_amd.nested=1"
		return res
	}
	if !on {
		res.Detail = fmt.Sprintf("%s is off; instances request nested virtualization (set kvm_intel.nested=1 or kvm_amd.nested=1)", path)
		return res
	}
	res.OK = true
	res.Detail = path + " is on"
	return res
}

// Package config loads the operator configuration for bentod from a TOML
// file. Every setting named in SPEC.md that the operator controls lives
// here: base domain, libvirt URI, directories, database path, overcommit
// ratio, name cooldown, reboot-restore batch size, the private address
// range for user /24s, ACME/DNS settings, OIDC settings, and listen
// addresses.
package config

import (
	"fmt"
	"net"
	"net/netip"
	"os"
	"strconv"
	"time"

	"github.com/BurntSushi/toml"
)

// Defaults from SPEC.md.
const (
	DefaultLibvirtURI       = "qemu:///system"
	DefaultImageDir         = "/var/lib/bento/images"
	DefaultStorageDir       = "/var/lib/bento/storage"
	DefaultDBPath           = "/var/lib/bento/bento.db"
	DefaultOvercommitRatio  = 1.0             // SPEC 5.3: no overcommit.
	DefaultNameCooldown     = 24 * time.Hour  // SPEC 7.2.
	DefaultRestoreBatchSize = 4               // SPEC 11.2.
	DefaultPrivateRange     = "10.100.0.0/16" // Carved into user /24s, SPEC 6.2.
	// DefaultListenHTTP is the control plane behind the proxy. It sits
	// outside DefaultProxyPortMin..Max on purpose: the proxy binds
	// every port of that range, on all interfaces by default, so a
	// control plane inside it would take a port from the proxy and one
	// of the two would fail to start.
	DefaultListenHTTP   = "127.0.0.1:10080"
	DefaultListenHTTPS  = ":443"                // HTTP proxy, SPEC 4.
	DefaultListenSSH    = ":22"                 // SSH frontend, SPEC 4.
	DefaultProxyPortMin = 3000                  // SPEC 9.1.
	DefaultProxyPortMax = 9999                  // SPEC 9.1.
	DefaultKeyDir       = "/var/lib/bento/keys" // SSH host and frontend keys.
	DefaultVCPU         = 2                     // `new` without --cpu.
	DefaultMemoryMiB    = int64(2048)           // `new` without --memory.
	DefaultDiskGiB      = int64(20)             // `new` without --disk.
)

// Duration wraps time.Duration so TOML values such as "24h" parse.
type Duration time.Duration

// UnmarshalText implements encoding.TextUnmarshaler.
func (d *Duration) UnmarshalText(text []byte) error {
	v, err := time.ParseDuration(string(text))
	if err != nil {
		return err
	}
	*d = Duration(v)
	return nil
}

// Std returns the value as a time.Duration.
func (d Duration) Std() time.Duration { return time.Duration(d) }

// Config is the full operator configuration.
type Config struct {
	// BaseDomain is the domain the operator owns, e.g. "bento.foid.space".
	BaseDomain string `toml:"base_domain"`
	// LibvirtURI is the libvirt connection URI.
	LibvirtURI string `toml:"libvirt_uri"`
	// ImageDir holds content-addressed image versions (SPEC 5.1).
	ImageDir string `toml:"image_dir"`
	// StorageDir holds instance overlay disks.
	StorageDir string `toml:"storage_dir"`
	// DBPath is the SQLite database path (SPEC 12.1).
	DBPath string `toml:"db_path"`
	// OvercommitRatio is the memory overcommit ratio (SPEC 5.3).
	OvercommitRatio float64 `toml:"overcommit_ratio"`
	// NameCooldown is how long a released name is held (SPEC 7.2).
	NameCooldown Duration `toml:"name_cooldown"`
	// RestoreBatchSize is the reboot-restore batch size (SPEC 11.2).
	RestoreBatchSize int `toml:"restore_batch_size"`
	// PrivateRange is the private range carved into user /24s (SPEC 6.2).
	PrivateRange string `toml:"private_range"`
	// KeyDir holds the SSH frontend host key and the key the frontend
	// uses toward the guests (SPEC 10). Both are created on first use.
	KeyDir string `toml:"key_dir"`
	// DNS names the resolvers written into every instance's cloud-init
	// network configuration (SPEC 6.2). Empty selects the built-in
	// defaults.
	DNS []string `toml:"dns"`
	// Operators are the user names allowed to use the operator-only
	// dashboard controls, such as the database download (SPEC 12.1).
	Operators []string `toml:"operators"`

	Listen   Listen   `toml:"listen"`
	Defaults Defaults `toml:"defaults"`
	ACME     ACME     `toml:"acme"`
	OIDC     OIDC     `toml:"oidc"`

	// Images is the operator image allowlist (SPEC 5.1).
	Images []ImageEntry `toml:"images"`
}

// Listen holds the listen addresses of the three processes.
type Listen struct {
	// HTTP is the control plane listen address (dashboard and API).
	HTTP string `toml:"http"`
	// HTTPS is the HTTP proxy listen address for port 443.
	HTTPS string `toml:"https"`
	// SSH is the SSH frontend listen address.
	SSH string `toml:"ssh"`
	// ProxyPortMin and ProxyPortMax bound the extra proxy ports (SPEC 9.1).
	ProxyPortMin int `toml:"proxy_port_min"`
	ProxyPortMax int `toml:"proxy_port_max"`
	// TLS decides how the proxy listeners get their certificate.
	// TLSACME, the default, is SPEC 8: the proxy obtains the wildcard
	// certificate itself over DNS-01. TLSOff serves plain HTTP, for a
	// host where something else already owns port 443 and terminates
	// TLS in front of Bento; those listeners must not be reachable
	// from the internet directly.
	TLS string `toml:"tls"`
}

// Values of Listen.TLS.
const (
	// TLSACME lets the proxy manage the wildcard certificate (SPEC 8).
	TLSACME = "acme"
	// TLSOff serves plain HTTP behind a terminating proxy.
	TLSOff = "off"
)

// Defaults is the shape of a `new` without flags (SPEC 15). An empty
// Image selects the first allowlist entry.
type Defaults struct {
	Image     string `toml:"image"`
	VCPU      int    `toml:"vcpu"`
	MemoryMiB int64  `toml:"memory_mib"`
	DiskGiB   int64  `toml:"disk_gib"`
}

// ACME holds certificate settings (SPEC 8). The wildcard certificate
// needs the DNS-01 challenge, so a DNS provider token is required.
type ACME struct {
	// Email is the ACME account contact.
	Email string `toml:"email"`
	// CloudflareToken is the DNS API token, ideally limited to the
	// _acme-challenge records.
	CloudflareToken string `toml:"cloudflare_token"`
	// Directory overrides the ACME directory URL. Empty means the
	// production Let's Encrypt directory.
	Directory string `toml:"directory"`
}

// OIDC holds the dashboard identity settings (SPEC 13).
type OIDC struct {
	Issuer       string `toml:"issuer"`
	ClientID     string `toml:"client_id"`
	ClientSecret string `toml:"client_secret"`
}

// ImageEntry is one row of the operator image allowlist (SPEC 5.1).
type ImageEntry struct {
	Name string `toml:"name"`
	URL  string `toml:"url"`
	// PinnedChecksum, when set, rejects any download whose checksum
	// differs. Empty means trust on first use.
	PinnedChecksum string `toml:"pinned_checksum"`
}

// Default returns a Config with every default applied and no
// deployment-specific value set.
func Default() Config {
	return Config{
		LibvirtURI:       DefaultLibvirtURI,
		ImageDir:         DefaultImageDir,
		StorageDir:       DefaultStorageDir,
		DBPath:           DefaultDBPath,
		OvercommitRatio:  DefaultOvercommitRatio,
		NameCooldown:     Duration(DefaultNameCooldown),
		RestoreBatchSize: DefaultRestoreBatchSize,
		PrivateRange:     DefaultPrivateRange,
		KeyDir:           DefaultKeyDir,
		Defaults: Defaults{
			VCPU:      DefaultVCPU,
			MemoryMiB: DefaultMemoryMiB,
			DiskGiB:   DefaultDiskGiB,
		},
		Listen: Listen{
			HTTP:         DefaultListenHTTP,
			HTTPS:        DefaultListenHTTPS,
			SSH:          DefaultListenSSH,
			ProxyPortMin: DefaultProxyPortMin,
			ProxyPortMax: DefaultProxyPortMax,
			TLS:          TLSACME,
		},
	}
}

// listenPort returns the port of a listen address, or 0 when it has
// none to read.
func listenPort(addr string) int {
	_, portStr, err := net.SplitHostPort(addr)
	if err != nil {
		return 0
	}
	port, err := strconv.Atoi(portStr)
	if err != nil {
		return 0
	}
	return port
}

// Load reads the TOML file at path, applies defaults for unset values,
// and validates the result.
func Load(path string) (Config, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return Config{}, fmt.Errorf("config: %w", err)
	}
	return Parse(data)
}

// Parse decodes TOML bytes, applies defaults for unset values, and
// validates the result.
func Parse(data []byte) (Config, error) {
	cfg := Default()
	meta, err := toml.Decode(string(data), &cfg)
	if err != nil {
		return Config{}, fmt.Errorf("config: %w", err)
	}
	if undecoded := meta.Undecoded(); len(undecoded) > 0 {
		return Config{}, fmt.Errorf("config: unknown key %q", undecoded[0].String())
	}
	if err := cfg.validate(); err != nil {
		return Config{}, fmt.Errorf("config: %w", err)
	}
	return cfg, nil
}

func (c *Config) validate() error {
	if c.BaseDomain == "" {
		return fmt.Errorf("base_domain is required")
	}
	if c.OvercommitRatio < 1.0 {
		return fmt.Errorf("overcommit_ratio must be at least 1.0, got %g", c.OvercommitRatio)
	}
	if c.NameCooldown < 0 {
		return fmt.Errorf("name_cooldown must not be negative")
	}
	if c.RestoreBatchSize < 1 {
		return fmt.Errorf("restore_batch_size must be at least 1, got %d", c.RestoreBatchSize)
	}
	prefix, err := netip.ParsePrefix(c.PrivateRange)
	if err != nil {
		return fmt.Errorf("private_range: %w", err)
	}
	if !prefix.Addr().Is4() {
		return fmt.Errorf("private_range must be an IPv4 range, got %q", c.PrivateRange)
	}
	if prefix.Bits() > 24 {
		return fmt.Errorf("private_range must be /24 or wider to hold user /24s, got /%d", prefix.Bits())
	}
	if c.Listen.ProxyPortMin > c.Listen.ProxyPortMax {
		return fmt.Errorf("proxy_port_min %d exceeds proxy_port_max %d", c.Listen.ProxyPortMin, c.Listen.ProxyPortMax)
	}
	if port := listenPort(c.Listen.HTTP); port >= c.Listen.ProxyPortMin && port <= c.Listen.ProxyPortMax {
		return fmt.Errorf("listen http port %d falls inside the proxy port range %d-%d; the proxy binds every port of that range, so one of the two processes could not start",
			port, c.Listen.ProxyPortMin, c.Listen.ProxyPortMax)
	}
	switch c.Listen.TLS {
	case TLSACME, TLSOff:
	default:
		return fmt.Errorf("listen tls must be %q or %q, got %q", TLSACME, TLSOff, c.Listen.TLS)
	}
	for _, d := range c.DNS {
		if _, err := netip.ParseAddr(d); err != nil {
			return fmt.Errorf("dns: %w", err)
		}
	}
	if c.Defaults.VCPU < 1 || c.Defaults.MemoryMiB < 1 || c.Defaults.DiskGiB < 1 {
		return fmt.Errorf("defaults: vcpu, memory_mib, and disk_gib must be positive")
	}
	seen := make(map[string]bool, len(c.Images))
	for _, img := range c.Images {
		if img.Name == "" {
			return fmt.Errorf("images: entry with empty name")
		}
		if img.URL == "" {
			return fmt.Errorf("images: entry %q has no url", img.Name)
		}
		if seen[img.Name] {
			return fmt.Errorf("images: duplicate entry %q", img.Name)
		}
		seen[img.Name] = true
	}
	return nil
}

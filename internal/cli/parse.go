package cli

import (
	"fmt"
	"regexp"
	"strconv"
	"strings"
	"time"
)

// namePattern is a DNS label: the name appears in the URL and in the SSH
// user name (SPEC 7.2), so it must be a valid host label.
var namePattern = regexp.MustCompile(`^[a-z0-9]([a-z0-9-]*[a-z0-9])?$`)

// reservedNames never name an instance. "bento" would shadow the CLI
// session and the base domain.
var reservedNames = map[string]bool{"bento": true, "www": true}

func validateName(name string) error {
	if name == "" || len(name) > 63 {
		return fmt.Errorf("an instance name has 1 to 63 characters")
	}
	if !namePattern.MatchString(name) {
		return fmt.Errorf("an instance name uses lowercase letters, digits, and inner hyphens")
	}
	if reservedNames[name] {
		return fmt.Errorf("the name %q is reserved", name)
	}
	return nil
}

// parseMemoryMiB parses a --memory value. A bare number is MiB; the
// suffixes M/MB/MiB and G/GB/GiB are accepted (binary units).
func parseMemoryMiB(s string) (int64, error) {
	n, unit, err := splitSize(s)
	if err != nil {
		return 0, fmt.Errorf("invalid memory size %q", s)
	}
	switch unit {
	case "", "M", "MB", "MIB":
		return n, nil
	case "G", "GB", "GIB":
		return n * 1024, nil
	}
	return 0, fmt.Errorf("invalid memory unit in %q (use MiB or GiB)", s)
}

// parseDiskGiB parses a --disk value. A bare number is GiB; the suffixes
// G/GB/GiB are accepted.
func parseDiskGiB(s string) (int64, error) {
	n, unit, err := splitSize(s)
	if err != nil {
		return 0, fmt.Errorf("invalid disk size %q", s)
	}
	switch unit {
	case "", "G", "GB", "GIB":
		return n, nil
	}
	return 0, fmt.Errorf("invalid disk unit in %q (use GiB)", s)
}

func splitSize(s string) (int64, string, error) {
	s = strings.ToUpper(strings.TrimSpace(s))
	i := len(s)
	for i > 0 && (s[i-1] < '0' || s[i-1] > '9') {
		i--
	}
	n, err := strconv.ParseInt(s[:i], 10, 64)
	if err != nil || n <= 0 {
		return 0, "", fmt.Errorf("not a positive integer")
	}
	return n, strings.TrimSpace(s[i:]), nil
}

// formatCooldown renders a remaining cooldown for the SPEC 15 error
// message, rounding up to whole minutes.
func formatCooldown(d time.Duration) string {
	if d < time.Minute {
		return "less than a minute"
	}
	minutes := int64((d + time.Minute - 1) / time.Minute)
	h, m := minutes/60, minutes%60
	switch {
	case h == 0:
		return fmt.Sprintf("%dm", m)
	case m == 0:
		return fmt.Sprintf("%dh", h)
	}
	return fmt.Sprintf("%dh%dm", h, m)
}

// ago renders a last-use time for the ls output.
func ago(now, t time.Time) string {
	if t.IsZero() {
		return "never"
	}
	d := now.Sub(t)
	switch {
	case d < time.Minute:
		return "just now"
	case d < time.Hour:
		return fmt.Sprintf("%dm ago", int(d/time.Minute))
	case d < 24*time.Hour:
		return fmt.Sprintf("%dh ago", int(d/time.Hour))
	}
	return fmt.Sprintf("%dd ago", int(d/(24*time.Hour)))
}

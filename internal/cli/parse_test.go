package cli

import (
	"testing"
	"time"
)

func TestParseMemoryMiB(t *testing.T) {
	tests := []struct {
		in      string
		want    int64
		wantErr bool
	}{
		{in: "2048", want: 2048},
		{in: "512M", want: 512},
		{in: "512MiB", want: 512},
		{in: "2G", want: 2048},
		{in: "2GiB", want: 2048},
		{in: "2gb", want: 2048},
		{in: " 4 GiB ", want: 4096},
		{in: "0", wantErr: true},
		{in: "-1", wantErr: true},
		{in: "2T", wantErr: true},
		{in: "abc", wantErr: true},
		{in: "", wantErr: true},
	}
	for _, tt := range tests {
		t.Run(tt.in, func(t *testing.T) {
			got, err := parseMemoryMiB(tt.in)
			if (err != nil) != tt.wantErr {
				t.Fatalf("parseMemoryMiB(%q) error = %v, wantErr %v", tt.in, err, tt.wantErr)
			}
			if err == nil && got != tt.want {
				t.Errorf("parseMemoryMiB(%q) = %d, want %d", tt.in, got, tt.want)
			}
		})
	}
}

func TestParseDiskGiB(t *testing.T) {
	tests := []struct {
		in      string
		want    int64
		wantErr bool
	}{
		{in: "20", want: 20},
		{in: "20G", want: 20},
		{in: "20GiB", want: 20},
		{in: "20gb", want: 20},
		{in: "512M", wantErr: true},
		{in: "0", wantErr: true},
		{in: "x", wantErr: true},
	}
	for _, tt := range tests {
		t.Run(tt.in, func(t *testing.T) {
			got, err := parseDiskGiB(tt.in)
			if (err != nil) != tt.wantErr {
				t.Fatalf("parseDiskGiB(%q) error = %v, wantErr %v", tt.in, err, tt.wantErr)
			}
			if err == nil && got != tt.want {
				t.Errorf("parseDiskGiB(%q) = %d, want %d", tt.in, got, tt.want)
			}
		})
	}
}

func TestValidateName(t *testing.T) {
	tests := []struct {
		name    string
		wantErr bool
	}{
		{name: "web"},
		{name: "my-app-2"},
		{name: "a"},
		{name: "0z"},
		{name: ""},
		{name: "-web", wantErr: true},
		{name: "web-", wantErr: true},
		{name: "Web", wantErr: true},
		{name: "we_b", wantErr: true},
		{name: "we.b", wantErr: true},
		{name: "bento", wantErr: true},
		{name: "www", wantErr: true},
	}
	tests[4].wantErr = true // empty
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			err := validateName(tt.name)
			if (err != nil) != tt.wantErr {
				t.Errorf("validateName(%q) = %v, wantErr %v", tt.name, err, tt.wantErr)
			}
		})
	}
}

func TestFormatCooldown(t *testing.T) {
	tests := []struct {
		d    time.Duration
		want string
	}{
		{d: 30 * time.Second, want: "less than a minute"},
		{d: 90 * time.Second, want: "2m"},
		{d: 45 * time.Minute, want: "45m"},
		{d: time.Hour, want: "1h"},
		{d: 23*time.Hour + 59*time.Minute + 5*time.Second, want: "24h"},
		{d: 23*time.Hour + 30*time.Minute, want: "23h30m"},
		{d: 24 * time.Hour, want: "24h"},
	}
	for _, tt := range tests {
		if got := formatCooldown(tt.d); got != tt.want {
			t.Errorf("formatCooldown(%s) = %q, want %q", tt.d, got, tt.want)
		}
	}
}

func TestAgo(t *testing.T) {
	now := time.Date(2026, 8, 10, 12, 0, 0, 0, time.UTC)
	tests := []struct {
		t    time.Time
		want string
	}{
		{t: time.Time{}, want: "never"},
		{t: now.Add(-10 * time.Second), want: "just now"},
		{t: now.Add(-5 * time.Minute), want: "5m ago"},
		{t: now.Add(-3 * time.Hour), want: "3h ago"},
		{t: now.Add(-49 * time.Hour), want: "2d ago"},
	}
	for _, tt := range tests {
		if got := ago(now, tt.t); got != tt.want {
			t.Errorf("ago(%v) = %q, want %q", tt.t, got, tt.want)
		}
	}
}

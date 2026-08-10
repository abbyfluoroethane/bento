package cli

import (
	"crypto/ed25519"
	"crypto/rand"
	"strings"
	"testing"
	"time"

	"github.com/abbyfluoroethane/bento/internal/hypervisor"
	"github.com/abbyfluoroethane/bento/internal/store"
	"github.com/abbyfluoroethane/bento/internal/types"
	gossh "golang.org/x/crypto/ssh"
)

func TestNewFlagsAndDefaults(t *testing.T) {
	tests := []struct {
		name string
		args []string
		want CreateRequest
	}{
		{
			name: "defaults",
			args: []string{"new", "box"},
			want: CreateRequest{
				OwnerID: 1, Name: "box", Image: "debian-13",
				VCPU: 2, MemoryMiB: 2048, DiskGiB: 20, KSM: true,
			},
		},
		{
			name: "all flags",
			args: []string{"new", "--image", "ubuntu-lts", "--memory", "4G", "--cpu", "8", "--disk", "50G", "--nested", "--no-ksm", "box"},
			want: CreateRequest{
				OwnerID: 1, Name: "box", Image: "ubuntu-lts",
				VCPU: 8, MemoryMiB: 4096, DiskGiB: 50, Nested: true, KSM: false,
			},
		},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			_, lc, c := newFixture()
			code, out, errOut := run(t, c, alice, "", tt.args...)
			if code != 0 {
				t.Fatalf("exit %d, stderr %q", code, errOut)
			}
			if len(lc.created) != 1 || lc.created[0] != tt.want {
				t.Errorf("created %+v, want %+v", lc.created, tt.want)
			}
			if !strings.Contains(out, "created box") {
				t.Errorf("stdout %q misses creation line", out)
			}
		})
	}
}

func TestNewRejectsBadInput(t *testing.T) {
	tests := []struct {
		name     string
		args     []string
		wantCode int
		wantErr  string
	}{
		{name: "no name", args: []string{"new"}, wantCode: 2, wantErr: "usage"},
		{name: "bad name", args: []string{"new", "Bad_Name"}, wantCode: 1, wantErr: "lowercase"},
		{name: "reserved name", args: []string{"new", "bento"}, wantCode: 1, wantErr: "reserved"},
		{name: "bad memory", args: []string{"new", "--memory", "lots", "box"}, wantCode: 1, wantErr: "memory"},
		{name: "zero cpu", args: []string{"new", "--cpu", "0", "box"}, wantCode: 1, wantErr: "--cpu"},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			_, lc, c := newFixture()
			code, _, errOut := run(t, c, alice, "", tt.args...)
			if code != tt.wantCode {
				t.Fatalf("exit %d, want %d (stderr %q)", code, tt.wantCode, errOut)
			}
			if !strings.Contains(errOut, tt.wantErr) {
				t.Errorf("stderr %q misses %q", errOut, tt.wantErr)
			}
			if len(lc.created) != 0 {
				t.Errorf("lifecycle called despite invalid input")
			}
		})
	}
}

func TestNewReportsCooldown(t *testing.T) {
	// SPEC 15/19: a new that names a released name held by another user
	// must report the remaining cooldown.
	_, lc, c := newFixture()
	lc.err = &store.NameCooldownError{Name: "web", Remaining: 90 * time.Minute}
	code, _, errOut := run(t, c, alice, "", "new", "web")
	if code != 1 {
		t.Fatalf("exit %d, want 1", code)
	}
	for _, want := range []string{`"web"`, "released by another user", "cooldown", "1h30m"} {
		if !strings.Contains(errOut, want) {
			t.Errorf("stderr %q misses %q", errOut, want)
		}
	}
}

func TestNewReportsQuota(t *testing.T) {
	_, lc, c := newFixture()
	lc.err = &store.QuotaError{Limit: "memory", Used: 6144, Requested: 4096, Max: 8192}
	code, _, errOut := run(t, c, alice, "", "new", "box")
	if code != 1 {
		t.Fatalf("exit %d, want 1", code)
	}
	if !strings.Contains(errOut, "quota exceeded") || !strings.Contains(errOut, "memory limit is 8192") {
		t.Errorf("stderr %q misses quota details", errOut)
	}
}

func TestRmConfirmation(t *testing.T) {
	tests := []struct {
		name        string
		stdin       string
		args        []string
		wantCode    int
		wantRemoved bool
		wantPrompt  bool
	}{
		{name: "declined", stdin: "n\n", args: []string{"rm", "web"}, wantCode: 1, wantPrompt: true},
		{name: "empty input declines", stdin: "", args: []string{"rm", "web"}, wantCode: 1, wantPrompt: true},
		{name: "confirmed", stdin: "y\n", args: []string{"rm", "web"}, wantCode: 0, wantRemoved: true, wantPrompt: true},
		{name: "force skips prompt", stdin: "", args: []string{"rm", "--force", "web"}, wantCode: 0, wantRemoved: true},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			_, lc, c := newFixture()
			code, out, _ := run(t, c, alice, tt.stdin, tt.args...)
			if code != tt.wantCode {
				t.Fatalf("exit %d, want %d", code, tt.wantCode)
			}
			removed := len(lc.removed) == 1
			if removed != tt.wantRemoved {
				t.Errorf("removed = %v, want %v", removed, tt.wantRemoved)
			}
			// SPEC 14.4: the confirmation names the instance.
			hasPrompt := strings.Contains(out, `delete instance "web"?`)
			if hasPrompt != tt.wantPrompt {
				t.Errorf("prompt shown = %v, want %v (stdout %q)", hasPrompt, tt.wantPrompt, out)
			}
		})
	}
}

func TestRenameConfirmationGating(t *testing.T) {
	// "web" is off, "db" is public in the fixture.
	tests := []struct {
		name        string
		args        []string
		stdin       string
		wantCode    int
		wantRenamed bool
		wantPrompt  bool
	}{
		{name: "off renames without prompt", args: []string{"rename", "web", "web2"}, wantCode: 0, wantRenamed: true},
		{name: "public declined", args: []string{"rename", "db", "db2"}, stdin: "n\n", wantCode: 1, wantPrompt: true},
		{name: "public confirmed", args: []string{"rename", "db", "db2"}, stdin: "yes\n", wantCode: 0, wantRenamed: true, wantPrompt: true},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			_, lc, c := newFixture()
			code, out, _ := run(t, c, alice, tt.stdin, tt.args...)
			if code != tt.wantCode {
				t.Fatalf("exit %d, want %d", code, tt.wantCode)
			}
			if got := len(lc.renamed) == 1; got != tt.wantRenamed {
				t.Errorf("renamed = %v, want %v", got, tt.wantRenamed)
			}
			if tt.wantPrompt {
				// SPEC 7.3: state both facts: the old URL stops working
				// (and there is no redirect), and the SSH user name
				// changes.
				for _, fact := range []string{
					"https://db.bento.example.org/ stops working",
					"no redirect",
					"SSH user name changes",
				} {
					if !strings.Contains(out, fact) {
						t.Errorf("prompt %q misses fact %q", out, fact)
					}
				}
			} else if strings.Contains(out, "[y/N]") {
				t.Errorf("unexpected prompt for non-public rename: %q", out)
			}
		})
	}
}

func TestRenamePropagatesCooldown(t *testing.T) {
	_, lc, c := newFixture()
	lc.err = &store.NameCooldownError{Name: "web2", Remaining: 24 * time.Hour}
	code, _, errOut := run(t, c, alice, "", "rename", "web", "web2")
	if code != 1 || !strings.Contains(errOut, "cooldown") || !strings.Contains(errOut, "24h") {
		t.Errorf("exit %d, stderr %q; want cooldown message with 24h", code, errOut)
	}
}

func TestAuthz(t *testing.T) {
	tests := []struct {
		name    string
		user    types.User
		args    []string
		stdin   string
		wantErr string
	}{
		{
			// bob has no share on web: the message must not reveal that
			// web exists.
			name: "stranger denied", user: bob, args: []string{"start", "web"},
			wantErr: "no such instance or no access: web",
		},
		{
			name: "missing instance same message", user: alice, args: []string{"start", "ghost"},
			wantErr: "no such instance or no access: ghost",
		},
		{
			// alice holds a share on theirs, but only the owner deletes.
			name: "share cannot rm", user: alice, args: []string{"rm", "--force", "theirs"},
			wantErr: "only the owner",
		},
		{
			name: "share cannot rename", user: alice, args: []string{"rename", "theirs", "mine"},
			wantErr: "only the owner",
		},
		{
			name: "share cannot change visibility", user: alice, args: []string{"visibility", "theirs", "public"},
			wantErr: "only the owner",
		},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			_, lc, c := newFixture()
			code, _, errOut := run(t, c, tt.user, tt.stdin, tt.args...)
			if code != 1 {
				t.Fatalf("exit %d, want 1 (stderr %q)", code, errOut)
			}
			if !strings.Contains(errOut, tt.wantErr) {
				t.Errorf("stderr %q misses %q", errOut, tt.wantErr)
			}
			if n := len(lc.started) + len(lc.removed) + len(lc.renamed); n != 0 {
				t.Errorf("lifecycle called %d times despite denial", n)
			}
		})
	}
}

func TestSharedUserCanStart(t *testing.T) {
	_, lc, c := newFixture()
	code, out, errOut := run(t, c, alice, "", "start", "theirs")
	if code != 0 {
		t.Fatalf("exit %d, stderr %q", code, errOut)
	}
	if len(lc.started) != 1 || lc.started[0] != "theirs" {
		t.Errorf("started %v, want [theirs]", lc.started)
	}
	if !strings.Contains(out, "theirs is starting") {
		t.Errorf("stdout %q", out)
	}
}

func TestStartAlreadyRunning(t *testing.T) {
	_, lc, c := newFixture()
	code, out, _ := run(t, c, alice, "", "start", "web")
	if code != 0 || !strings.Contains(out, "already running") {
		t.Errorf("exit %d stdout %q", code, out)
	}
	if len(lc.started) != 0 {
		t.Errorf("start called for a running instance")
	}
}

func TestStopReportsPath(t *testing.T) {
	// SPEC 11.1: report which path the stop took.
	tests := []struct {
		result hypervisor.StopResult
		want   string
	}{
		{result: hypervisor.StopGraceful, want: "shut down after the ACPI request"},
		{result: hypervisor.StopForced, want: "forced off"},
		{result: hypervisor.StopNoop, want: "already stopped"},
	}
	for _, tt := range tests {
		t.Run(string(tt.result), func(t *testing.T) {
			_, lc, c := newFixture()
			lc.stopResult = tt.result
			code, out, _ := run(t, c, alice, "", "stop", "web")
			if code != 0 || !strings.Contains(out, tt.want) {
				t.Errorf("exit %d, stdout %q, want %q", code, out, tt.want)
			}
		})
	}
}

func TestCpRequiresStoppedSource(t *testing.T) {
	_, lc, c := newFixture()
	code, _, errOut := run(t, c, alice, "", "cp", "web", "web2")
	if code != 1 || !strings.Contains(errOut, "must be stopped") {
		t.Fatalf("exit %d, stderr %q", code, errOut)
	}
	if len(lc.copied) != 0 {
		t.Errorf("copy ran on a running source")
	}

	code, out, errOut := run(t, c, alice, "", "cp", "db", "db2")
	if code != 0 {
		t.Fatalf("exit %d, stderr %q", code, errOut)
	}
	want := CreateRequest{OwnerID: 1, Name: "db2", Image: "debian-13", VCPU: 4, MemoryMiB: 4096, DiskGiB: 40, KSM: true}
	if len(lc.copied) != 1 || lc.copied[0] != want {
		t.Errorf("copied %+v, want %+v", lc.copied, want)
	}
	if !strings.Contains(out, "created db2 from db") {
		t.Errorf("stdout %q", out)
	}
}

func TestResize(t *testing.T) {
	t.Run("warns restart and applies", func(t *testing.T) {
		_, lc, c := newFixture()
		code, out, errOut := run(t, c, alice, "", "resize", "--memory", "8G", "--cpu", "4", "web")
		if code != 0 {
			t.Fatalf("exit %d, stderr %q", code, errOut)
		}
		// SPEC 11.1: tell the user before the change.
		if !strings.Contains(out, "after a restart") {
			t.Errorf("stdout %q misses the restart warning", out)
		}
		if len(lc.resized) != 1 {
			t.Fatalf("resized %d times", len(lc.resized))
		}
		req := lc.resized[0]
		if req.MemoryMiB == nil || *req.MemoryMiB != 8192 || req.VCPU == nil || *req.VCPU != 4 || req.DiskGiB != nil || req.Nested != nil {
			t.Errorf("request %+v", req)
		}
	})
	t.Run("disk cannot shrink", func(t *testing.T) {
		_, lc, c := newFixture()
		code, _, errOut := run(t, c, alice, "", "resize", "--disk", "10", "web")
		if code != 1 || !strings.Contains(errOut, "can only grow") {
			t.Errorf("exit %d, stderr %q", code, errOut)
		}
		if len(lc.resized) != 0 {
			t.Errorf("resize applied a shrink")
		}
	})
	t.Run("no change is a usage error", func(t *testing.T) {
		_, _, c := newFixture()
		code, _, _ := run(t, c, alice, "", "resize", "web")
		if code != 2 {
			t.Errorf("exit %d, want 2", code)
		}
	})
	t.Run("nested flags conflict", func(t *testing.T) {
		_, _, c := newFixture()
		code, _, _ := run(t, c, alice, "", "resize", "--nested", "--no-nested", "web")
		if code != 2 {
			t.Errorf("exit %d, want 2", code)
		}
	})
}

func TestLsOutput(t *testing.T) {
	st, _, c := newFixture()
	st.shared = []types.Instance{{
		UUID: "uuid-theirs", Name: "theirs", OwnerID: 2, State: types.StateStopped,
		Address: "10.100.1.2",
	}}
	code, out, errOut := run(t, c, alice, "", "ls")
	if code != 0 {
		t.Fatalf("exit %d, stderr %q", code, errOut)
	}
	want := `instances 2/4 · vcpu 6/8 · memory 6144/8192 MiB · disk 60/100 GiB
NAME  STATE    ADDRESS     IMAGE      VISIBILITY  LAST USE
db    stopped  10.100.0.3  debian-13  public      never
web   running  10.100.0.2  debian-13  off         3h ago

shared with you:
NAME    STATE    ADDRESS     OWNER  LAST USE
theirs  stopped  10.100.1.2  bob    never
`
	if out != want {
		t.Errorf("ls output:\n%q\nwant:\n%q", out, want)
	}
}

func TestLsUnlimitedQuota(t *testing.T) {
	st, _, c := newFixture()
	st.quota = nil
	code, out, _ := run(t, c, alice, "", "ls")
	if code != 0 || !strings.Contains(out, "instances 2/- · vcpu 6/-") {
		t.Errorf("exit %d, stdout %q", code, out)
	}
}

func TestImagesOutput(t *testing.T) {
	st, _, c := newFixture()
	st.images = []types.Image{
		{Name: "ubuntu-lts", URL: "https://example.org/u", CurrentChecksum: "ccc"},
		{Name: "debian-13", URL: "https://example.org/d", CurrentChecksum: "aaa"},
	}
	// Fixture: web and theirs hold "aaa" (current), db holds "bbb"
	// (older). Nothing runs ubuntu-lts.
	code, out, errOut := run(t, c, alice, "", "images")
	if code != 0 {
		t.Fatalf("exit %d, stderr %q", code, errOut)
	}
	want := `NAME        CURRENT CHECKSUM  ON OLDER VERSIONS
debian-13   aaa               1
ubuntu-lts  ccc               0
`
	if out != want {
		t.Errorf("images output:\n%q\nwant:\n%q", out, want)
	}
}

func TestPort(t *testing.T) {
	// The port command goes through the lifecycle, which reloads the
	// nftables table (SPEC 6.3), never straight to the store.
	_, lc, c := newFixture()
	code, out, _ := run(t, c, alice, "", "port", "web", "3456")
	if code != 0 || lc.setPortUUID != "uuid-web" || lc.setPort != 3456 {
		t.Errorf("exit %d, set %q=%d", code, lc.setPortUUID, lc.setPort)
	}
	if !strings.Contains(out, "now 3456") {
		t.Errorf("stdout %q", out)
	}
	for _, bad := range []string{"0", "65536", "-1", "http"} {
		code, _, errOut := run(t, c, alice, "", "port", "web", bad)
		if code != 1 || !strings.Contains(errOut, "not a port") {
			t.Errorf("port %q: exit %d, stderr %q", bad, code, errOut)
		}
	}
}

func TestVisibility(t *testing.T) {
	// Visibility also goes through the lifecycle for the SPEC 6.3
	// firewall reload.
	_, lc, c := newFixture()
	code, out, _ := run(t, c, alice, "", "visibility", "web", "public")
	if code != 0 || lc.setVis != types.VisibilityPublic || lc.setVisUUID != "uuid-web" {
		t.Errorf("exit %d, set %s on %s", code, lc.setVis, lc.setVisUUID)
	}
	if !strings.Contains(out, "anyone can reach https://web.bento.example.org/") {
		t.Errorf("stdout %q", out)
	}
	code, _, _ = run(t, c, alice, "", "visibility", "web", "hidden")
	if code != 2 {
		t.Errorf("bad value: exit %d, want 2", code)
	}
}

func TestShare(t *testing.T) {
	t.Run("grant", func(t *testing.T) {
		st, _, c := newFixture()
		code, out, errOut := run(t, c, alice, "", "share", "web", "bob")
		if code != 0 {
			t.Fatalf("exit %d, stderr %q", code, errOut)
		}
		if len(st.addedShares) != 1 || st.addedShares[0] != [2]any{"uuid-web", int64(2)} {
			t.Errorf("added %+v", st.addedShares)
		}
		if !strings.Contains(out, "bob can now use web") {
			t.Errorf("stdout %q", out)
		}
	})
	t.Run("revoke", func(t *testing.T) {
		st, _, c := newFixture()
		st.shares = map[string][]types.Share{"uuid-web": {{InstanceUUID: "uuid-web", UserID: 2}}}
		code, out, _ := run(t, c, alice, "", "share", "--revoke", "web", "bob")
		if code != 0 || len(st.removed) != 1 {
			t.Errorf("exit %d, removed %+v", code, st.removed)
		}
		if !strings.Contains(out, "no longer has access") {
			t.Errorf("stdout %q", out)
		}
	})
	t.Run("revoke without share", func(t *testing.T) {
		_, _, c := newFixture()
		code, _, errOut := run(t, c, alice, "", "share", "--revoke", "web", "bob")
		if code != 1 || !strings.Contains(errOut, "has no share") {
			t.Errorf("exit %d, stderr %q", code, errOut)
		}
	})
	t.Run("unknown user", func(t *testing.T) {
		_, _, c := newFixture()
		code, _, errOut := run(t, c, alice, "", "share", "web", "mallory")
		if code != 1 || !strings.Contains(errOut, "no such user: mallory") {
			t.Errorf("exit %d, stderr %q", code, errOut)
		}
	})
	t.Run("self", func(t *testing.T) {
		_, _, c := newFixture()
		code, _, errOut := run(t, c, alice, "", "share", "web", "alice")
		if code != 1 || !strings.Contains(errOut, "you own") {
			t.Errorf("exit %d, stderr %q", code, errOut)
		}
	})
}

func TestSSHKeyAddListRemove(t *testing.T) {
	pub, _, err := ed25519.GenerateKey(rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	sshPub, err := gossh.NewPublicKey(pub)
	if err != nil {
		t.Fatal(err)
	}
	line := strings.TrimSpace(string(gossh.MarshalAuthorizedKey(sshPub))) + " laptop"
	wantFP := gossh.FingerprintSHA256(sshPub)

	st, _, c := newFixture()
	code, out, errOut := run(t, c, alice, "", "ssh-key", "add", line)
	if code != 0 {
		t.Fatalf("exit %d, stderr %q", code, errOut)
	}
	if len(st.addedKeys) != 1 {
		t.Fatalf("added %d keys", len(st.addedKeys))
	}
	k := st.addedKeys[0]
	if k.Fingerprint != wantFP || k.Comment != "laptop" || k.UserID != 1 {
		t.Errorf("stored key %+v, want fingerprint %s", k, wantFP)
	}
	if !strings.Contains(out, wantFP) {
		t.Errorf("stdout %q misses the fingerprint", out)
	}

	// add from stdin
	st2, _, c2 := newFixture()
	code, _, _ = run(t, c2, alice, line+"\n", "ssh-key", "add")
	if code != 0 || len(st2.addedKeys) != 1 {
		t.Errorf("stdin add: exit %d, added %d", code, len(st2.addedKeys))
	}

	// garbage
	code, _, errOut = run(t, c, alice, "", "ssh-key", "add", "not a key")
	if code != 1 || !strings.Contains(errOut, "authorized_keys") {
		t.Errorf("exit %d, stderr %q", code, errOut)
	}

	// list and remove
	st.keys = []types.SSHKey{{ID: 7, UserID: 1, Fingerprint: wantFP, Comment: "laptop", CreatedAt: testTime()}}
	code, out, _ = run(t, c, alice, "", "ssh-key", "list")
	if code != 0 || !strings.Contains(out, wantFP) || !strings.Contains(out, "7") {
		t.Errorf("list: exit %d, stdout %q", code, out)
	}
	code, _, _ = run(t, c, alice, "", "ssh-key", "remove", "7")
	if code != 0 || len(st.deletedKeys) != 1 || st.deletedKeys[0] != 7 {
		t.Errorf("remove: exit %d, deleted %v", code, st.deletedKeys)
	}
	code, _, errOut = run(t, c, alice, "", "ssh-key", "remove", "99")
	if code != 1 || !strings.Contains(errOut, "no key with id 99") {
		t.Errorf("remove missing: exit %d, stderr %q", code, errOut)
	}
}

func TestWhoami(t *testing.T) {
	_, _, c := newFixture()
	code, out, _ := run(t, c, alice, "", "whoami")
	if code != 0 {
		t.Fatalf("exit %d", code)
	}
	for _, want := range []string{"alice", "alice@example.com", "10.100.0.0/24", "instances 2/4"} {
		if !strings.Contains(out, want) {
			t.Errorf("whoami %q misses %q", out, want)
		}
	}
}

func TestConsole(t *testing.T) {
	_, lc, c := newFixture()
	code, out, _ := run(t, c, alice, "", "console", "web")
	if code != 0 || len(lc.consoled) != 1 || lc.consoled[0] != "web" {
		t.Errorf("exit %d, consoled %v", code, lc.consoled)
	}
	if !strings.Contains(out, "attached to web") {
		t.Errorf("stdout %q", out)
	}
}

func TestUnknownCommand(t *testing.T) {
	_, _, c := newFixture()
	code, _, errOut := run(t, c, alice, "", "destroy", "web")
	if code != 2 || !strings.Contains(errOut, `unknown command "destroy"`) {
		t.Errorf("exit %d, stderr %q", code, errOut)
	}
}

func TestHelpOnNoArgs(t *testing.T) {
	_, _, c := newFixture()
	code, out, _ := run(t, c, alice, "")
	if code != 0 || !strings.Contains(out, "ssh bento.example.org <command>") {
		t.Errorf("exit %d, stdout %q", code, out)
	}
}

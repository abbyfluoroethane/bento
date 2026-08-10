package api

import (
	"crypto/ed25519"
	"crypto/rand"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strconv"
	"strings"
	"testing"
	"time"

	"github.com/abbyfluoroethane/bento/internal/store"
	"github.com/abbyfluoroethane/bento/internal/types"
	"golang.org/x/crypto/ssh"
)

// fixture builds a server with two users (alice id 1, bob id 2), one
// instance owned by alice, and one owned by bob shared with alice.
type fixture struct {
	st   *fakeStore
	lc   *fakeLifecycle
	auth *fakeAuth
	srv  *Server
}

var (
	alice = types.User{ID: 1, Name: "alice", Email: "alice@example.com", CreatedAt: time.Unix(1000, 0)}
	bob   = types.User{ID: 2, Name: "bob", Email: "bob@example.com"}
)

func newFixture(t *testing.T) *fixture {
	t.Helper()
	st := newFakeStore()
	st.users[alice.ID] = alice
	st.users[bob.ID] = bob
	st.instances["uuid-web"] = types.Instance{
		UUID: "uuid-web", Name: "web", OwnerID: alice.ID,
		ImageName: "debian-13", BaseChecksum: "aaa",
		State: types.StateRunning, DesiredState: types.DesiredRunning,
		Address: "10.42.0.2", MAC: "ba:c9:e6:00:00:01",
		VCPU: 2, MemoryMiB: 2048, DiskGiB: 20,
		KSM: true, HTTPPort: 80, Visibility: types.VisibilityOff,
	}
	st.instances["uuid-db"] = types.Instance{
		UUID: "uuid-db", Name: "db", OwnerID: bob.ID,
		ImageName: "debian-13", BaseChecksum: "aaa",
		State: types.StateStopped, DesiredState: types.DesiredStopped,
		Address: "10.42.1.2", VCPU: 1, MemoryMiB: 1024, DiskGiB: 10,
		KSM: true, HTTPPort: 80, Visibility: types.VisibilityOff,
	}
	st.shares["uuid-db"] = []types.Share{{InstanceUUID: "uuid-db", UserID: alice.ID}}
	lc := &fakeLifecycle{st: st}
	auth := &fakeAuth{user: &alice}
	srv := New(Config{
		Store:      st,
		Lifecycle:  lc,
		Auth:       auth,
		IsOperator: func(u types.User) bool { return u.ID == alice.ID },
		DBPath:     "/var/lib/bento/bento.db",
	})
	return &fixture{st: st, lc: lc, auth: auth, srv: srv}
}

func (f *fixture) do(t *testing.T, method, path, body string) *httptest.ResponseRecorder {
	t.Helper()
	var req *http.Request
	if body == "" {
		req = httptest.NewRequest(method, path, nil)
	} else {
		req = httptest.NewRequest(method, path, strings.NewReader(body))
	}
	w := httptest.NewRecorder()
	f.srv.ServeHTTP(w, req)
	return w
}

func decodeBody[T any](t *testing.T, w *httptest.ResponseRecorder) T {
	t.Helper()
	var v T
	if err := json.Unmarshal(w.Body.Bytes(), &v); err != nil {
		t.Fatalf("decoding %q: %v", w.Body.String(), err)
	}
	return v
}

func TestAuthRequired(t *testing.T) {
	routes := []struct{ method, path string }{
		{"GET", "/api/whoami"},
		{"GET", "/api/instances"},
		{"POST", "/api/instances"},
		{"GET", "/api/instances/uuid-web"},
		{"DELETE", "/api/instances/uuid-web"},
		{"POST", "/api/instances/uuid-web/start"},
		{"POST", "/api/instances/uuid-web/stop"},
		{"POST", "/api/instances/uuid-web/restart"},
		{"POST", "/api/instances/uuid-web/rename"},
		{"POST", "/api/instances/uuid-web/resize"},
		{"POST", "/api/instances/uuid-web/port"},
		{"POST", "/api/instances/uuid-web/visibility"},
		{"GET", "/api/instances/uuid-web/shares"},
		{"POST", "/api/instances/uuid-web/shares"},
		{"DELETE", "/api/instances/uuid-web/shares/bob"},
		{"GET", "/api/images"},
		{"GET", "/api/ssh-keys"},
		{"POST", "/api/ssh-keys"},
		{"DELETE", "/api/ssh-keys/1"},
		{"GET", "/api/db.sqlite"},
	}
	f := newFixture(t)
	f.auth.user = nil // reject everyone
	for _, rt := range routes {
		w := f.do(t, rt.method, rt.path, "")
		if w.Code != http.StatusUnauthorized {
			t.Errorf("%s %s: got %d, want 401", rt.method, rt.path, w.Code)
		}
		if ct := w.Header().Get("Content-Type"); !strings.HasPrefix(ct, "application/json") {
			t.Errorf("%s %s: content type %q, want JSON", rt.method, rt.path, ct)
		}
	}
	if len(f.lc.calls) != 0 {
		t.Errorf("unauthenticated requests reached the lifecycle: %v", f.lc.calls)
	}
}

func TestUnknownAPIRouteIsJSON404(t *testing.T) {
	f := newFixture(t)
	w := f.do(t, "GET", "/api/nope", "")
	if w.Code != http.StatusNotFound {
		t.Fatalf("got %d, want 404", w.Code)
	}
	body := decodeBody[errorBody](t, w)
	if body.Error == "" {
		t.Error("404 body has no error field")
	}
}

func TestWhoami(t *testing.T) {
	f := newFixture(t)
	f.st.quotas[alice.ID] = types.Quota{UserID: 1, MaxInstances: 5, MaxVCPU: 8, MaxMemoryMiB: 8192, MaxDiskGiB: 100}

	w := f.do(t, "GET", "/api/whoami", "")
	if w.Code != http.StatusOK {
		t.Fatalf("got %d: %s", w.Code, w.Body)
	}
	resp := decodeBody[whoamiResponse](t, w)
	if resp.User.Name != "alice" || resp.User.Email != "alice@example.com" {
		t.Errorf("wrong user: %+v", resp.User)
	}
	if resp.Quota == nil || resp.Quota.MaxInstances != 5 || resp.Quota.MaxDiskGiB != 100 {
		t.Errorf("wrong quota: %+v", resp.Quota)
	}
	if resp.Usage.Instances != 1 || resp.Usage.VCPU != 2 || resp.Usage.MemoryMiB != 2048 || resp.Usage.DiskGiB != 20 {
		t.Errorf("wrong usage: %+v", resp.Usage)
	}
	if !resp.Operator || resp.DBPath != "/var/lib/bento/bento.db" {
		t.Errorf("operator fields wrong: operator=%t db_path=%q", resp.Operator, resp.DBPath)
	}

	// A non-operator sees neither the flag nor the path.
	f.auth.user = &bob
	resp = decodeBody[whoamiResponse](t, f.do(t, "GET", "/api/whoami", ""))
	if resp.Operator || resp.DBPath != "" {
		t.Errorf("bob must not be operator: %+v", resp)
	}
	if resp.Quota != nil {
		t.Errorf("bob has no quota row, want null quota, got %+v", resp.Quota)
	}
}

func TestListInstances(t *testing.T) {
	f := newFixture(t)
	f.st.quotas[alice.ID] = types.Quota{UserID: 1, MaxInstances: 5, MaxVCPU: 8, MaxMemoryMiB: 8192, MaxDiskGiB: 100}

	w := f.do(t, "GET", "/api/instances", "")
	if w.Code != http.StatusOK {
		t.Fatalf("got %d: %s", w.Code, w.Body)
	}
	resp := decodeBody[instanceListResponse](t, w)
	if len(resp.Instances) != 2 {
		t.Fatalf("got %d instances, want 2 (owned + shared)", len(resp.Instances))
	}
	// Sorted by name: db before web.
	if resp.Instances[0].Name != "db" || resp.Instances[1].Name != "web" {
		t.Errorf("not sorted by name: %s, %s", resp.Instances[0].Name, resp.Instances[1].Name)
	}
	if !resp.Instances[0].SharedWithMe || resp.Instances[0].Owner != "bob" {
		t.Errorf("shared instance not marked: %+v", resp.Instances[0])
	}
	if resp.Instances[1].SharedWithMe || resp.Instances[1].Owner != "alice" {
		t.Errorf("owned instance marked shared: %+v", resp.Instances[1])
	}
	if resp.Quota == nil || resp.Quota.MaxInstances != 5 {
		t.Errorf("quota missing: %+v", resp.Quota)
	}
	// Usage counts owned instances only, not shared ones.
	if resp.Usage.Instances != 1 {
		t.Errorf("usage instances = %d, want 1", resp.Usage.Instances)
	}
}

func TestCreateInstance(t *testing.T) {
	tests := []struct {
		name       string
		body       string
		wantStatus int
		wantCall   string // "" means the lifecycle must not be called
	}{
		{"defaults ksm on", `{"name":"api","image":"debian-13","vcpu":2,"memory_mib":2048,"disk_gib":20}`,
			http.StatusCreated, "create api"},
		{"explicit no ksm", `{"name":"noksm","image":"debian-13","ksm":false}`,
			http.StatusCreated, "create noksm"},
		{"bad name uppercase", `{"name":"Web","image":"debian-13"}`, http.StatusBadRequest, ""},
		{"bad name leading hyphen", `{"name":"-web","image":"debian-13"}`, http.StatusBadRequest, ""},
		{"bad name empty", `{"name":"","image":"debian-13"}`, http.StatusBadRequest, ""},
		{"missing image", `{"name":"api"}`, http.StatusBadRequest, ""},
		{"negative memory", `{"name":"api","image":"debian-13","memory_mib":-1}`, http.StatusBadRequest, ""},
		{"unknown field", `{"name":"api","image":"debian-13","bogus":1}`, http.StatusBadRequest, ""},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			f := newFixture(t)
			w := f.do(t, "POST", "/api/instances", tt.body)
			if w.Code != tt.wantStatus {
				t.Fatalf("got %d, want %d: %s", w.Code, tt.wantStatus, w.Body)
			}
			if tt.wantCall == "" {
				if len(f.lc.calls) != 0 {
					t.Errorf("lifecycle called on invalid input: %v", f.lc.calls)
				}
				return
			}
			if len(f.lc.calls) != 1 || f.lc.calls[0] != tt.wantCall {
				t.Errorf("calls = %v, want [%s]", f.lc.calls, tt.wantCall)
			}
		})
	}

	t.Run("ksm defaults true", func(t *testing.T) {
		f := newFixture(t)
		w := f.do(t, "POST", "/api/instances", `{"name":"api","image":"debian-13"}`)
		inst := decodeBody[instanceJSON](t, w)
		if !inst.KSM {
			t.Error("ksm not defaulted to true")
		}
	})

	t.Run("quota error is 409 with detail", func(t *testing.T) {
		f := newFixture(t)
		f.lc.err = &store.QuotaError{Limit: "memory", Used: 6144, Requested: 4096, Max: 8192}
		w := f.do(t, "POST", "/api/instances", `{"name":"api","image":"debian-13"}`)
		if w.Code != http.StatusConflict {
			t.Fatalf("got %d, want 409", w.Code)
		}
		body := decodeBody[errorBody](t, w)
		if body.Quota == nil || body.Quota.Limit != "memory" || body.Quota.Max != 8192 {
			t.Errorf("quota detail missing: %+v", body)
		}
	})

	t.Run("cooldown error is 409 naming the remaining time", func(t *testing.T) {
		f := newFixture(t)
		f.lc.err = &store.NameCooldownError{Name: "api", Remaining: 3 * time.Hour}
		w := f.do(t, "POST", "/api/instances", `{"name":"api","image":"debian-13"}`)
		if w.Code != http.StatusConflict {
			t.Fatalf("got %d, want 409", w.Code)
		}
		body := decodeBody[errorBody](t, w)
		if body.CooldownSeconds != int64((3 * time.Hour).Seconds()) {
			t.Errorf("cooldown_seconds = %d", body.CooldownSeconds)
		}
		if !strings.Contains(body.Error, "cooldown") {
			t.Errorf("error message %q does not mention the cooldown", body.Error)
		}
	})

	t.Run("name taken is 409", func(t *testing.T) {
		f := newFixture(t)
		f.lc.err = store.ErrNameTaken
		w := f.do(t, "POST", "/api/instances", `{"name":"web","image":"debian-13"}`)
		if w.Code != http.StatusConflict {
			t.Fatalf("got %d, want 409", w.Code)
		}
	})
}

func TestInstanceOwnership(t *testing.T) {
	// alice mutating: her own instance works, the one shared with her is
	// 403 (shares grant access, not control), a stranger's is 404, and a
	// missing UUID is 404.
	tests := []struct {
		name string
		uuid string
		want int
	}{
		{"owned", "uuid-web", http.StatusAccepted},
		{"shared", "uuid-db", http.StatusForbidden},
		{"missing", "uuid-nope", http.StatusNotFound},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			f := newFixture(t)
			w := f.do(t, "POST", "/api/instances/"+tt.uuid+"/start", "")
			if w.Code != tt.want {
				t.Fatalf("got %d, want %d: %s", w.Code, tt.want, w.Body)
			}
		})
	}

	t.Run("stranger gets 404 not 403", func(t *testing.T) {
		f := newFixture(t)
		f.st.instances["uuid-secret"] = types.Instance{UUID: "uuid-secret", Name: "secret", OwnerID: bob.ID}
		w := f.do(t, "POST", "/api/instances/uuid-secret/stop", "")
		if w.Code != http.StatusNotFound {
			t.Fatalf("got %d, want 404 (existence must stay hidden)", w.Code)
		}
	})

	t.Run("shared instance is visible via GET", func(t *testing.T) {
		f := newFixture(t)
		w := f.do(t, "GET", "/api/instances/uuid-db", "")
		if w.Code != http.StatusOK {
			t.Fatalf("got %d, want 200: %s", w.Code, w.Body)
		}
		inst := decodeBody[instanceJSON](t, w)
		if !inst.SharedWithMe || inst.Owner != "bob" {
			t.Errorf("wrong shared view: %+v", inst)
		}
	})
}

func TestLifecycleActions(t *testing.T) {
	for _, action := range []string{"start", "stop", "restart"} {
		t.Run(action, func(t *testing.T) {
			f := newFixture(t)
			w := f.do(t, "POST", "/api/instances/uuid-web/"+action, "")
			if w.Code != http.StatusAccepted {
				t.Fatalf("got %d: %s", w.Code, w.Body)
			}
			want := action + " uuid-web"
			if len(f.lc.calls) != 1 || f.lc.calls[0] != want {
				t.Errorf("calls = %v, want [%s]", f.lc.calls, want)
			}
		})
	}
}

func TestDeleteInstance(t *testing.T) {
	f := newFixture(t)
	w := f.do(t, "DELETE", "/api/instances/uuid-web", "")
	if w.Code != http.StatusNoContent {
		t.Fatalf("got %d: %s", w.Code, w.Body)
	}
	if len(f.lc.calls) != 1 || f.lc.calls[0] != "delete uuid-web" {
		t.Errorf("calls = %v", f.lc.calls)
	}
}

func TestRename(t *testing.T) {
	f := newFixture(t)
	w := f.do(t, "POST", "/api/instances/uuid-web/rename", `{"new_name":"web2"}`)
	if w.Code != http.StatusOK {
		t.Fatalf("got %d: %s", w.Code, w.Body)
	}
	inst := decodeBody[instanceJSON](t, w)
	if inst.Name != "web2" {
		t.Errorf("response name = %q, want web2", inst.Name)
	}

	w = f.do(t, "POST", "/api/instances/uuid-web/rename", `{"new_name":"Bad_Name"}`)
	if w.Code != http.StatusBadRequest {
		t.Errorf("invalid new name: got %d, want 400", w.Code)
	}
}

func TestResize(t *testing.T) {
	t.Run("partial fill keeps current values", func(t *testing.T) {
		f := newFixture(t)
		w := f.do(t, "POST", "/api/instances/uuid-web/resize", `{"memory_mib":4096}`)
		if w.Code != http.StatusOK {
			t.Fatalf("got %d: %s", w.Code, w.Body)
		}
		want := "resize uuid-web vcpu=2 mem=4096 disk=20 nested=false"
		if len(f.lc.calls) != 1 || f.lc.calls[0] != want {
			t.Errorf("calls = %v, want [%s]", f.lc.calls, want)
		}
	})

	t.Run("disk shrink rejected", func(t *testing.T) {
		f := newFixture(t)
		w := f.do(t, "POST", "/api/instances/uuid-web/resize", `{"disk_gib":10}`)
		if w.Code != http.StatusBadRequest {
			t.Fatalf("got %d, want 400: %s", w.Code, w.Body)
		}
		if len(f.lc.calls) != 0 {
			t.Errorf("lifecycle called on shrink: %v", f.lc.calls)
		}
	})

	t.Run("nested toggle", func(t *testing.T) {
		f := newFixture(t)
		w := f.do(t, "POST", "/api/instances/uuid-web/resize", `{"nested":true}`)
		if w.Code != http.StatusOK {
			t.Fatalf("got %d: %s", w.Code, w.Body)
		}
		if !strings.Contains(f.lc.calls[0], "nested=true") {
			t.Errorf("calls = %v", f.lc.calls)
		}
	})
}

func TestPort(t *testing.T) {
	tests := []struct {
		name string
		body string
		want int
	}{
		{"valid", `{"port":8080}`, http.StatusOK},
		{"zero", `{"port":0}`, http.StatusBadRequest},
		{"too big", `{"port":70000}`, http.StatusBadRequest},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			f := newFixture(t)
			w := f.do(t, "POST", "/api/instances/uuid-web/port", tt.body)
			if w.Code != tt.want {
				t.Fatalf("got %d, want %d: %s", w.Code, tt.want, w.Body)
			}
			if tt.want == http.StatusOK {
				inst := decodeBody[instanceJSON](t, w)
				if inst.HTTPPort != 8080 {
					t.Errorf("http_port = %d, want 8080", inst.HTTPPort)
				}
			}
		})
	}
}

func TestVisibility(t *testing.T) {
	f := newFixture(t)
	w := f.do(t, "POST", "/api/instances/uuid-web/visibility", `{"visibility":"public"}`)
	if w.Code != http.StatusOK {
		t.Fatalf("got %d: %s", w.Code, w.Body)
	}
	if got := f.st.instances["uuid-web"].Visibility; got != types.VisibilityPublic {
		t.Errorf("stored visibility = %q", got)
	}

	w = f.do(t, "POST", "/api/instances/uuid-web/visibility", `{"visibility":"hidden"}`)
	if w.Code != http.StatusBadRequest {
		t.Errorf("invalid value: got %d, want 400", w.Code)
	}
}

func TestShares(t *testing.T) {
	t.Run("add list remove", func(t *testing.T) {
		f := newFixture(t)
		w := f.do(t, "POST", "/api/instances/uuid-web/shares", `{"user":"bob"}`)
		if w.Code != http.StatusCreated {
			t.Fatalf("add: got %d: %s", w.Code, w.Body)
		}
		shares := decodeBody[[]shareJSON](t, f.do(t, "GET", "/api/instances/uuid-web/shares", ""))
		if len(shares) != 1 || shares[0].User != "bob" {
			t.Fatalf("shares = %+v", shares)
		}
		w = f.do(t, "DELETE", "/api/instances/uuid-web/shares/bob", "")
		if w.Code != http.StatusNoContent {
			t.Fatalf("remove: got %d", w.Code)
		}
		shares = decodeBody[[]shareJSON](t, f.do(t, "GET", "/api/instances/uuid-web/shares", ""))
		if len(shares) != 0 {
			t.Errorf("share not removed: %+v", shares)
		}
	})

	t.Run("unknown user", func(t *testing.T) {
		f := newFixture(t)
		w := f.do(t, "POST", "/api/instances/uuid-web/shares", `{"user":"mallory"}`)
		if w.Code != http.StatusNotFound {
			t.Errorf("got %d, want 404", w.Code)
		}
	})

	t.Run("share with self", func(t *testing.T) {
		f := newFixture(t)
		w := f.do(t, "POST", "/api/instances/uuid-web/shares", `{"user":"alice"}`)
		if w.Code != http.StatusBadRequest {
			t.Errorf("got %d, want 400", w.Code)
		}
	})

	t.Run("only the owner manages shares", func(t *testing.T) {
		f := newFixture(t)
		// alice has uuid-db shared with her but does not own it.
		w := f.do(t, "POST", "/api/instances/uuid-db/shares", `{"user":"alice"}`)
		if w.Code != http.StatusForbidden {
			t.Errorf("got %d, want 403", w.Code)
		}
	})
}

func TestImages(t *testing.T) {
	f := newFixture(t)
	f.st.images = []types.Image{
		{Name: "debian-13", URL: "https://example.com/d13.qcow2", CurrentChecksum: "bbb"},
		{Name: "fedora-42", URL: "https://example.com/f42.qcow2", CurrentChecksum: "ccc", PinnedChecksum: "ccc"},
	}
	// Both fixture instances hold base checksum aaa; current is bbb, so
	// both count as "on an older version" of debian-13.
	w := f.do(t, "GET", "/api/images", "")
	if w.Code != http.StatusOK {
		t.Fatalf("got %d: %s", w.Code, w.Body)
	}
	images := decodeBody[[]imageJSON](t, w)
	if len(images) != 2 {
		t.Fatalf("got %d images", len(images))
	}
	byName := map[string]imageJSON{}
	for _, img := range images {
		byName[img.Name] = img
	}
	if byName["debian-13"].InstancesOnOlderVersions != 2 {
		t.Errorf("debian-13 older count = %d, want 2", byName["debian-13"].InstancesOnOlderVersions)
	}
	if byName["fedora-42"].InstancesOnOlderVersions != 0 {
		t.Errorf("fedora-42 older count = %d, want 0", byName["fedora-42"].InstancesOnOlderVersions)
	}
}

func testPublicKey(t *testing.T) string {
	t.Helper()
	pub, _, err := ed25519.GenerateKey(rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	sshPub, err := ssh.NewPublicKey(pub)
	if err != nil {
		t.Fatal(err)
	}
	return strings.TrimSpace(string(ssh.MarshalAuthorizedKey(sshPub))) + " alice@laptop"
}

func TestSSHKeys(t *testing.T) {
	t.Run("add computes fingerprint and keeps key comment", func(t *testing.T) {
		f := newFixture(t)
		body, _ := json.Marshal(addSSHKeyRequest{PublicKey: testPublicKey(t)})
		w := f.do(t, "POST", "/api/ssh-keys", string(body))
		if w.Code != http.StatusCreated {
			t.Fatalf("got %d: %s", w.Code, w.Body)
		}
		key := decodeBody[sshKeyJSON](t, w)
		if !strings.HasPrefix(key.Fingerprint, "SHA256:") {
			t.Errorf("fingerprint = %q", key.Fingerprint)
		}
		if key.Comment != "alice@laptop" {
			t.Errorf("comment = %q, want the key comment", key.Comment)
		}

		keys := decodeBody[[]sshKeyJSON](t, f.do(t, "GET", "/api/ssh-keys", ""))
		if len(keys) != 1 || keys[0].ID != key.ID {
			t.Fatalf("list = %+v", keys)
		}

		w = f.do(t, "DELETE", "/api/ssh-keys/"+strconv.FormatInt(key.ID, 10), "")
		if w.Code != http.StatusNoContent {
			t.Fatalf("delete: got %d", w.Code)
		}
	})

	t.Run("invalid key rejected", func(t *testing.T) {
		f := newFixture(t)
		w := f.do(t, "POST", "/api/ssh-keys", `{"public_key":"not a key"}`)
		if w.Code != http.StatusBadRequest {
			t.Errorf("got %d, want 400", w.Code)
		}
	})

	t.Run("explicit comment wins", func(t *testing.T) {
		f := newFixture(t)
		body, _ := json.Marshal(addSSHKeyRequest{PublicKey: testPublicKey(t), Comment: "work"})
		key := decodeBody[sshKeyJSON](t, f.do(t, "POST", "/api/ssh-keys", string(body)))
		if key.Comment != "work" {
			t.Errorf("comment = %q, want work", key.Comment)
		}
	})
}

func TestDumpDB(t *testing.T) {
	t.Run("operator downloads a snapshot", func(t *testing.T) {
		f := newFixture(t)
		w := f.do(t, "GET", "/api/db.sqlite", "")
		if w.Code != http.StatusOK {
			t.Fatalf("got %d: %s", w.Code, w.Body)
		}
		if got := w.Header().Get("Content-Type"); got != "application/vnd.sqlite3" {
			t.Errorf("content type = %q", got)
		}
		if !strings.Contains(w.Header().Get("Content-Disposition"), "attachment") {
			t.Errorf("disposition = %q", w.Header().Get("Content-Disposition"))
		}
		if w.Body.String() != string(f.st.dumpBytes) {
			t.Errorf("body = %q, want the dump bytes", w.Body.String())
		}
	})

	t.Run("non-operator gets 403", func(t *testing.T) {
		f := newFixture(t)
		f.auth.user = &bob
		w := f.do(t, "GET", "/api/db.sqlite", "")
		if w.Code != http.StatusForbidden {
			t.Errorf("got %d, want 403", w.Code)
		}
	})

	t.Run("nil predicate denies everyone", func(t *testing.T) {
		f := newFixture(t)
		f.srv = New(Config{Store: f.st, Lifecycle: f.lc, Auth: f.auth})
		w := f.do(t, "GET", "/api/db.sqlite", "")
		if w.Code != http.StatusForbidden {
			t.Errorf("got %d, want 403", w.Code)
		}
	})
}

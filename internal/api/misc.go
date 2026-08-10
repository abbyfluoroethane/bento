package api

import (
	"errors"
	"io"
	"net/http"
	"os"
	"path/filepath"
	"strconv"
	"time"

	"github.com/abbyfluoroethane/bento/internal/store"
	"github.com/abbyfluoroethane/bento/internal/types"
	"golang.org/x/crypto/ssh"
)

type whoamiResponse struct {
	User     userJSON   `json:"user"`
	Quota    *quotaJSON `json:"quota"`
	Usage    usageJSON  `json:"usage"`
	Operator bool       `json:"operator"`
	// DBPath is the documented database path (SPEC 12.1), reported to
	// operators only.
	DBPath string `json:"db_path,omitempty"`
}

type userJSON struct {
	ID        int64  `json:"id"`
	Name      string `json:"name"`
	Email     string `json:"email"`
	CreatedAt string `json:"created_at"`
}

func (s *Server) isOperator(u types.User) bool {
	return s.cfg.IsOperator != nil && s.cfg.IsOperator(u)
}

func (s *Server) handleWhoami(w http.ResponseWriter, r *http.Request, u types.User) {
	usage, err := s.cfg.Store.UsageFor(u.ID)
	if err != nil {
		s.writeStoreError(w, err)
		return
	}
	resp := whoamiResponse{
		User:     userJSON{ID: u.ID, Name: u.Name, Email: u.Email, CreatedAt: rfc3339(u.CreatedAt)},
		Usage:    toUsageJSON(usage),
		Operator: s.isOperator(u),
	}
	if resp.Operator {
		resp.DBPath = s.cfg.DBPath
	}
	quota, err := s.cfg.Store.QuotaFor(u.ID)
	switch {
	case err == nil:
		resp.Quota = &quotaJSON{
			MaxInstances: quota.MaxInstances,
			MaxVCPU:      quota.MaxVCPU,
			MaxMemoryMiB: quota.MaxMemoryMiB,
			MaxDiskGiB:   quota.MaxDiskGiB,
		}
	case errors.Is(err, store.ErrNotFound):
	default:
		s.writeStoreError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, resp)
}

type imageJSON struct {
	Name            string `json:"name"`
	URL             string `json:"url"`
	PinnedChecksum  string `json:"pinned_checksum"`
	CurrentChecksum string `json:"current_checksum"`
	// InstancesOnOlderVersions counts instances built from a version that
	// is no longer current (SPEC 5.1, the `images` command).
	InstancesOnOlderVersions int `json:"instances_on_older_versions"`
}

func (s *Server) handleImages(w http.ResponseWriter, r *http.Request, _ types.User) {
	images, err := s.cfg.Store.Images()
	if err != nil {
		s.writeStoreError(w, err)
		return
	}
	instances, err := s.cfg.Store.Instances()
	if err != nil {
		s.writeStoreError(w, err)
		return
	}
	older := map[string]int{}
	current := map[string]string{}
	for _, img := range images {
		current[img.Name] = img.CurrentChecksum
	}
	for _, inst := range instances {
		cur, ok := current[inst.ImageName]
		if ok && inst.BaseChecksum != "" && inst.BaseChecksum != cur {
			older[inst.ImageName]++
		}
	}
	out := []imageJSON{}
	for _, img := range images {
		out = append(out, imageJSON{
			Name:                     img.Name,
			URL:                      img.URL,
			PinnedChecksum:           img.PinnedChecksum,
			CurrentChecksum:          img.CurrentChecksum,
			InstancesOnOlderVersions: older[img.Name],
		})
	}
	writeJSON(w, http.StatusOK, out)
}

type sshKeyJSON struct {
	ID          int64  `json:"id"`
	Fingerprint string `json:"fingerprint"`
	Comment     string `json:"comment"`
	PublicKey   string `json:"public_key"`
	CreatedAt   string `json:"created_at"`
}

func (s *Server) handleListSSHKeys(w http.ResponseWriter, r *http.Request, u types.User) {
	keys, err := s.cfg.Store.SSHKeysForUser(u.ID)
	if err != nil {
		s.writeStoreError(w, err)
		return
	}
	out := []sshKeyJSON{}
	for _, k := range keys {
		out = append(out, sshKeyJSON{
			ID:          k.ID,
			Fingerprint: k.Fingerprint,
			Comment:     k.Comment,
			PublicKey:   k.PublicKey,
			CreatedAt:   rfc3339(k.CreatedAt),
		})
	}
	writeJSON(w, http.StatusOK, out)
}

type addSSHKeyRequest struct {
	PublicKey string `json:"public_key"`
	Comment   string `json:"comment"`
}

func (s *Server) handleAddSSHKey(w http.ResponseWriter, r *http.Request, u types.User) {
	var req addSSHKeyRequest
	if err := decodeJSON(r, &req); err != nil {
		writeError(w, http.StatusBadRequest, "bad request body: "+err.Error())
		return
	}
	pub, keyComment, _, _, err := ssh.ParseAuthorizedKey([]byte(req.PublicKey))
	if err != nil {
		writeError(w, http.StatusBadRequest, "not a valid SSH public key")
		return
	}
	comment := req.Comment
	if comment == "" {
		comment = keyComment
	}
	fingerprint := ssh.FingerprintSHA256(pub)
	id, err := s.cfg.Store.AddSSHKey(u.ID, req.PublicKey, fingerprint, comment)
	if err != nil {
		s.writeStoreError(w, err)
		return
	}
	writeJSON(w, http.StatusCreated, sshKeyJSON{
		ID:          id,
		Fingerprint: fingerprint,
		Comment:     comment,
		PublicKey:   req.PublicKey,
	})
}

func (s *Server) handleDeleteSSHKey(w http.ResponseWriter, r *http.Request, u types.User) {
	id, err := strconv.ParseInt(r.PathValue("id"), 10, 64)
	if err != nil {
		writeError(w, http.StatusBadRequest, "bad key id")
		return
	}
	if err := s.cfg.Store.DeleteSSHKey(u.ID, id); err != nil {
		s.writeStoreError(w, err)
		return
	}
	w.WriteHeader(http.StatusNoContent)
}

// handleDumpDB streams a consistent database snapshot (SPEC 12.1: the
// "Download database" control). The store writes the snapshot with the
// SQLite backup mechanism — never a file copy, which is unsafe under WAL —
// into a temporary directory that is removed after streaming.
func (s *Server) handleDumpDB(w http.ResponseWriter, r *http.Request, u types.User) {
	if !s.isOperator(u) {
		writeError(w, http.StatusForbidden, "operator only")
		return
	}
	dir, err := os.MkdirTemp("", "bento-dump-*")
	if err != nil {
		writeError(w, http.StatusInternalServerError, err.Error())
		return
	}
	defer os.RemoveAll(dir)

	dest := filepath.Join(dir, "bento.db")
	if err := s.cfg.Store.DumpDB(dest); err != nil {
		s.writeStoreError(w, err)
		return
	}
	f, err := os.Open(dest)
	if err != nil {
		writeError(w, http.StatusInternalServerError, err.Error())
		return
	}
	defer f.Close()
	info, err := f.Stat()
	if err != nil {
		writeError(w, http.StatusInternalServerError, err.Error())
		return
	}

	w.Header().Set("Content-Type", "application/vnd.sqlite3")
	w.Header().Set("Content-Length", strconv.FormatInt(info.Size(), 10))
	w.Header().Set("Content-Disposition",
		`attachment; filename="bento-`+time.Now().UTC().Format("20060102T150405Z")+`.db"`)
	_, _ = io.Copy(w, f)
}

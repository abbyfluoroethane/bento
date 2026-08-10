package api

import (
	"errors"
	"net/http"
	"sort"
	"time"

	"github.com/abbyfluoroethane/bento/internal/store"
	"github.com/abbyfluoroethane/bento/internal/types"
)

// instanceJSON is the wire form of an instance row. Times are RFC 3339.
type instanceJSON struct {
	UUID         string `json:"uuid"`
	Name         string `json:"name"`
	Owner        string `json:"owner"`
	State        string `json:"state"`
	DesiredState string `json:"desired_state"`
	Address      string `json:"address"`
	MAC          string `json:"mac"`
	Image        string `json:"image"`
	BaseChecksum string `json:"base_checksum"`
	VCPU         int    `json:"vcpu"`
	MemoryMiB    int64  `json:"memory_mib"`
	DiskGiB      int64  `json:"disk_gib"`
	Nested       bool   `json:"nested"`
	KSM          bool   `json:"ksm"`
	HTTPPort     int    `json:"http_port"`
	Visibility   string `json:"visibility"`
	CreatedAt    string `json:"created_at"`
	LastSeenAt   string `json:"last_seen_at"`
	SharedWithMe bool   `json:"shared_with_me"`
}

type quotaJSON struct {
	MaxInstances int   `json:"max_instances"`
	MaxVCPU      int   `json:"max_vcpu"`
	MaxMemoryMiB int64 `json:"max_memory_mib"`
	MaxDiskGiB   int64 `json:"max_disk_gib"`
}

type usageJSON struct {
	Instances int   `json:"instances"`
	VCPU      int64 `json:"vcpu"`
	MemoryMiB int64 `json:"memory_mib"`
	DiskGiB   int64 `json:"disk_gib"`
}

func toUsageJSON(u store.Usage) usageJSON {
	return usageJSON{Instances: u.Instances, VCPU: u.VCPU, MemoryMiB: u.MemoryMiB, DiskGiB: u.DiskGiB}
}

func rfc3339(t time.Time) string {
	if t.IsZero() {
		return ""
	}
	return t.UTC().Format(time.RFC3339)
}

func (s *Server) toInstanceJSON(inst types.Instance, viewer types.User, owners map[int64]string) instanceJSON {
	owner, ok := owners[inst.OwnerID]
	if !ok {
		if u, err := s.cfg.Store.UserByID(inst.OwnerID); err == nil {
			owner = u.Name
		}
		owners[inst.OwnerID] = owner
	}
	return instanceJSON{
		UUID:         inst.UUID,
		Name:         inst.Name,
		Owner:        owner,
		State:        string(inst.State),
		DesiredState: string(inst.DesiredState),
		Address:      inst.Address,
		MAC:          inst.MAC,
		Image:        inst.ImageName,
		BaseChecksum: inst.BaseChecksum,
		VCPU:         inst.VCPU,
		MemoryMiB:    inst.MemoryMiB,
		DiskGiB:      inst.DiskGiB,
		Nested:       inst.Nested,
		KSM:          inst.KSM,
		HTTPPort:     inst.HTTPPort,
		Visibility:   string(inst.Visibility),
		CreatedAt:    rfc3339(inst.CreatedAt),
		LastSeenAt:   rfc3339(inst.LastSeenAt),
		SharedWithMe: inst.OwnerID != viewer.ID,
	}
}

type instanceListResponse struct {
	Instances []instanceJSON `json:"instances"`
	Quota     *quotaJSON     `json:"quota"`
	Usage     usageJSON      `json:"usage"`
}

// handleListInstances answers the primary dashboard view (SPEC 14.4): the
// user's instances plus the ones shared with them, sorted by name, with
// the four quota limits and current use for the bar above the table.
func (s *Server) handleListInstances(w http.ResponseWriter, r *http.Request, u types.User) {
	owned, err := s.cfg.Store.InstancesByOwner(u.ID)
	if err != nil {
		s.writeStoreError(w, err)
		return
	}
	shared, err := s.cfg.Store.InstancesSharedWith(u.ID)
	if err != nil {
		s.writeStoreError(w, err)
		return
	}
	usage, err := s.cfg.Store.UsageFor(u.ID)
	if err != nil {
		s.writeStoreError(w, err)
		return
	}

	resp := instanceListResponse{Instances: []instanceJSON{}, Usage: toUsageJSON(usage)}
	owners := map[int64]string{u.ID: u.Name}
	for _, inst := range append(owned, shared...) {
		resp.Instances = append(resp.Instances, s.toInstanceJSON(inst, u, owners))
	}
	sort.Slice(resp.Instances, func(i, j int) bool {
		return resp.Instances[i].Name < resp.Instances[j].Name
	})

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
		// No quotas row means unlimited; the dashboard shows use only.
	default:
		s.writeStoreError(w, err)
		return
	}

	writeJSON(w, http.StatusOK, resp)
}

func (s *Server) handleGetInstance(w http.ResponseWriter, r *http.Request, u types.User) {
	inst, err := s.cfg.Store.Instance(r.PathValue("uuid"))
	if err != nil {
		s.writeStoreError(w, err)
		return
	}
	if inst.OwnerID != u.ID {
		shares, err := s.cfg.Store.SharesFor(inst.UUID)
		if err != nil || !sharedWith(shares, u.ID) {
			writeError(w, http.StatusNotFound, "not found")
			return
		}
	}
	owners := map[int64]string{u.ID: u.Name}
	writeJSON(w, http.StatusOK, s.toInstanceJSON(inst, u, owners))
}

type createRequest struct {
	Name      string `json:"name"`
	Image     string `json:"image"`
	VCPU      int    `json:"vcpu"`
	MemoryMiB int64  `json:"memory_mib"`
	DiskGiB   int64  `json:"disk_gib"`
	Nested    bool   `json:"nested"`
	// KSM defaults to on (SPEC 5.4); a pointer keeps "absent" distinct
	// from the JSON zero value false.
	KSM *bool `json:"ksm"`
}

func (s *Server) handleCreateInstance(w http.ResponseWriter, r *http.Request, u types.User) {
	var req createRequest
	if err := decodeJSON(r, &req); err != nil {
		writeError(w, http.StatusBadRequest, "bad request body: "+err.Error())
		return
	}
	if !validName(req.Name) {
		writeError(w, http.StatusBadRequest,
			"instance name must be a DNS label: lower-case letters, digits, and hyphens, up to 63 characters")
		return
	}
	if req.Image == "" {
		writeError(w, http.StatusBadRequest, "image is required")
		return
	}
	if req.VCPU < 0 || req.MemoryMiB < 0 || req.DiskGiB < 0 {
		writeError(w, http.StatusBadRequest, "vcpu, memory_mib, and disk_gib must not be negative")
		return
	}
	spec := CreateSpec{
		Name:      req.Name,
		Image:     req.Image,
		VCPU:      req.VCPU,
		MemoryMiB: req.MemoryMiB,
		DiskGiB:   req.DiskGiB,
		Nested:    req.Nested,
		KSM:       req.KSM == nil || *req.KSM,
	}
	inst, err := s.cfg.Lifecycle.Create(r.Context(), u, spec)
	if err != nil {
		s.writeStoreError(w, err)
		return
	}
	owners := map[int64]string{u.ID: u.Name}
	writeJSON(w, http.StatusCreated, s.toInstanceJSON(inst, u, owners))
}

func (s *Server) handleDeleteInstance(w http.ResponseWriter, r *http.Request, u types.User) {
	inst, ok := s.ownedInstance(w, r, u)
	if !ok {
		return
	}
	if err := s.cfg.Lifecycle.Delete(r.Context(), inst.UUID); err != nil {
		s.writeStoreError(w, err)
		return
	}
	w.WriteHeader(http.StatusNoContent)
}

// actionHandler builds the start/stop/restart handlers, which differ only
// in the lifecycle call.
func (s *Server) actionHandler(action string) func(http.ResponseWriter, *http.Request, types.User) {
	return func(w http.ResponseWriter, r *http.Request, u types.User) {
		inst, ok := s.ownedInstance(w, r, u)
		if !ok {
			return
		}
		var err error
		switch action {
		case "start":
			err = s.cfg.Lifecycle.Start(r.Context(), inst.UUID)
		case "stop":
			err = s.cfg.Lifecycle.Stop(r.Context(), inst.UUID)
		case "restart":
			err = s.cfg.Lifecycle.Restart(r.Context(), inst.UUID)
		}
		if err != nil {
			s.writeStoreError(w, err)
			return
		}
		writeJSON(w, http.StatusAccepted, map[string]string{"uuid": inst.UUID, "action": action})
	}
}

type renameRequest struct {
	NewName string `json:"new_name"`
}

// handleRename renames an instance. The confirmation dialog of SPEC 7.3
// (old links break, the SSH user name changes) is the dashboard's job;
// the API performs the rename it is asked for.
func (s *Server) handleRename(w http.ResponseWriter, r *http.Request, u types.User) {
	inst, ok := s.ownedInstance(w, r, u)
	if !ok {
		return
	}
	var req renameRequest
	if err := decodeJSON(r, &req); err != nil {
		writeError(w, http.StatusBadRequest, "bad request body: "+err.Error())
		return
	}
	if !validName(req.NewName) {
		writeError(w, http.StatusBadRequest,
			"instance name must be a DNS label: lower-case letters, digits, and hyphens, up to 63 characters")
		return
	}
	if err := s.cfg.Lifecycle.Rename(r.Context(), inst.UUID, req.NewName); err != nil {
		s.writeStoreError(w, err)
		return
	}
	s.writeRefreshed(w, inst.UUID, u)
}

type resizeRequest struct {
	VCPU      int   `json:"vcpu"`
	MemoryMiB int64 `json:"memory_mib"`
	DiskGiB   int64 `json:"disk_gib"`
	// Nested is a pointer so "leave it alone" is distinct from false.
	Nested *bool `json:"nested"`
}

func (s *Server) handleResize(w http.ResponseWriter, r *http.Request, u types.User) {
	inst, ok := s.ownedInstance(w, r, u)
	if !ok {
		return
	}
	var req resizeRequest
	if err := decodeJSON(r, &req); err != nil {
		writeError(w, http.StatusBadRequest, "bad request body: "+err.Error())
		return
	}
	spec := ResizeSpec{
		VCPU:      inst.VCPU,
		MemoryMiB: inst.MemoryMiB,
		DiskGiB:   inst.DiskGiB,
		Nested:    inst.Nested,
	}
	if req.VCPU != 0 {
		spec.VCPU = req.VCPU
	}
	if req.MemoryMiB != 0 {
		spec.MemoryMiB = req.MemoryMiB
	}
	if req.DiskGiB != 0 {
		spec.DiskGiB = req.DiskGiB
	}
	if req.Nested != nil {
		spec.Nested = *req.Nested
	}
	if spec.VCPU < 1 || spec.MemoryMiB < 1 {
		writeError(w, http.StatusBadRequest, "vcpu and memory_mib must be positive")
		return
	}
	// A qcow2 overlay only grows (SPEC 11.1); catching a shrink here gives
	// a clear message instead of a qemu-img failure.
	if spec.DiskGiB < inst.DiskGiB {
		writeError(w, http.StatusBadRequest, "the disk can grow but never shrink")
		return
	}
	if err := s.cfg.Lifecycle.Resize(r.Context(), inst.UUID, spec); err != nil {
		s.writeStoreError(w, err)
		return
	}
	s.writeRefreshed(w, inst.UUID, u)
}

type portRequest struct {
	Port int `json:"port"`
}

func (s *Server) handlePort(w http.ResponseWriter, r *http.Request, u types.User) {
	inst, ok := s.ownedInstance(w, r, u)
	if !ok {
		return
	}
	var req portRequest
	if err := decodeJSON(r, &req); err != nil {
		writeError(w, http.StatusBadRequest, "bad request body: "+err.Error())
		return
	}
	if req.Port < 1 || req.Port > 65535 {
		writeError(w, http.StatusBadRequest, "port must be between 1 and 65535")
		return
	}
	if err := s.cfg.Lifecycle.SetHTTPPort(r.Context(), inst.UUID, req.Port); err != nil {
		s.writeStoreError(w, err)
		return
	}
	s.writeRefreshed(w, inst.UUID, u)
}

type visibilityRequest struct {
	Visibility string `json:"visibility"`
}

func (s *Server) handleVisibility(w http.ResponseWriter, r *http.Request, u types.User) {
	inst, ok := s.ownedInstance(w, r, u)
	if !ok {
		return
	}
	var req visibilityRequest
	if err := decodeJSON(r, &req); err != nil {
		writeError(w, http.StatusBadRequest, "bad request body: "+err.Error())
		return
	}
	v := types.Visibility(req.Visibility)
	if v != types.VisibilityOff && v != types.VisibilityPrivate && v != types.VisibilityPublic {
		writeError(w, http.StatusBadRequest, `visibility must be "off", "private", or "public"`)
		return
	}
	// Through the lifecycle, not the store: a visibility change alters
	// the published ports, and SPEC 6.3 reloads the nftables table on
	// every change.
	if err := s.cfg.Lifecycle.SetVisibility(r.Context(), inst.UUID, v); err != nil {
		s.writeStoreError(w, err)
		return
	}
	s.writeRefreshed(w, inst.UUID, u)
}

// writeRefreshed answers a mutation with the instance as it now stands.
func (s *Server) writeRefreshed(w http.ResponseWriter, uuid string, u types.User) {
	inst, err := s.cfg.Store.Instance(uuid)
	if err != nil {
		s.writeStoreError(w, err)
		return
	}
	owners := map[int64]string{u.ID: u.Name}
	writeJSON(w, http.StatusOK, s.toInstanceJSON(inst, u, owners))
}

type shareJSON struct {
	User      string `json:"user"`
	CreatedAt string `json:"created_at"`
}

func (s *Server) handleListShares(w http.ResponseWriter, r *http.Request, u types.User) {
	inst, ok := s.ownedInstance(w, r, u)
	if !ok {
		return
	}
	shares, err := s.cfg.Store.SharesFor(inst.UUID)
	if err != nil {
		s.writeStoreError(w, err)
		return
	}
	out := []shareJSON{}
	for _, sh := range shares {
		name := ""
		if user, err := s.cfg.Store.UserByID(sh.UserID); err == nil {
			name = user.Name
		}
		out = append(out, shareJSON{User: name, CreatedAt: rfc3339(sh.CreatedAt)})
	}
	writeJSON(w, http.StatusOK, out)
}

type shareRequest struct {
	User string `json:"user"`
}

func (s *Server) handleAddShare(w http.ResponseWriter, r *http.Request, u types.User) {
	inst, ok := s.ownedInstance(w, r, u)
	if !ok {
		return
	}
	var req shareRequest
	if err := decodeJSON(r, &req); err != nil {
		writeError(w, http.StatusBadRequest, "bad request body: "+err.Error())
		return
	}
	target, err := s.cfg.Store.UserByName(req.User)
	if err != nil {
		if errors.Is(err, store.ErrNotFound) {
			writeError(w, http.StatusNotFound, "no user named "+req.User)
			return
		}
		s.writeStoreError(w, err)
		return
	}
	if target.ID == u.ID {
		writeError(w, http.StatusBadRequest, "you already own this instance")
		return
	}
	if err := s.cfg.Store.AddShare(inst.UUID, target.ID); err != nil {
		s.writeStoreError(w, err)
		return
	}
	writeJSON(w, http.StatusCreated, shareJSON{User: target.Name})
}

func (s *Server) handleRemoveShare(w http.ResponseWriter, r *http.Request, u types.User) {
	inst, ok := s.ownedInstance(w, r, u)
	if !ok {
		return
	}
	target, err := s.cfg.Store.UserByName(r.PathValue("user"))
	if err != nil {
		if errors.Is(err, store.ErrNotFound) {
			writeError(w, http.StatusNotFound, "no user named "+r.PathValue("user"))
			return
		}
		s.writeStoreError(w, err)
		return
	}
	if err := s.cfg.Store.RemoveShare(inst.UUID, target.ID); err != nil {
		s.writeStoreError(w, err)
		return
	}
	w.WriteHeader(http.StatusNoContent)
}

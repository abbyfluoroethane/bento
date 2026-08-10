// Package api serves the control plane HTTP API consumed by the dashboard
// (SPEC section 14). The dashboard exposes every operation of SPEC section
// 15, so the API carries all of them: instance CRUD and lifecycle actions,
// rename, resize, port, visibility, shares, images, SSH keys, whoami, and
// the database download of SPEC 12.1.
package api

import (
	"encoding/json"
	"errors"
	"io"
	"net/http"
	"regexp"

	"github.com/abbyfluoroethane/bento/internal/store"
	"github.com/abbyfluoroethane/bento/internal/types"
)

// Config wires a Server. Store, Lifecycle, and Auth are required.
type Config struct {
	Store     Store
	Lifecycle Lifecycle
	Auth      Authenticator

	// IsOperator gates the operator-only routes (GET /api/db.sqlite,
	// SPEC 12.1). A nil predicate denies everyone.
	IsOperator func(types.User) bool

	// DBPath is the documented database path (default
	// /var/lib/bento/bento.db). SPEC 12.1 shows it on the dashboard, so
	// whoami reports it to operators.
	DBPath string
}

// Server is the /api/ HTTP handler.
type Server struct {
	cfg Config
	mux *http.ServeMux
}

// New builds the API server and registers every route.
func New(cfg Config) *Server {
	s := &Server{cfg: cfg, mux: http.NewServeMux()}

	s.route("GET /api/whoami", s.handleWhoami)
	s.route("GET /api/instances", s.handleListInstances)
	s.route("POST /api/instances", s.handleCreateInstance)
	s.route("GET /api/instances/{uuid}", s.handleGetInstance)
	s.route("DELETE /api/instances/{uuid}", s.handleDeleteInstance)
	s.route("POST /api/instances/{uuid}/start", s.actionHandler("start"))
	s.route("POST /api/instances/{uuid}/stop", s.actionHandler("stop"))
	s.route("POST /api/instances/{uuid}/restart", s.actionHandler("restart"))
	s.route("POST /api/instances/{uuid}/rename", s.handleRename)
	s.route("POST /api/instances/{uuid}/resize", s.handleResize)
	s.route("POST /api/instances/{uuid}/port", s.handlePort)
	s.route("POST /api/instances/{uuid}/visibility", s.handleVisibility)
	s.route("GET /api/instances/{uuid}/shares", s.handleListShares)
	s.route("POST /api/instances/{uuid}/shares", s.handleAddShare)
	s.route("DELETE /api/instances/{uuid}/shares/{user}", s.handleRemoveShare)
	s.route("GET /api/images", s.handleImages)
	s.route("GET /api/ssh-keys", s.handleListSSHKeys)
	s.route("POST /api/ssh-keys", s.handleAddSSHKey)
	s.route("DELETE /api/ssh-keys/{id}", s.handleDeleteSSHKey)
	s.route("GET /api/db.sqlite", s.handleDumpDB)
	s.mux.HandleFunc("/api/", func(w http.ResponseWriter, r *http.Request) {
		writeError(w, http.StatusNotFound, "not found")
	})

	return s
}

func (s *Server) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	s.mux.ServeHTTP(w, r)
}

// route registers a handler behind authentication. Every API route,
// reading or mutating, requires a user.
func (s *Server) route(pattern string, h func(http.ResponseWriter, *http.Request, types.User)) {
	s.mux.HandleFunc(pattern, func(w http.ResponseWriter, r *http.Request) {
		user, err := s.cfg.Auth.UserFromRequest(r)
		if err != nil {
			writeError(w, http.StatusUnauthorized, "unauthorized")
			return
		}
		h(w, r, user)
	})
}

// ownedInstance loads the instance in the URL and enforces that the user
// owns it. A stranger gets 404, never 403: the visibility rules of SPEC
// 9.2 hide the existence of an instance, and the API keeps that property.
// A user the instance is shared with gets 403 on mutation: shares grant
// access to the machine, not control over it.
func (s *Server) ownedInstance(w http.ResponseWriter, r *http.Request, u types.User) (types.Instance, bool) {
	inst, err := s.cfg.Store.Instance(r.PathValue("uuid"))
	if err != nil {
		s.writeStoreError(w, err)
		return types.Instance{}, false
	}
	if inst.OwnerID != u.ID {
		if ok, err := s.cfg.Store.SharesFor(inst.UUID); err == nil && sharedWith(ok, u.ID) {
			writeError(w, http.StatusForbidden, "only the owner may do this")
		} else {
			writeError(w, http.StatusNotFound, "not found")
		}
		return types.Instance{}, false
	}
	return inst, true
}

func sharedWith(shares []types.Share, userID int64) bool {
	for _, sh := range shares {
		if sh.UserID == userID {
			return true
		}
	}
	return false
}

// nameRe accepts a DNS label: the instance name appears in the URL and in
// the SSH user name (SPEC 7.2), so it must be a valid host name label.
var nameRe = regexp.MustCompile(`^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$`)

func validName(name string) bool { return nameRe.MatchString(name) }

// decodeJSON reads a small JSON body into v.
func decodeJSON(r *http.Request, v any) error {
	dec := json.NewDecoder(io.LimitReader(r.Body, 1<<20))
	dec.DisallowUnknownFields()
	return dec.Decode(v)
}

func writeJSON(w http.ResponseWriter, status int, v any) {
	w.Header().Set("Content-Type", "application/json; charset=utf-8")
	w.WriteHeader(status)
	_ = json.NewEncoder(w).Encode(v)
}

type errorBody struct {
	Error string `json:"error"`
	// CooldownSeconds is set when a name is in another user's cooldown
	// (SPEC 7.2); the message also carries it, this field is for programs.
	CooldownSeconds int64 `json:"cooldown_seconds,omitempty"`
	// Quota is set when a create or resize would exceed a limit.
	Quota *quotaDetail `json:"quota,omitempty"`
}

type quotaDetail struct {
	Limit     string `json:"limit"`
	Used      int64  `json:"used"`
	Requested int64  `json:"requested"`
	Max       int64  `json:"max"`
}

func writeError(w http.ResponseWriter, status int, msg string) {
	writeJSON(w, status, errorBody{Error: msg})
}

// writeStoreError maps store and lifecycle errors onto HTTP statuses.
func (s *Server) writeStoreError(w http.ResponseWriter, err error) {
	var (
		qe *store.QuotaError
		ce *store.NameCooldownError
		se StatusError
	)
	switch {
	case errors.Is(err, store.ErrNotFound):
		writeError(w, http.StatusNotFound, "not found")
	case errors.Is(err, store.ErrNameTaken):
		writeError(w, http.StatusConflict, "that name is taken")
	case errors.As(err, &qe):
		writeJSON(w, http.StatusConflict, errorBody{
			Error: qe.Error(),
			Quota: &quotaDetail{Limit: qe.Limit, Used: qe.Used, Requested: qe.Requested, Max: qe.Max},
		})
	case errors.As(err, &ce):
		writeJSON(w, http.StatusConflict, errorBody{
			Error:           ce.Error(),
			CooldownSeconds: int64(ce.Remaining.Seconds()),
		})
	case errors.As(err, &se):
		writeError(w, se.HTTPStatus(), se.Error())
	default:
		writeError(w, http.StatusInternalServerError, err.Error())
	}
}

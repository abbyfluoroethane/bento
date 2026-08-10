package dashboard

import (
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"testing/fstest"

	"github.com/abbyfluoroethane/bento/web"
)

func buildFS() fstest.MapFS {
	return fstest.MapFS{
		"index.html":            {Data: []byte("<!doctype html><div id=root></div>")},
		"assets/app-abc123.js":  {Data: []byte("console.log('bento')")},
		"assets/app-abc123.css": {Data: []byte(":root{}")},
		"favicon.svg":           {Data: []byte("<svg/>")},
	}
}

func get(t *testing.T, h http.Handler, path string) *httptest.ResponseRecorder {
	t.Helper()
	w := httptest.NewRecorder()
	h.ServeHTTP(w, httptest.NewRequest(http.MethodGet, path, nil))
	return w
}

func TestServesFiles(t *testing.T) {
	h := HandlerFromFS(buildFS())
	tests := []struct {
		path       string
		wantStatus int
		wantBody   string // substring; "" skips the check
		wantCache  string
		wantCTPart string
	}{
		{"/", http.StatusOK, "id=root", "no-cache", "text/html"},
		// net/http canonicalizes /index.html to ./ with a redirect.
		{"/index.html", http.StatusMovedPermanently, "", "", ""},
		{"/assets/app-abc123.js", http.StatusOK, "console.log", "public, max-age=31536000, immutable", "javascript"},
		{"/assets/app-abc123.css", http.StatusOK, ":root", "public, max-age=31536000, immutable", "text/css"},
		{"/favicon.svg", http.StatusOK, "<svg", "no-cache", "image/svg"},
		// SPA fallback: a client-side route reloads to the app shell.
		{"/instances", http.StatusOK, "id=root", "no-cache", "text/html"},
		{"/keys/deep/route", http.StatusOK, "id=root", "no-cache", "text/html"},
		// A missing file with an extension is a real 404.
		{"/assets/missing.js", http.StatusNotFound, "", "", ""},
		{"/logo.png", http.StatusNotFound, "", "", ""},
	}
	for _, tt := range tests {
		t.Run(tt.path, func(t *testing.T) {
			w := get(t, h, tt.path)
			if w.Code != tt.wantStatus {
				t.Fatalf("status = %d, want %d", w.Code, tt.wantStatus)
			}
			if tt.wantBody != "" && !strings.Contains(w.Body.String(), tt.wantBody) {
				t.Errorf("body %q does not contain %q", w.Body.String(), tt.wantBody)
			}
			if tt.wantCache != "" && w.Header().Get("Cache-Control") != tt.wantCache {
				t.Errorf("cache-control = %q, want %q", w.Header().Get("Cache-Control"), tt.wantCache)
			}
			if tt.wantCTPart != "" && !strings.Contains(w.Header().Get("Content-Type"), tt.wantCTPart) {
				t.Errorf("content-type = %q, want it to contain %q", w.Header().Get("Content-Type"), tt.wantCTPart)
			}
		})
	}
}

func TestPathTraversalStaysInsideFS(t *testing.T) {
	h := HandlerFromFS(buildFS())
	// path.Clean collapses a traversal to a path inside the FS, and
	// http.ServeFileFS additionally rejects any raw ".." path with 400,
	// so no request escapes the embedded build.
	tests := []struct {
		path string
		want int
	}{
		{"/../../etc/passwd", http.StatusBadRequest}, // fallback hits ServeFileFS's dotdot check
		{"/../secret.js", http.StatusNotFound},       // cleaned to secret.js, which does not exist
	}
	for _, tt := range tests {
		w := get(t, h, tt.path)
		if w.Code != tt.want {
			t.Fatalf("%s: status = %d, want %d", tt.path, w.Code, tt.want)
		}
	}
}

func TestMethodNotAllowed(t *testing.T) {
	h := HandlerFromFS(buildFS())
	w := httptest.NewRecorder()
	h.ServeHTTP(w, httptest.NewRequest(http.MethodPost, "/", nil))
	if w.Code != http.StatusMethodNotAllowed {
		t.Fatalf("status = %d, want 405", w.Code)
	}
}

func TestMissingBuildServesPlaceholder(t *testing.T) {
	// Tolerated at test time only: an FS without index.html answers 503
	// and says how to build, instead of panicking or serving nothing.
	h := HandlerFromFS(fstest.MapFS{})
	w := get(t, h, "/")
	if w.Code != http.StatusServiceUnavailable {
		t.Fatalf("status = %d, want 503", w.Code)
	}
	if !strings.Contains(w.Body.String(), "npm") {
		t.Errorf("placeholder does not say how to build: %q", w.Body.String())
	}
}

func TestEmbeddedBuild(t *testing.T) {
	assets, ok := web.Dist()
	if !ok {
		t.Skip("no embedded build (bento_noweb)")
	}
	w := get(t, HandlerFromFS(assets), "/")
	if w.Code != http.StatusOK {
		t.Fatalf("embedded index.html: status = %d", w.Code)
	}
	if !strings.Contains(w.Header().Get("Content-Type"), "text/html") {
		t.Errorf("content-type = %q", w.Header().Get("Content-Type"))
	}
	// Handler() must agree with HandlerFromFS(web.Dist()).
	w = get(t, Handler(), "/")
	if w.Code != http.StatusOK {
		t.Fatalf("Handler(): status = %d", w.Code)
	}
}

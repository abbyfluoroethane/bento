// Package dashboard serves the built web dashboard (SPEC section 14).
// The assets are a single-page application built in web/ and embedded at
// compile time; the deployed artifact stays one Go binary with no Node
// runtime (SPEC 14.1). The control plane mounts this handler at / and the
// API at /api/, so this handler never sees an API request.
package dashboard

import (
	"io/fs"
	"net/http"
	"path"
	"strings"

	"github.com/abbyfluoroethane/bento/web"
)

// Handler serves the embedded dashboard build. When no build is embedded
// (the bento_noweb tag, a test environment) it serves a plain placeholder
// that says how to build the assets.
func Handler() http.Handler {
	assets, ok := web.Dist()
	if !ok {
		return placeholder()
	}
	return HandlerFromFS(assets)
}

// HandlerFromFS serves a dashboard build from any fs.FS. Paths that name
// a file are served as-is; every other path falls back to index.html so
// client-side routes survive a reload. Hashed assets under assets/ are
// immutable and cached for a year; index.html is revalidated on every
// load so a new deploy takes effect at once.
func HandlerFromFS(assets fs.FS) http.Handler {
	if _, err := fs.Stat(assets, "index.html"); err != nil {
		return placeholder()
	}
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Method != http.MethodGet && r.Method != http.MethodHead {
			http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
			return
		}
		p := strings.TrimPrefix(path.Clean("/"+r.URL.Path), "/")
		if p == "" {
			p = "index.html"
		}
		if info, err := fs.Stat(assets, p); err == nil && !info.IsDir() {
			if strings.HasPrefix(p, "assets/") {
				w.Header().Set("Cache-Control", "public, max-age=31536000, immutable")
			} else {
				w.Header().Set("Cache-Control", "no-cache")
			}
			http.ServeFileFS(w, r, assets, p)
			return
		}
		// A path with an extension names a missing file: a real 404. A
		// path without one is a client-side route: serve the app shell.
		if path.Ext(p) != "" {
			http.NotFound(w, r)
			return
		}
		w.Header().Set("Cache-Control", "no-cache")
		http.ServeFileFS(w, r, assets, "index.html")
	})
}

// placeholder answers when no dashboard build is embedded. It returns
// 503: the API still works, the UI is what is unavailable. This is not
// the proxy's instance error page of SPEC 9.3/14.5 — that page belongs
// to the HTTP proxy, never to this package.
func placeholder() http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "text/html; charset=utf-8")
		w.WriteHeader(http.StatusServiceUnavailable)
		_, _ = w.Write([]byte(`<!doctype html><title>Bento</title>` +
			`<p>The dashboard assets are not embedded in this build. ` +
			`Run <code>npm install &amp;&amp; npm run build</code> in <code>web/</code> ` +
			`and rebuild without the <code>bento_noweb</code> tag.</p>`))
	})
}

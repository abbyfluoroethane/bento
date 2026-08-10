//go:build !bento_noweb

// Package web embeds the built dashboard assets. The Vite build writes to
// web/dist (committed, per the repository README), and go:embed can only
// reach files under its own package directory, so the embed directive
// lives here; internal/dashboard serves these assets over HTTP.
package web

import (
	"embed"
	"io/fs"
)

//go:embed all:dist
var dist embed.FS

// Dist returns the built dashboard assets rooted at the dist directory.
// The boolean reports whether a build is embedded; with the bento_noweb
// build tag it is always false.
func Dist() (fs.FS, bool) {
	sub, err := fs.Sub(dist, "dist")
	if err != nil {
		return nil, false
	}
	return sub, true
}

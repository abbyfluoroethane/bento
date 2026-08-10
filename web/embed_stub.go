//go:build bento_noweb

package web

import "io/fs"

// Dist reports that no dashboard build is embedded. The bento_noweb build
// tag exists so the Go packages compile and test in an environment that
// has no Node build; the real build always embeds the real assets.
func Dist() (fs.FS, bool) { return nil, false }

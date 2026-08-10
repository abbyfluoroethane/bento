package main

import "testing"

// The default config path does not exist on a development machine, so
// every real command fails with exit 1 after config load; dispatch
// errors exit 2.
func TestRunDispatch(t *testing.T) {
	tests := []struct {
		name string
		args []string
		want int
	}{
		{"no arguments", nil, 2},
		{"unknown command", []string{"frobnicate"}, 2},
		{"serve without config", []string{"serve"}, 1},
		{"proxy without config", []string{"proxy"}, 1},
		{"sshd without config", []string{"sshd"}, 1},
		{"fetch-images without config", []string{"fetch-images"}, 1},
		{"reconcile without config", []string{"reconcile"}, 1},
		{"dump-db without config", []string{"dump-db"}, 1},
		{"images without config", []string{"images"}, 1},
		{"config flag before command", []string{"-config", "/nonexistent/x.toml", "serve"}, 1},
		{"bad flag", []string{"-nope"}, 2},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			if got := run(tt.args); got != tt.want {
				t.Errorf("run(%v) = %d, want %d", tt.args, got, tt.want)
			}
		})
	}
}

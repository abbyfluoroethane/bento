package main

// SSH key material for the frontend (SPEC 10): one host key every
// connection sees, and one keypair the frontend uses to authenticate to
// sshd inside the guests. Both are ed25519, generated on first use, and
// stored under the operator key directory.

import (
	"crypto/ed25519"
	"crypto/rand"
	"encoding/pem"
	"fmt"
	"os"
	"path/filepath"
	"strings"

	gossh "golang.org/x/crypto/ssh"
)

const (
	hostKeyFile     = "ssh_host_ed25519_key"
	frontendKeyFile = "frontend_ed25519_key"
)

// keyPath resolves a key file inside the operator key directory.
func keyPath(a *app, file string) string {
	return filepath.Join(a.cfg.KeyDir, file)
}

// ensureKey loads the OpenSSH private key at path, generating an
// ed25519 key (with a .pub sibling) when the file does not exist yet.
func ensureKey(path, comment string) (gossh.Signer, error) {
	data, err := os.ReadFile(path)
	switch {
	case err == nil:
		signer, err := gossh.ParsePrivateKey(data)
		if err != nil {
			return nil, fmt.Errorf("parse %s: %w", path, err)
		}
		return signer, nil
	case !os.IsNotExist(err):
		return nil, err
	}

	if err := os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
		return nil, err
	}
	pub, priv, err := ed25519.GenerateKey(rand.Reader)
	if err != nil {
		return nil, err
	}
	block, err := gossh.MarshalPrivateKey(priv, comment)
	if err != nil {
		return nil, err
	}
	if err := os.WriteFile(path, pem.EncodeToMemory(block), 0o600); err != nil {
		return nil, err
	}
	sshPub, err := gossh.NewPublicKey(pub)
	if err != nil {
		return nil, err
	}
	pubLine := authorizedKeyLine(sshPub, comment)
	if err := os.WriteFile(path+".pub", []byte(pubLine+"\n"), 0o644); err != nil {
		return nil, err
	}
	return gossh.NewSignerFromKey(priv)
}

// authorizedKeyLine renders a public key in authorized_keys format with
// a comment.
func authorizedKeyLine(pub gossh.PublicKey, comment string) string {
	line := strings.TrimSpace(string(gossh.MarshalAuthorizedKey(pub)))
	if comment != "" {
		line += " " + comment
	}
	return line
}

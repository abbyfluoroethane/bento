package network

import (
	"regexp"
	"strconv"
	"testing"
)

var macFormat = regexp.MustCompile(`^[0-9a-f]{2}(:[0-9a-f]{2}){5}$`)

func TestMACFormat(t *testing.T) {
	uuids := []string{
		"8f9c3a1e-2b4d-4e6f-8a9b-0c1d2e3f4a5b",
		"00000000-0000-0000-0000-000000000000",
		"another-identity",
	}
	for _, uuid := range uuids {
		t.Run(uuid, func(t *testing.T) {
			mac := MAC(uuid)
			if !macFormat.MatchString(mac) {
				t.Fatalf("MAC(%q) = %q, not colon-separated lowercase hex", uuid, mac)
			}
			first, err := strconv.ParseUint(mac[:2], 16, 8)
			if err != nil {
				t.Fatal(err)
			}
			if first&0x01 != 0 {
				t.Errorf("MAC(%q) = %q: multicast bit set, want unicast", uuid, mac)
			}
			if first&0x02 == 0 {
				t.Errorf("MAC(%q) = %q: locally administered bit not set", uuid, mac)
			}
		})
	}
}

func TestMACDeterministic(t *testing.T) {
	const uuid = "8f9c3a1e-2b4d-4e6f-8a9b-0c1d2e3f4a5b"
	if MAC(uuid) != MAC(uuid) {
		t.Error("MAC is not deterministic for the same UUID")
	}
	if MAC(uuid) == MAC("a different uuid") {
		t.Error("MAC collides for different UUIDs")
	}
}

func TestMACGolden(t *testing.T) {
	// Pin the derivation: a change here silently changes the MAC of every
	// existing instance and breaks guest interface-name stability.
	const uuid = "8f9c3a1e-2b4d-4e6f-8a9b-0c1d2e3f4a5b"
	const want = "ba:c9:e6:f9:7c:2b"
	if got := MAC(uuid); got != want {
		t.Errorf("MAC(%q) = %q, want %q; the derivation must never change", uuid, got, want)
	}
}

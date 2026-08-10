package network

import (
	"crypto/sha256"
	"fmt"
)

// MAC returns the MAC address for an instance. Bento assigns the MAC;
// libvirt never generates it (SPEC 5, 6.2). The address is derived
// deterministically from the instance UUID, the identity of an instance
// (SPEC 7.2), so it is stable across restarts and domain redefines. A
// fixed MAC keeps the interface name stable in the guest.
//
// The address is in the locally administered unicast range: the local
// bit of the first octet is set and the multicast bit is clear.
func MAC(instanceUUID string) string {
	sum := sha256.Sum256([]byte("bento-mac:" + instanceUUID))
	b := sum[:6]
	b[0] = b[0]&^0x01 | 0x02 // unicast, locally administered
	return fmt.Sprintf("%02x:%02x:%02x:%02x:%02x:%02x", b[0], b[1], b[2], b[3], b[4], b[5])
}

// Command mdns-publisher is a minimal, RFC 6762 compliant mDNS hostname
// responder.
//
// Lifecycle:
//
//  1. Probe (§8.1) — 3 ANY queries 250ms apart, QU bit set, with our
//     proposed records in the Authority section for tiebreaking (§8.2).
//  2. Announce (§8.3) — 2 unsolicited responses 1s apart, with the
//     cache-flush bit set on unique records (§10.2).
//  3. Reactive — answer incoming queries via hashicorp/mdns.
//  4. Goodbye (§10.1) — on shutdown, send 2 responses with TTL=0
//     records to invalidate peer caches.
//
// Intentionally tiny: no service discovery (DNS-SD), no reflection
// between interfaces, no dbus, no extra daemons.
package main

import (
	"flag"
	"fmt"
	"log"
	"math/rand"
	"net"
	"os"
	"os/signal"
	"strings"
	"syscall"
	"time"

	"github.com/hashicorp/mdns"
	"github.com/miekg/dns"
	"github.com/vishvananda/netlink"
	"golang.org/x/net/ipv4"
	"golang.org/x/net/ipv6"
)

const (
	// RFC 6762 §10.2 cache-flush bit (top bit of rrclass).
	cacheFlushBit uint16 = 0x8000
	// RFC 6762 §5.4 QU bit (top bit of qclass in question).
	quBit uint16 = 0x8000

	probeCount       = 3
	probeInterval    = 250 * time.Millisecond
	probeInitialMax  = 250 * time.Millisecond
	probeTiebreakTTL = 255 // mDNSResponder/avahi convention

	announceCount    = 2
	announceInterval = 1 * time.Second

	goodbyeCount    = 2
	goodbyeInterval = 100 * time.Millisecond

	// RFC 6762 §6.7: TTL in legacy unicast responses should not exceed 10s.
	legacyUnicastMaxTTL = 10

	maxLabelLen = 63 // RFC 1035 §2.3.4
	maxTTL      = 86400
)

var (
	mdnsAddr4 = &net.UDPAddr{IP: net.ParseIP("224.0.0.251"), Port: 5353}
	mdnsAddr6 = &net.UDPAddr{IP: net.ParseIP("ff02::fb"), Port: 5353}
)

// hostnameZone implements hashicorp/mdns's `Zone` interface. The records
// passed in must already have the cache-flush bit set in their Class
// field (RFC 6762 §10.2). For QU queries we strip the bit and cap the
// TTL (RFC 6762 §6.7).
type hostnameZone struct {
	records []dns.RR
}

func (z *hostnameZone) Records(q dns.Question) []dns.RR {
	qu := q.Qclass&quBit != 0
	var out []dns.RR
	for _, r := range z.records {
		h := r.Header()
		if h.Name != q.Name {
			continue
		}
		if q.Qtype != dns.TypeANY && h.Rrtype != q.Qtype {
			continue
		}
		rCopy := dns.Copy(r)
		if qu {
			// §6.7: legacy unicast responses MUST NOT have the
			// cache-flush bit, and SHOULD have TTL ≤ 10s.
			rCopy.Header().Class = dns.ClassINET
			if rCopy.Header().Ttl > legacyUnicastMaxTTL {
				rCopy.Header().Ttl = legacyUnicastMaxTTL
			}
		}
		out = append(out, rCopy)
	}
	return out
}

func isValidLabel(s string) bool {
	if len(s) == 0 || len(s) > maxLabelLen {
		return false
	}
	for _, r := range s {
		switch {
		case r >= 'a' && r <= 'z':
		case r >= 'A' && r <= 'Z':
		case r >= '0' && r <= '9':
		case r == '-':
		default:
			return false
		}
	}
	return true
}

func main() {
	ifaceName := flag.String("iface", "", "interface to bind mDNS to (required)")
	hostnameOverride := flag.String("hostname", "", "hostname (without .local) to advertise; empty = system hostname")
	ttl := flag.Uint("ttl", 120, "TTL in seconds for A/AAAA records (1-86400)")
	refresh := flag.Duration("refresh", 60*time.Second, "alive log interval")
	skipProbe := flag.Bool("skip-probe", false, "skip RFC 6762 §8.1 probing (NOT compliant)")
	skipAnnounce := flag.Bool("skip-announce", false, "skip RFC 6762 §8.3 announcing")
	skipGoodbye := flag.Bool("skip-goodbye", false, "skip RFC 6762 §10.1 goodbye on shutdown")
	flag.Parse()

	if *ifaceName == "" {
		log.Fatalf("--iface is required")
	}
	if *ttl == 0 || *ttl > maxTTL {
		log.Fatalf("--ttl must be in (0, %d]", maxTTL)
	}

	hostname := *hostnameOverride
	if hostname == "" {
		h, err := os.Hostname()
		if err != nil {
			log.Fatalf("hostname: %v", err)
		}
		hostname = h
	}
	if i := strings.IndexByte(hostname, '.'); i >= 0 {
		hostname = hostname[:i]
	}
	if !isValidLabel(hostname) {
		log.Fatalf("hostname %q invalid: must be 1-%d chars of [A-Za-z0-9-]", hostname, maxLabelLen)
	}

	iface, err := net.InterfaceByName(*ifaceName)
	if err != nil {
		log.Fatalf("interface %s: %v", *ifaceName, err)
	}

	ips, err := getInterfaceIPs(iface)
	if err != nil {
		log.Fatalf("addrs: %v", err)
	}

	fqdn := dns.Fqdn(hostname + ".local")
	uintTTL := uint32(*ttl)

	// Build records with cache-flush bit set (RFC 6762 §10.2).
	var records []dns.RR
	for _, ip := range ips {
		if ip == nil || ip.IsLinkLocalUnicast() || ip.IsLoopback() {
			continue
		}
		if v4 := ip.To4(); v4 != nil {
			records = append(records, &dns.A{
				Hdr: dns.RR_Header{
					Name:   fqdn,
					Rrtype: dns.TypeA,
					Class:  dns.ClassINET | cacheFlushBit,
					Ttl:    uintTTL,
				},
				A: v4,
			})
		} else {
			records = append(records, &dns.AAAA{
				Hdr: dns.RR_Header{
					Name:   fqdn,
					Rrtype: dns.TypeAAAA,
					Class:  dns.ClassINET | cacheFlushBit,
					Ttl:    uintTTL,
				},
				AAAA: ip,
			})
		}
	}
	if len(records) == 0 {
		log.Fatalf("no usable global unicast addresses on %s", *ifaceName)
	}

	log.Printf("publishing %s on %s with %d record(s):", fqdn, *ifaceName, len(records))
	for _, r := range records {
		log.Printf("  %s", r.String())
	}

	// Open sender sockets for probe/announce/goodbye.
	// hashicorp/mdns doesn't expose sending, so we do it ourselves.
	sendConn4, sendConn6, err := openSenderSockets(iface)
	if err != nil {
		log.Fatalf("sender sockets: %v", err)
	}
	if sendConn4 != nil {
		defer sendConn4.Close()
	}
	if sendConn6 != nil {
		defer sendConn6.Close()
	}
	if sendConn4 == nil && sendConn6 == nil {
		log.Fatalf("no mDNS send sockets available")
	}

	// RFC 6762 §8.1: Probing.
	if !*skipProbe {
		runProbe(fqdn, records, sendConn4, sendConn6)
	}

	// RFC 6762 §8.3: Announcing.
	if !*skipAnnounce {
		runAnnounce(fqdn, records, sendConn4, sendConn6)
	}

	// Start the reactive server.
	zone := &hostnameZone{records: records}
	server, err := mdns.NewServer(&mdns.Config{
		Zone:  zone,
		Iface: iface,
	})
	if err != nil {
		log.Fatalf("server: %v", err)
	}

	log.Printf("server running; waiting for signal")

	// Liveness ticker.
	livenessDone := make(chan struct{})
	go func() {
		defer close(livenessDone)
		t := time.NewTicker(*refresh)
		defer t.Stop()
		for {
			select {
			case <-t.C:
				log.Printf("alive (%d record(s) for %s)", len(records), fqdn)
			}
		}
	}()

	// Wait for signal.
	sigCh := make(chan os.Signal, 1)
	signal.Notify(sigCh, syscall.SIGINT, syscall.SIGTERM)
	sig := <-sigCh
	log.Printf("received %s", sig)

	// RFC 6762 §10.1: Goodbye.
	if !*skipGoodbye {
		runGoodbye(fqdn, records, sendConn4, sendConn6)
	}

	// Stop server, then wait for liveness goroutine.
	if err := server.Shutdown(); err != nil {
		log.Printf("server shutdown: %v", err)
	}
	<-livenessDone
	log.Printf("shutting down")
}

// getInterfaceIPs returns the IP addresses of iface using vishvananda/netlink.
// This library is used instead of net.Interface.Addrs() because the Go
// stdlib has known issues parsing netlink responses on certain
// bridge/VRF/netns configurations (e.g. "netlinkrib: address family
// not supported by protocol"); netlink (used by Docker, Kubernetes,
// etc.) handles those cases robustly. We pass family=0 to get both
// AF_INET and AF_INET6 addresses.
func getInterfaceIPs(iface *net.Interface) ([]net.IP, error) {
	link, err := netlink.LinkByName(iface.Name)
	if err != nil {
		return nil, fmt.Errorf("netlink.LinkByName(%s): %w", iface.Name, err)
	}
	addrs, err := netlink.AddrList(link, 0)
	if err != nil {
		return nil, fmt.Errorf("netlink.AddrList(%s): %w", iface.Name, err)
	}
	var ips []net.IP
	for _, a := range addrs {
		if a.IPNet == nil || a.IPNet.IP == nil {
			continue
		}
		ips = append(ips, a.IPNet.IP)
	}
	return ips, nil
}

// openSenderSockets creates outbound multicast sockets scoped to iface.
// Multicast on Linux requires IP_MULTICAST_IF (v4) / IPV6_MULTICAST_IF (v6)
// — without these, packets would go out the kernel's default interface
// (which on a router is usually ppp0, not br-lan).
func openSenderSockets(iface *net.Interface) (v4, v6 *net.UDPConn, err error) {
	if c, e := net.DialUDP("udp4", nil, mdnsAddr4); e == nil {
		if e := ipv4.NewPacketConn(c).SetMulticastInterface(iface); e != nil {
			c.Close()
			log.Printf("WARN: set IPv4 multicast iface: %v", e)
		} else {
			v4 = c
		}
	} else {
		log.Printf("WARN: IPv4 mDNS send socket: %v", e)
	}
	if c, e := net.DialUDP("udp6", nil, mdnsAddr6); e == nil {
		if e := ipv6.NewPacketConn(c).SetMulticastInterface(iface); e != nil {
			c.Close()
			log.Printf("WARN: set IPv6 multicast iface: %v", e)
		} else {
			v6 = c
		}
	} else {
		log.Printf("WARN: IPv6 mDNS send socket: %v", e)
	}
	return
}

// runProbe implements RFC 6762 §8.1. Three ANY queries 250ms apart with
// the QU bit set, carrying our proposed records in the Authority section
// for §8.2 tiebreaking. We do not actively detect conflicts (any response
// would have to come from a third party, since the reactive server is not
// yet running); in case of conflict smartdns may receive multiple answers.
func runProbe(name string, ourRecords []dns.RR, conn4, conn6 *net.UDPConn) {
	log.Printf("probing for %s", name)

	// Random initial delay 0-250ms (§8.1: guards against synchronised
	// power-on storms).
	time.Sleep(time.Duration(rand.Int63n(int64(probeInitialMax))))

	for i := 0; i < probeCount; i++ {
		if i > 0 {
			time.Sleep(probeInterval)
		}
		msg := new(dns.Msg)
		msg.SetQuestion(name, dns.TypeANY)
		msg.Question[0].Qclass = dns.ClassINET | quBit
		msg.Id = 0
		// Authority section: our proposed records with TTL 255
		// (mDNSResponder/avahi convention) for tiebreaking.
		msg.Authoritative = true
		for _, r := range ourRecords {
			rCopy := dns.Copy(r)
			rCopy.Header().Ttl = probeTiebreakTTL
			msg.Ns = append(msg.Ns, rCopy)
		}
		sendMsg(msg, conn4, conn6)
		log.Printf("sent probe %d/%d for %s", i+1, probeCount, name)
	}
	log.Printf("probe complete for %s", name)
}

// runAnnounce implements RFC 6762 §8.3. Unsolicited responses with the
// cache-flush bit set on the (already-claimed) unique records. At least
// 2 announcements, 1 second apart.
func runAnnounce(name string, records []dns.RR, conn4, conn6 *net.UDPConn) {
	log.Printf("announcing %s", name)
	for i := 0; i < announceCount; i++ {
		if i > 0 {
			time.Sleep(announceInterval)
		}
		msg := new(dns.Msg)
		msg.MsgHdr.Response = true
		msg.MsgHdr.Authoritative = true
		msg.Id = 0
		msg.Answer = make([]dns.RR, 0, len(records))
		for _, r := range records {
			msg.Answer = append(msg.Answer, dns.Copy(r))
		}
		sendMsg(msg, conn4, conn6)
		log.Printf("sent announcement %d/%d for %s", i+1, announceCount, name)
	}
}

// runGoodbye implements RFC 6762 §10.1. Two responses with TTL=0 records,
// 100ms apart, to invalidate peer caches within ~1s.
func runGoodbye(name string, records []dns.RR, conn4, conn6 *net.UDPConn) {
	log.Printf("sending goodbye for %s", name)
	gbRecs := make([]dns.RR, 0, len(records))
	for _, r := range records {
		rCopy := dns.Copy(r)
		rCopy.Header().Ttl = 0
		gbRecs = append(gbRecs, rCopy)
	}
	for i := 0; i < goodbyeCount; i++ {
		if i > 0 {
			time.Sleep(goodbyeInterval)
		}
		msg := new(dns.Msg)
		msg.MsgHdr.Response = true
		msg.MsgHdr.Authoritative = true
		msg.Id = 0
		msg.Answer = gbRecs
		sendMsg(msg, conn4, conn6)
	}
	log.Printf("goodbye sent for %s", name)
}

func sendMsg(msg *dns.Msg, conn4, conn6 *net.UDPConn) {
	buf, err := msg.Pack()
	if err != nil {
		log.Printf("WARN: pack mDNS message: %v", err)
		return
	}
	if conn4 != nil {
		if _, err := conn4.Write(buf); err != nil {
			log.Printf("WARN: send mDNS (v4): %v", err)
		}
	}
	if conn6 != nil {
		if _, err := conn6.Write(buf); err != nil {
			log.Printf("WARN: send mDNS (v6): %v", err)
		}
	}
}

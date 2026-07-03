// Command mdns-publisher is a minimal, RFC 6762 compliant mDNS hostname
// responder.
//
// Lifecycle:
//
//  1. Create one UDP socket per address family, bound to UDP port 5353 on
//     the chosen interface, joined to the mDNS multicast group.
//  2. Probe (§8.1) — 3 ANY queries 250 ms apart, QU bit set, with our
//     proposed records in the Authority section for tiebreaking (§8.2).
//     The recv loop is already running so we can detect conflicting
//     responses during the probe window.
//  3. Announce (§8.3) — 2 unsolicited responses 1 s apart, with the
//     cache-flush bit set on unique records (§10.2).
//  4. Reactive — answer incoming A/AAAA/ANY queries via the same socket.
//     Legacy unicast queries (source port != 5353) are answered unicast
//     with the question echoed and cache-flush stripped (§6.7).  Known-
//     answer suppression (§7.1) and negative responses via NSEC (§6.1)
//     are implemented.
//  5. Goodbye (§10.1) — on shutdown, send TTL=0 records to invalidate
//     peer caches.
//
// Intentionally tiny: no service discovery (DNS-SD), no reflection
// between interfaces, no dbus, no extra daemons.
package main

import (
	"bytes"
	"flag"
	"fmt"
	"log"
	"math/rand"
	"net"
	"os"
	"os/signal"
	"sort"
	"strings"
	"sync"
	"sync/atomic"
	"syscall"
	"time"

	"github.com/miekg/dns"
	"github.com/vishvananda/netlink"
	"golang.org/x/net/ipv4"
	"golang.org/x/net/ipv6"
)

const (
	mdnsPort = 5353

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
	mdnsAddr4 = &net.UDPAddr{IP: net.ParseIP("224.0.0.251"), Port: mdnsPort}
	mdnsAddr6 = &net.UDPAddr{IP: net.ParseIP("ff02::fb"), Port: mdnsPort}
	mdnsGroup4 = net.ParseIP("224.0.0.251")
	mdnsGroup6 = net.ParseIP("ff02::fb")
)

// serverState controls what the recv loop does with incoming packets.
type serverState int32

const (
	statePending serverState = iota
	stateProbing
	stateRunning
)

// hostnameZone serves a fixed set of A/AAAA records and a negative-response
// NSEC record.  The records have the cache-flush bit set in their Class field
// (RFC 6762 §10.2).  For legacy unicast queries (source port != 5353) we strip
// the bit and cap the TTL (§6.7).
type hostnameZone struct {
	records []dns.RR
	nsec    dns.RR
}

func (z *hostnameZone) Records(q dns.Question, legacy bool) []dns.RR {
	qclass := q.Qclass &^ quBit
	if qclass != dns.ClassINET && qclass != dns.ClassANY {
		return nil
	}

	var out []dns.RR
	for _, r := range z.records {
		h := r.Header()
		if !strings.EqualFold(h.Name, q.Name) {
			continue
		}
		if q.Qtype != dns.TypeANY && h.Rrtype != q.Qtype {
			continue
		}
		rCopy := dns.Copy(r)
		if legacy {
			// §6.7: legacy unicast responses MUST NOT have the
			// cache-flush bit.  TTL capping is applied later in
			// answerQuery, after known-answer suppression, so that
			// filterKnownAnswers compares against the true TTL
			// (RFC 6762 §7.1).
			rCopy.Header().Class = dns.ClassINET
		}
		out = append(out, rCopy)
	}

	// Negative response (§6.1): if we own the name but have no record of the
	// requested type, return an NSEC record indicating which types exist.
	if len(out) == 0 && q.Qtype != dns.TypeANY && z.nsec != nil &&
		strings.EqualFold(z.nsec.Header().Name, q.Name) &&
		q.Qclass&^quBit == dns.ClassINET {
		rCopy := dns.Copy(z.nsec)
		if legacy {
			rCopy.Header().Class = dns.ClassINET
		}
		out = append(out, rCopy)
	}

	return out
}

// filterKnownAnswers removes records that the querier already knows with a
// TTL at least half the true TTL (RFC 6762 §7.1).
func (z *hostnameZone) filterKnownAnswers(records, known []dns.RR) []dns.RR {
	if len(known) == 0 || len(records) == 0 {
		return records
	}
	var out []dns.RR
	for _, r := range records {
		if !z.isKnownAnswer(r, known) {
			out = append(out, r)
		}
	}
	return out
}

func (z *hostnameZone) isKnownAnswer(r dns.RR, known []dns.RR) bool {
	rh := r.Header()
	for _, k := range known {
		kh := k.Header()
		if !strings.EqualFold(kh.Name, rh.Name) {
			continue
		}
		if kh.Rrtype != rh.Rrtype {
			continue
		}
		// Compare class ignoring cache-flush bit.
		if kh.Class&^cacheFlushBit != rh.Class&^cacheFlushBit {
			continue
		}
		if !rrdataEqual(k, r) {
			continue
		}
		if kh.Ttl >= rh.Ttl/2 {
			return true
		}
	}
	return false
}

func rrdataEqual(a, b dns.RR) bool {
	switch x := a.(type) {
	case *dns.A:
		y, ok := b.(*dns.A)
		return ok && x.A.Equal(y.A)
	case *dns.AAAA:
		y, ok := b.(*dns.AAAA)
		return ok && x.AAAA.Equal(y.AAAA)
	case *dns.NSEC:
		y, ok := b.(*dns.NSEC)
		return ok && strings.EqualFold(x.NextDomain, y.NextDomain) && nsecTypesEqual(x.TypeBitMap, y.TypeBitMap)
	}
	return false
}

// rrdataBytes returns the raw rdata of a record as bytes, for RFC 6762 §8.2
// tiebreaking comparison.
func rrdataBytes(r dns.RR) []byte {
	switch x := r.(type) {
	case *dns.A:
		return x.A.To4()
	case *dns.AAAA:
		return x.AAAA.To16()
	case *dns.NSEC:
		// For tiebreaking we only care about A/AAAA; NSEC is not expected in
		// probe Authority sections for a hostname-only responder.
		return nil
	}
	return nil
}

// rrLess implements the canonical ordering for RFC 6762 §8.2 tiebreaking:
// class (ignoring cache-flush bit), then type, then raw rdata.
func rrLess(a, b dns.RR) bool {
	ah := a.Header()
	bh := b.Header()
	aClass := ah.Class &^ cacheFlushBit
	bClass := bh.Class &^ cacheFlushBit
	if aClass != bClass {
		return aClass < bClass
	}
	if ah.Rrtype != bh.Rrtype {
		return ah.Rrtype < bh.Rrtype
	}
	return bytes.Compare(rrdataBytes(a), rrdataBytes(b)) < 0
}

func nsecTypesEqual(a, b []uint16) bool {
	if len(a) != len(b) {
		return false
	}
	m := make(map[uint16]struct{}, len(a))
	for _, t := range a {
		m[t] = struct{}{}
	}
	for _, t := range b {
		if _, ok := m[t]; !ok {
			return false
		}
	}
	return true
}

// buildNSEC creates the restricted NSEC record described in RFC 6762 §6.1.
func buildNSEC(name string, records []dns.RR, ttl uint32) dns.RR {
	typeSet := make(map[uint16]struct{})
	for _, r := range records {
		typeSet[r.Header().Rrtype] = struct{}{}
	}
	if len(typeSet) == 0 {
		return nil
	}
	types := make([]uint16, 0, len(typeSet))
	for t := range typeSet {
		types = append(types, t)
	}
	return &dns.NSEC{
		Hdr: dns.RR_Header{
			Name:   name,
			Rrtype: dns.TypeNSEC,
			Class:  dns.ClassINET | cacheFlushBit,
			Ttl:    ttl,
		},
		NextDomain: name,
		TypeBitMap: types,
	}
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

	// NB: we deliberately avoid net.InterfaceByName() / net.Interfaces().
	// Both go through Go stdlib's linkAddrTable(), which on this
	// br-lan (bridge + dynamic delegated IPv6 address) fails with
	// "netlinkrib: address family not supported by protocol" because
	// it cannot parse the netlink RTM_GETADDR response. vishvananda/netlink
	// handles those cases correctly, so we use it for everything.
	link, err := netlink.LinkByName(*ifaceName)
	if err != nil {
		log.Fatalf("netlink.LinkByName(%s): %v", *ifaceName, err)
	}
	attrs := link.Attrs()
	iface := &net.Interface{
		Index:        attrs.Index,
		Name:         attrs.Name,
		MTU:          attrs.MTU,
		HardwareAddr: attrs.HardwareAddr,
		Flags:        attrs.Flags,
	}

	ips, err := getInterfaceIPs(link)
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

	// Create a single socket per address family, bound to UDP 5353, joined
	// to the mDNS multicast group.  The same socket is used for sending
	// probes/announcements/goodbyes and for receiving queries/replies.
	socks, err := createSockets(iface)
	if err != nil {
		log.Fatalf("sockets: %v", err)
	}

	zone := &hostnameZone{
		records: records,
		nsec:    buildNSEC(fqdn, records, uintTTL),
	}

	server, err := newMDNSServer(zone, socks, fqdn)
	if err != nil {
		log.Fatalf("server: %v", err)
	}

	// RFC 6762 §8.1: Probing.  The recv loop is already running so it can
	// observe conflicting responses during the probe window.
	if !*skipProbe {
		if err := runProbe(server, records); err != nil {
			log.Fatalf("probe failed: %v", err)
		}
	}

	// Transition to reactive mode.
	atomic.StoreInt32(&server.state, int32(stateRunning))

	// RFC 6762 §8.3: Announcing.
	if !*skipAnnounce {
		runAnnounce(server, records)
	}

	log.Printf("server running; waiting for signal")

	// Liveness ticker.
	livenessDone := make(chan struct{})
	livenessQuit := make(chan struct{})
	go func() {
		defer close(livenessDone)
		t := time.NewTicker(*refresh)
		defer t.Stop()
		for {
			select {
			case <-t.C:
				log.Printf("alive (%d record(s) for %s)", len(records), fqdn)
			case <-livenessQuit:
				return
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
		runGoodbye(server, records)
	}

	// Stop server, then signal liveness goroutine to exit and wait for it.
	if err := server.Shutdown(); err != nil {
		log.Printf("server shutdown: %v", err)
	}
	close(livenessQuit)
	<-livenessDone
	log.Printf("shutting down")
}

// getInterfaceIPs returns the IP addresses of link using vishvananda/netlink.
// This library is used instead of net.Interface.Addrs() because the Go
// stdlib has known issues parsing netlink responses on certain
// bridge/VRF/netns configurations (e.g. "netlinkrib: address family
// not supported by protocol"); netlink (used by Docker, Kubernetes,
// etc.) handles those cases robustly. We pass family=0 to get both
// AF_INET and AF_INET6 addresses.
func getInterfaceIPs(link netlink.Link) ([]net.IP, error) {
	addrs, err := netlink.AddrList(link, 0)
	if err != nil {
		return nil, fmt.Errorf("netlink.AddrList(%s): %w", link.Attrs().Name, err)
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

// mdnsSockets holds the per-family UDP sockets used for mDNS.
type mdnsSockets struct {
	v4 *net.UDPConn
	v6 *net.UDPConn
}

// createSockets creates one UDP socket per address family, bound to :5353 on
// the given interface, joined to the mDNS multicast group.  We use
// golang.org/x/net/ipv{4,6} for the multicast setup; it uses the interface
// index and does not call ifi.Addrs(), so it works on the problematic bridge
// interface that breaks net.ListenMulticastUDP.
//
// RFC 6762 §6: the source UDP port in all Multicast DNS responses MUST be
// 5353, and receivers MUST silently ignore responses from any other source
// port.  Binding our send+receive socket to 5353 ensures compliance.
// RFC 6762 §11: all mDNS messages SHOULD be sent with IP TTL / hop-limit 255.
func createSockets(iface *net.Interface) (*mdnsSockets, error) {
	var socks mdnsSockets
	var err4, err6 error

	laddr4 := &net.UDPAddr{IP: net.IPv4zero, Port: mdnsPort}
	c4, err4 := net.ListenUDP("udp4", laddr4)
	if err4 == nil {
		pc4 := ipv4.NewPacketConn(c4)
		if err := pc4.SetMulticastInterface(iface); err != nil {
			c4.Close()
			err4 = fmt.Errorf("set v4 multicast interface: %w", err)
		} else if err := pc4.SetMulticastTTL(255); err != nil {
			c4.Close()
			err4 = fmt.Errorf("set v4 multicast ttl: %w", err)
		} else if err := pc4.JoinGroup(iface, &net.IPAddr{IP: mdnsGroup4}); err != nil {
			c4.Close()
			err4 = fmt.Errorf("join v4 multicast group: %w", err)
		} else {
			socks.v4 = c4
		}
	}
	if socks.v4 == nil && err4 != nil {
		log.Printf("WARN: IPv4 mDNS socket: %v", err4)
	}

	laddr6 := &net.UDPAddr{IP: net.IPv6unspecified, Port: mdnsPort}
	c6, err6 := net.ListenUDP("udp6", laddr6)
	if err6 == nil {
		pc6 := ipv6.NewPacketConn(c6)
		if err := pc6.SetMulticastInterface(iface); err != nil {
			c6.Close()
			err6 = fmt.Errorf("set v6 multicast interface: %w", err)
		} else if err := pc6.SetMulticastHopLimit(255); err != nil {
			c6.Close()
			err6 = fmt.Errorf("set v6 multicast hop limit: %w", err)
		} else if err := pc6.JoinGroup(iface, &net.IPAddr{IP: mdnsGroup6}); err != nil {
			c6.Close()
			err6 = fmt.Errorf("join v6 multicast group: %w", err)
		} else {
			socks.v6 = c6
		}
	}
	if socks.v6 == nil && err6 != nil {
		log.Printf("WARN: IPv6 mDNS socket: %v", err6)
	}

	if socks.v4 == nil && socks.v6 == nil {
		return nil, fmt.Errorf("no mDNS sockets available (v4: %v, v6: %v)", err4, err6)
	}
	return &socks, nil
}

// mdnsServer is a minimal mDNS responder.  One recv goroutine per socket
// handles incoming packets; the same sockets are used for outbound
// probe/announce/goodbye traffic.
type mdnsServer struct {
	zone             *hostnameZone
	socks            *mdnsSockets
	shutdown         int32
	state            int32 // serverState
	probeName        string
	conflictDetected int32
	wg               sync.WaitGroup
}

func newMDNSServer(zone *hostnameZone, socks *mdnsSockets, probeName string) (*mdnsServer, error) {
	if socks.v4 == nil && socks.v6 == nil {
		return nil, fmt.Errorf("no mDNS sockets available")
	}
	s := &mdnsServer{
		zone:      zone,
		socks:     socks,
		state:     int32(statePending),
		probeName: probeName,
	}
	if socks.v4 != nil {
		s.wg.Add(1)
		go func() { defer s.wg.Done(); s.recv(socks.v4) }()
	}
	if socks.v6 != nil {
		s.wg.Add(1)
		go func() { defer s.wg.Done(); s.recv(socks.v6) }()
	}
	return s, nil
}

// recv reads mDNS packets from conn and dispatches them.
func (s *mdnsServer) recv(conn *net.UDPConn) {
	buf := make([]byte, 65536)
	for atomic.LoadInt32(&s.shutdown) == 0 {
		n, from, err := conn.ReadFromUDP(buf)
		if err != nil {
			if atomic.LoadInt32(&s.shutdown) != 0 {
				return
			}
			log.Printf("WARN: mDNS recv: %v", err)
			time.Sleep(10 * time.Millisecond)
			continue
		}
		s.handlePacket(buf[:n], from, conn)
	}
}

// handlePacket parses an mDNS packet and dispatches based on server state.
func (s *mdnsServer) handlePacket(data []byte, from *net.UDPAddr, conn *net.UDPConn) {
	state := atomic.LoadInt32(&s.state)
	if state == int32(statePending) {
		return
	}

	var msg dns.Msg
	if err := msg.Unpack(data); err != nil {
		return
	}
	if msg.Opcode != dns.OpcodeQuery {
		return
	}
	if msg.Rcode != 0 {
		return
	}
	if msg.Truncated {
		return
	}

	if state == int32(stateProbing) {
		if msg.Response {
			s.checkConflict(&msg)
		} else {
			// Simultaneous probe tiebreaking (RFC 6762 §8.2).
			s.handleProbeTiebreak(&msg)
		}
		return
	}

	// stateRunning: answer queries, ignore responses.
	if !msg.Response {
		s.answerQuery(&msg, from, conn)
	}
}

// checkConflict records a conflict if any response contains a record for the
// name we are probing.  RFC 6762 §8.1: any answer containing a record with
// the probed name, of any type, is a conflicting response.
func (s *mdnsServer) checkConflict(msg *dns.Msg) {
	for _, rr := range msg.Answer {
		if strings.EqualFold(rr.Header().Name, s.probeName) {
			atomic.StoreInt32(&s.conflictDetected, 1)
			return
		}
	}
	for _, rr := range msg.Ns {
		if strings.EqualFold(rr.Header().Name, s.probeName) {
			atomic.StoreInt32(&s.conflictDetected, 1)
			return
		}
	}
	for _, rr := range msg.Extra {
		if strings.EqualFold(rr.Header().Name, s.probeName) {
			atomic.StoreInt32(&s.conflictDetected, 1)
			return
		}
	}
}

// handleProbeTiebreak examines another host's probe query and applies RFC 6762
// §8.2 simultaneous-probe tiebreaking.  If the other host's records win, a
// conflict is recorded and we defer to the existing host.
func (s *mdnsServer) handleProbeTiebreak(msg *dns.Msg) {
	askingForUs := false
	for _, q := range msg.Question {
		if strings.EqualFold(q.Name, s.probeName) {
			askingForUs = true
			break
		}
	}
	if !askingForUs {
		return
	}
	var their []dns.RR
	for _, rr := range msg.Ns {
		if strings.EqualFold(rr.Header().Name, s.probeName) {
			their = append(their, rr)
		}
	}
	if len(their) == 0 {
		return
	}
	if s.theyWinTiebreak(their) {
		atomic.StoreInt32(&s.conflictDetected, 1)
	}
}

// theyWinTiebreak returns true if the other host's proposed records win the
// §8.2 tiebreak.  The canonical ordering is class, type, raw rdata; the first
// differing record decides, and if one list is a prefix of the other, the
// longer list wins.
func (s *mdnsServer) theyWinTiebreak(their []dns.RR) bool {
	our := make([]dns.RR, len(s.zone.records))
	copy(our, s.zone.records)
	sort.Slice(our, func(i, j int) bool { return rrLess(our[i], our[j]) })

	sortedTheir := make([]dns.RR, len(their))
	copy(sortedTheir, their)
	sort.Slice(sortedTheir, func(i, j int) bool { return rrLess(sortedTheir[i], sortedTheir[j]) })

	for i := 0; i < len(our) && i < len(sortedTheir); i++ {
		if rrLess(our[i], sortedTheir[i]) {
			return false
		}
		if rrLess(sortedTheir[i], our[i]) {
			return true
		}
	}
	// All compared equal; the list with remaining records wins.
	return len(sortedTheir) > len(our)
}

// answerQuery handles an incoming mDNS query in the running state.
func (s *mdnsServer) answerQuery(msg *dns.Msg, from *net.UDPAddr, conn *net.UDPConn) {
	legacy := from.Port != mdnsPort
	var unicastAnswer, multicastAnswer []dns.RR

	for _, q := range msg.Question {
		records := s.zone.Records(q, legacy)
		if len(records) == 0 {
			continue
		}
		records = s.zone.filterKnownAnswers(records, msg.Answer)
		if len(records) == 0 {
			continue
		}
		if legacy {
			// §6.7: TTL SHOULD NOT be greater than ten seconds.
			// Applied after known-answer suppression so that
			// filterKnownAnswers compares against the true TTL.
			for _, r := range records {
				if r.Header().Ttl > legacyUnicastMaxTTL {
					r.Header().Ttl = legacyUnicastMaxTTL
				}
			}
		}
		if legacy || (q.Qclass&quBit != 0) {
			unicastAnswer = append(unicastAnswer, records...)
		} else {
			multicastAnswer = append(multicastAnswer, records...)
		}
	}

	if len(multicastAnswer) > 0 {
		resp := &dns.Msg{
			MsgHdr: dns.MsgHdr{
				Id:            0,
				Response:      true,
				Opcode:        dns.OpcodeQuery,
				Authoritative: true,
			},
			Compress: true,
			Answer:   multicastAnswer,
		}
		s.sendMsg(resp)
	}
	if len(unicastAnswer) > 0 {
		resp := &dns.Msg{
			MsgHdr: dns.MsgHdr{
				Response:      true,
				Opcode:        dns.OpcodeQuery,
				Authoritative: true,
			},
			Compress: true,
			Answer:   unicastAnswer,
		}
		if legacy {
			// RFC 6762 §6.7: legacy unicast responses MUST repeat the
			// query ID and the question given in the query message.
			resp.Id = msg.Id
			resp.Question = msg.Question
		}
		s.sendRespTo(resp, from, conn)
	}
}

// sendMsg sends a multicast mDNS message via all available sockets.
func (s *mdnsServer) sendMsg(msg *dns.Msg) {
	buf, err := msg.Pack()
	if err != nil {
		log.Printf("WARN: pack mDNS message: %v", err)
		return
	}
	if s.socks.v4 != nil {
		if _, err := s.socks.v4.WriteToUDP(buf, mdnsAddr4); err != nil {
			log.Printf("WARN: send mDNS (v4): %v", err)
		}
	}
	if s.socks.v6 != nil {
		if _, err := s.socks.v6.WriteToUDP(buf, mdnsAddr6); err != nil {
			log.Printf("WARN: send mDNS (v6): %v", err)
		}
	}
}

// sendRespTo sends a unicast mDNS response to a specific address over the
// socket that received the query.
func (s *mdnsServer) sendRespTo(resp *dns.Msg, to *net.UDPAddr, conn *net.UDPConn) {
	buf, err := resp.Pack()
	if err != nil {
		log.Printf("WARN: pack unicast mDNS message: %v", err)
		return
	}
	if conn == nil {
		return
	}
	if _, err := conn.WriteToUDP(buf, to); err != nil {
		log.Printf("WARN: send unicast mDNS: %v", err)
	}
}

// Shutdown stops the mDNS server.
func (s *mdnsServer) Shutdown() error {
	if !atomic.CompareAndSwapInt32(&s.shutdown, 0, 1) {
		return nil
	}
	if s.socks.v4 != nil {
		s.socks.v4.Close()
	}
	if s.socks.v6 != nil {
		s.socks.v6.Close()
	}
	s.wg.Wait()
	return nil
}

// runProbe implements RFC 6762 §8.1. Three ANY queries 250ms apart with
// the QU bit set, carrying our proposed records in the Authority section
// for §8.2 tiebreaking.  Conflicting responses received during the probe
// window cause the probe to fail.
func runProbe(server *mdnsServer, ourRecords []dns.RR) error {
	name := server.probeName
	log.Printf("probing for %s", name)

	// Random initial delay 0-250ms (§8.1: guards against synchronised
	// power-on storms).  Responses received before this point are ignored
	// because the server is still in statePending.
	time.Sleep(time.Duration(rand.Int63n(int64(probeInitialMax))))

	// Activate conflict detection.
	atomic.StoreInt32(&server.state, int32(stateProbing))

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
		server.sendMsg(msg)
		log.Printf("sent probe %d/%d for %s", i+1, probeCount, name)
	}

	// §8.1: wait 250 ms after the third probe for any final conflicting
	// responses before deciding the name is available.
	time.Sleep(probeInterval)

	if atomic.LoadInt32(&server.conflictDetected) != 0 {
		return fmt.Errorf("conflicting response received for %s", name)
	}
	log.Printf("probe complete for %s", name)
	return nil
}

// runAnnounce implements RFC 6762 §8.3. Unsolicited responses with the
// cache-flush bit set on the (already-claimed) unique records. At least
// two announcements, 1 second apart.
func runAnnounce(server *mdnsServer, records []dns.RR) {
	name := server.probeName
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
		server.sendMsg(msg)
		log.Printf("sent announcement %d/%d for %s", i+1, announceCount, name)
	}
}

// runGoodbye implements RFC 6762 §10.1. Two responses with TTL=0 records,
// 100ms apart, to invalidate peer caches within ~1s.
func runGoodbye(server *mdnsServer, records []dns.RR) {
	name := server.probeName
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
		server.sendMsg(msg)
	}
	log.Printf("goodbye sent for %s", name)
}

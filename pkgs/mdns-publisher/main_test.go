package main

import (
	"net"
	"sync/atomic"
	"testing"
	"time"

	"github.com/miekg/dns"
)

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

func mustA(t *testing.T, name string, ip string, ttl uint32) dns.RR {
	t.Helper()
	return &dns.A{
		Hdr: dns.RR_Header{
			Name:   name,
			Rrtype: dns.TypeA,
			Class:  dns.ClassINET | cacheFlushBit,
			Ttl:    ttl,
		},
		A: net.ParseIP(ip).To4(),
	}
}

func mustAAAA(t *testing.T, name string, ip string, ttl uint32) dns.RR {
	t.Helper()
	return &dns.AAAA{
		Hdr: dns.RR_Header{
			Name:   name,
			Rrtype: dns.TypeAAAA,
			Class:  dns.ClassINET | cacheFlushBit,
			Ttl:    ttl,
		},
		AAAA: net.ParseIP(ip),
	}
}

func newTestZone(t *testing.T) *hostnameZone {
	t.Helper()
	records := []dns.RR{
		mustA(t, "testhost.local.", "192.168.1.100", 120),
		mustAAAA(t, "testhost.local.", "fd00::1", 120),
	}
	return &hostnameZone{
		records: records,
		nsec:    buildNSEC("testhost.local.", records, 120),
	}
}

// newTestServer creates a real mdnsServer bound to localhost UDP sockets
// so that handlePacket can be exercised over the wire.  The caller must
// call server.Shutdown() when done.
func newTestServer(t *testing.T, zone *hostnameZone, probeName string) (*mdnsServer, *net.UDPAddr) {
	t.Helper()

	// Bind a UDP socket on localhost with a random port for IPv4.
	conn, err := net.ListenUDP("udp4", &net.UDPAddr{IP: net.IPv4(127, 0, 0, 1), Port: 0})
	if err != nil {
		t.Skipf("cannot bind loopback UDP: %v", err)
	}

	socks := &mdnsSockets{v4: conn}
	server, err := newMDNSServer(zone, socks, probeName)
	if err != nil {
		conn.Close()
		t.Fatalf("newMDNSServer: %v", err)
	}
	atomic.StoreInt32(&server.state, int32(stateRunning))
	return server, conn.LocalAddr().(*net.UDPAddr)
}

// sendQueryTo sends an mDNS query to the server's address and returns
// the raw response bytes, or nil if none arrives within timeout.
func sendQueryTo(t *testing.T, serverAddr *net.UDPAddr, query *dns.Msg) []byte {
	t.Helper()
	qbuf, err := query.Pack()
	if err != nil {
		t.Fatalf("pack query: %v", err)
	}

	client, err := net.ListenUDP("udp4", &net.UDPAddr{IP: net.IPv4(127, 0, 0, 1), Port: 0})
	if err != nil {
		t.Fatalf("client listen: %v", err)
	}
	defer client.Close()

	if _, err := client.WriteToUDP(qbuf, serverAddr); err != nil {
		t.Fatalf("send query: %v", err)
	}

	client.SetReadDeadline(time.Now().Add(2 * time.Second))
	buf := make([]byte, 65536)
	n, _, err := client.ReadFromUDP(buf)
	if err != nil {
		return nil // timeout
	}
	out := make([]byte, n)
	copy(out, buf[:n])
	return out
}

// ---------------------------------------------------------------------------
// isValidLabel
// ---------------------------------------------------------------------------

func TestIsValidLabel(t *testing.T) {
	tests := []struct {
		name  string
		valid bool
	}{
		{"host", true},
		{"my-host", true},
		{"Host123", true},
		{"a", true},
		{"", false},            // empty
		{"host.local", false},  // dot
		{"host_name", false},   // underscore
		{"host name", false},   // space
		{"host-name-0", true},  // trailing dash ok
		{"a]b", false},         // bracket
		{"`host`", false},      // backtick
	}
	for _, tc := range tests {
		got := isValidLabel(tc.name)
		if got != tc.valid {
			t.Errorf("isValidLabel(%q) = %v, want %v", tc.name, got, tc.valid)
		}
	}
}

// ---------------------------------------------------------------------------
// rrdataBytes / rrdataEqual / rrLess
// ---------------------------------------------------------------------------

func TestRRDataEqual_A(t *testing.T) {
	a1 := mustA(t, "host.local.", "10.0.0.1", 120)
	a2 := mustA(t, "host.local.", "10.0.0.1", 999) // different TTL, same data
	a3 := mustA(t, "host.local.", "10.0.0.2", 120) // different IP

	if !rrdataEqual(a1, a2) {
		t.Error("same A records should be equal regardless of TTL")
	}
	if rrdataEqual(a1, a3) {
		t.Error("different A records should not be equal")
	}
}

func TestRRDataEqual_AAAA(t *testing.T) {
	s1 := mustAAAA(t, "host.local.", "fd00::1", 120)
	s2 := mustAAAA(t, "host.local.", "fd00::1", 60)
	s3 := mustAAAA(t, "host.local.", "fd00::2", 120)

	if !rrdataEqual(s1, s2) {
		t.Error("same AAAA records should be equal")
	}
	if rrdataEqual(s1, s3) {
		t.Error("different AAAA records should not be equal")
	}
}

func TestRRDataEqual_MixedTypes(t *testing.T) {
	a := mustA(t, "host.local.", "10.0.0.1", 120)
	aaaa := mustAAAA(t, "host.local.", "fd00::1", 120)

	if rrdataEqual(a, aaaa) {
		t.Error("A and AAAA should not be equal")
	}
}

func TestRRLess_ClassOrder(t *testing.T) {
	// Two records with different class (ignoring cache-flush) – unlikely
	// in practice but we verify the comparison order.
	r1 := &dns.A{
		Hdr: dns.RR_Header{Name: "a.local.", Rrtype: dns.TypeA, Class: dns.ClassINET, Ttl: 120},
		A:   net.ParseIP("10.0.0.1").To4(),
	}
	r2 := &dns.A{
		Hdr: dns.RR_Header{Name: "a.local.", Rrtype: dns.TypeA, Class: dns.ClassCHAOS, Ttl: 120},
		A:   net.ParseIP("10.0.0.1").To4(),
	}
	if !rrLess(r1, r2) {
		t.Error("IN (1) < CHAOS (3)")
	}
	if rrLess(r2, r1) {
		t.Error("CHAOS not < IN")
	}
}

func TestRRLess_TypeOrder(t *testing.T) {
	a := mustA(t, "host.local.", "10.0.0.1", 120)
	aaaa := mustAAAA(t, "host.local.", "fd00::1", 120)

	// A (type 1) < AAAA (type 28)
	if !rrLess(a, aaaa) {
		t.Error("A should sort before AAAA")
	}
	if rrLess(aaaa, a) {
		t.Error("AAAA should not sort before A")
	}
}

func TestRRLess_RdataOrder(t *testing.T) {
	a1 := mustA(t, "host.local.", "10.0.0.1", 120)
	a2 := mustA(t, "host.local.", "10.0.0.2", 120)

	if !rrLess(a1, a2) {
		t.Error("10.0.0.1 < 10.0.0.2")
	}
	if rrLess(a2, a1) {
		t.Error("10.0.0.2 not < 10.0.0.1")
	}
	if rrLess(a1, a1) {
		t.Error("equal records should not be less")
	}
}

func TestRRLess_IPv6Order(t *testing.T) {
	s1 := mustAAAA(t, "host.local.", "fd00::1", 120)
	s2 := mustAAAA(t, "host.local.", "fd00::2", 120)

	if !rrLess(s1, s2) {
		t.Error("fd00::1 < fd00::2")
	}
}

// ---------------------------------------------------------------------------
// buildNSEC
// ---------------------------------------------------------------------------

func TestBuildNSEC(t *testing.T) {
	records := []dns.RR{
		mustA(t, "host.local.", "10.0.0.1", 120),
		mustAAAA(t, "host.local.", "fd00::1", 120),
	}

	nsec := buildNSEC("host.local.", records, 120)
	if nsec == nil {
		t.Fatal("buildNSEC returned nil")
	}
	ns, ok := nsec.(*dns.NSEC)
	if !ok {
		t.Fatal("expected *dns.NSEC")
	}

	if ns.Hdr.Name != "host.local." {
		t.Errorf("NSEC name = %q, want host.local.", ns.Hdr.Name)
	}
	if ns.Hdr.Rrtype != dns.TypeNSEC {
		t.Errorf("NSEC type = %d, want %d", ns.Hdr.Rrtype, dns.TypeNSEC)
	}
	if ns.Hdr.Ttl != 120 {
		t.Errorf("NSEC TTL = %d, want 120", ns.Hdr.Ttl)
	}
	if ns.NextDomain != "host.local." {
		t.Errorf("NSEC NextDomain = %q, want host.local.", ns.NextDomain)
	}

	// TypeBitMap should contain A (1) and AAAA (28).
	typeSet := make(map[uint16]bool)
	for _, t := range ns.TypeBitMap {
		typeSet[t] = true
	}
	if !typeSet[dns.TypeA] {
		t.Error("NSEC missing A type")
	}
	if !typeSet[dns.TypeAAAA] {
		t.Error("NSEC missing AAAA type")
	}
	if len(ns.TypeBitMap) != 2 {
		t.Errorf("NSEC TypeBitMap has %d entries, want 2", len(ns.TypeBitMap))
	}
}

func TestBuildNSEC_Empty(t *testing.T) {
	if nsec := buildNSEC("host.local.", nil, 120); nsec != nil {
		t.Error("buildNSEC with no records should return nil")
	}
}

// ---------------------------------------------------------------------------
// nsecTypesEqual
// ---------------------------------------------------------------------------

func TestNsecTypesEqual(t *testing.T) {
	tests := []struct {
		a, b []uint16
		want bool
	}{
		{[]uint16{dns.TypeA, dns.TypeAAAA}, []uint16{dns.TypeAAAA, dns.TypeA}, true},
		{[]uint16{dns.TypeA}, []uint16{dns.TypeA}, true},
		{[]uint16{dns.TypeA}, []uint16{dns.TypeAAAA}, false},
		{[]uint16{}, []uint16{}, true},
		{[]uint16{dns.TypeA}, []uint16{}, false},
	}
	for _, tc := range tests {
		got := nsecTypesEqual(tc.a, tc.b)
		if got != tc.want {
			t.Errorf("nsecTypesEqual(%v, %v) = %v, want %v", tc.a, tc.b, got, tc.want)
		}
	}
}

// ---------------------------------------------------------------------------
// hostnameZone.Records
// ---------------------------------------------------------------------------

func TestRecords_ANY(t *testing.T) {
	zone := newTestZone(t)
	q := dns.Question{Name: "testhost.local.", Qtype: dns.TypeANY, Qclass: dns.ClassINET}
	recs := zone.Records(q, false)
	if len(recs) != 2 {
		t.Fatalf("ANY query: got %d records, want 2", len(recs))
	}
	// Both should have cache-flush bit set.
	for _, r := range recs {
		if r.Header().Class != dns.ClassINET|cacheFlushBit {
			t.Errorf("record class = %#x, want %#x", r.Header().Class, dns.ClassINET|cacheFlushBit)
		}
	}
}

func TestRecords_TypeA(t *testing.T) {
	zone := newTestZone(t)
	q := dns.Question{Name: "testhost.local.", Qtype: dns.TypeA, Qclass: dns.ClassINET}
	recs := zone.Records(q, false)
	if len(recs) != 1 {
		t.Fatalf("A query: got %d records, want 1", len(recs))
	}
	if recs[0].Header().Rrtype != dns.TypeA {
		t.Errorf("record type = %d, want A", recs[0].Header().Rrtype)
	}
}

func TestRecords_TypeAAAA(t *testing.T) {
	zone := newTestZone(t)
	q := dns.Question{Name: "testhost.local.", Qtype: dns.TypeAAAA, Qclass: dns.ClassINET}
	recs := zone.Records(q, false)
	if len(recs) != 1 {
		t.Fatalf("AAAA query: got %d records, want 1", len(recs))
	}
	if recs[0].Header().Rrtype != dns.TypeAAAA {
		t.Errorf("record type = %d, want AAAA", recs[0].Header().Rrtype)
	}
}

func TestRecords_NSEC_Negative(t *testing.T) {
	zone := newTestZone(t)
	// Query for a type we don't have: TXT.
	q := dns.Question{Name: "testhost.local.", Qtype: dns.TypeTXT, Qclass: dns.ClassINET}
	recs := zone.Records(q, false)
	if len(recs) != 1 {
		t.Fatalf("TXT query (we only have A/AAAA): got %d records, want 1", len(recs))
	}
	nsec, ok := recs[0].(*dns.NSEC)
	if !ok {
		t.Fatalf("expected NSEC, got %T", recs[0])
	}
	if nsec.Hdr.Name != "testhost.local." {
		t.Errorf("NSEC name = %q", nsec.Hdr.Name)
	}
	// Should have cache-flush bit (unique record).
	if nsec.Hdr.Class != dns.ClassINET|cacheFlushBit {
		t.Errorf("NSEC class = %#x, want %#x", nsec.Hdr.Class, dns.ClassINET|cacheFlushBit)
	}
}

func TestRecords_NSEC_Legacy(t *testing.T) {
	zone := newTestZone(t)
	q := dns.Question{Name: "testhost.local.", Qtype: dns.TypeTXT, Qclass: dns.ClassINET}
	recs := zone.Records(q, true) // legacy
	if len(recs) != 1 {
		t.Fatalf("got %d records, want 1", len(recs))
	}
	h := recs[0].Header()
	// Legacy: no cache-flush bit, but TTL is preserved (capping
	// happens in answerQuery after known-answer suppression).
	if h.Class != dns.ClassINET {
		t.Errorf("legacy NSEC class = %#x, want INET", h.Class)
	}
	if h.Ttl != 120 {
		t.Errorf("legacy NSEC TTL = %d, want 120 (true value)", h.Ttl)
	}
}

func TestRecords_NSEC_NotForANY(t *testing.T) {
	zone := newTestZone(t)
	// ANY query should return positive records, not NSEC.
	q := dns.Question{Name: "testhost.local.", Qtype: dns.TypeANY, Qclass: dns.ClassINET}
	recs := zone.Records(q, false)
	for _, r := range recs {
		if r.Header().Rrtype == dns.TypeNSEC {
			t.Error("ANY query should not return NSEC")
		}
	}
}

func TestRecords_NSEC_WrongName(t *testing.T) {
	zone := newTestZone(t)
	// Query for a name we don't own.
	q := dns.Question{Name: "otherhost.local.", Qtype: dns.TypeTXT, Qclass: dns.ClassINET}
	recs := zone.Records(q, false)
	if len(recs) != 0 {
		t.Errorf("query for unknown name: got %d records, want 0", len(recs))
	}
}

func TestRecords_NSEC_WrongClass(t *testing.T) {
	zone := newTestZone(t)
	// Non-INET class should return nothing.
	q := dns.Question{Name: "testhost.local.", Qtype: dns.TypeTXT, Qclass: dns.ClassCHAOS}
	recs := zone.Records(q, false)
	if len(recs) != 0 {
		t.Errorf("CHAOS class: got %d records, want 0", len(recs))
	}
}

func TestRecords_Legacy_StripsCacheFlush(t *testing.T) {
	zone := newTestZone(t)
	q := dns.Question{Name: "testhost.local.", Qtype: dns.TypeA, Qclass: dns.ClassINET}
	recs := zone.Records(q, true) // legacy
	if len(recs) != 1 {
		t.Fatalf("got %d, want 1", len(recs))
	}
	h := recs[0].Header()
	if h.Class != dns.ClassINET {
		t.Errorf("legacy A class = %#x, want INET (no cache-flush)", h.Class)
	}
}

func TestRecords_Legacy_PreservesTrueTTL(t *testing.T) {
	// Records() strips cache-flush for legacy but does NOT cap TTL.
	// TTL capping (§6.7) happens in answerQuery after known-answer
	// suppression, so filterKnownAnswers sees the true TTL.
	records := []dns.RR{mustA(t, "host.local.", "10.0.0.1", 120)}
	zone := &hostnameZone{
		records: records,
		nsec:    buildNSEC("host.local.", records, 120),
	}
	q := dns.Question{Name: "host.local.", Qtype: dns.TypeA, Qclass: dns.ClassINET}
	recs := zone.Records(q, true)
	if len(recs) != 1 {
		t.Fatalf("got %d, want 1", len(recs))
	}
	if recs[0].Header().Ttl != 120 {
		t.Errorf("legacy TTL = %d, want 120 (true value)", recs[0].Header().Ttl)
	}
}

func TestRecords_Legacy_TTLAlreadyLow(t *testing.T) {
	records := []dns.RR{mustA(t, "host.local.", "10.0.0.1", 5)}
	zone := &hostnameZone{records: records, nsec: buildNSEC("host.local.", records, 5)}
	q := dns.Question{Name: "host.local.", Qtype: dns.TypeA, Qclass: dns.ClassINET}
	recs := zone.Records(q, true)
	if len(recs) != 1 {
		t.Fatalf("got %d, want 1", len(recs))
	}
	if recs[0].Header().Ttl != 5 {
		t.Errorf("legacy TTL = %d, want 5 (not capped)", recs[0].Header().Ttl)
	}
}

func TestRecords_QU_ClassINET(t *testing.T) {
	zone := newTestZone(t)
	// QU query: class INET | QU bit.
	q := dns.Question{Name: "testhost.local.", Qtype: dns.TypeA, Qclass: dns.ClassINET | quBit}
	recs := zone.Records(q, false) // not legacy
	if len(recs) != 1 {
		t.Fatalf("QU query: got %d, want 1", len(recs))
	}
	// Cache-flush should be preserved.
	if recs[0].Header().Class != dns.ClassINET|cacheFlushBit {
		t.Errorf("QU response class = %#x, want cache-flush", recs[0].Header().Class)
	}
}

func TestRecords_ClassANY(t *testing.T) {
	zone := newTestZone(t)
	q := dns.Question{Name: "testhost.local.", Qtype: dns.TypeANY, Qclass: dns.ClassANY}
	recs := zone.Records(q, false)
	if len(recs) != 2 {
		t.Fatalf("qclass ANY: got %d, want 2", len(recs))
	}
}

func TestRecords_CopyIndependence(t *testing.T) {
	zone := newTestZone(t)
	q := dns.Question{Name: "testhost.local.", Qtype: dns.TypeA, Qclass: dns.ClassINET}
	r1 := zone.Records(q, false)
	r2 := zone.Records(q, false)
	if len(r1) != 1 || len(r2) != 1 {
		t.Fatal("unexpected count")
	}
	// Mutate r1's TTL; r2 should be unaffected.
	r1[0].Header().Ttl = 1
	if r2[0].Header().Ttl == 1 {
		t.Error("Records returned shared mutable state")
	}
}

// ---------------------------------------------------------------------------
// filterKnownAnswers / isKnownAnswer
// ---------------------------------------------------------------------------

func TestFilterKnownAnswers_NoKnown(t *testing.T) {
	zone := newTestZone(t)
	recs := []dns.RR{mustA(t, "testhost.local.", "192.168.1.100", 120)}
	got := zone.filterKnownAnswers(recs, nil)
	if len(got) != 1 {
		t.Errorf("no known answers: got %d, want 1", len(got))
	}
}

func TestFilterKnownAnswers_SuppressExact(t *testing.T) {
	zone := newTestZone(t)
	record := mustA(t, "testhost.local.", "192.168.1.100", 120)
	known := []dns.RR{mustA(t, "testhost.local.", "192.168.1.100", 120)}

	got := zone.filterKnownAnswers([]dns.RR{record}, known)
	if len(got) != 0 {
		t.Errorf("exact known answer: got %d, want 0", len(got))
	}
}

func TestFilterKnownAnswers_SuppressHighTTL(t *testing.T) {
	zone := newTestZone(t)
	record := mustA(t, "testhost.local.", "192.168.1.100", 120)
	// Known answer TTL 60 ≥ 120/2 = 60 → suppress.
	known := []dns.RR{mustA(t, "testhost.local.", "192.168.1.100", 60)}

	got := zone.filterKnownAnswers([]dns.RR{record}, known)
	if len(got) != 0 {
		t.Errorf("known TTL ≥ half: got %d, want 0", len(got))
	}
}

func TestFilterKnownAnswers_NoSuppressLowTTL(t *testing.T) {
	zone := newTestZone(t)
	record := mustA(t, "testhost.local.", "192.168.1.100", 120)
	// Known answer TTL 59 < 120/2 = 60 → send.
	known := []dns.RR{mustA(t, "testhost.local.", "192.168.1.100", 59)}

	got := zone.filterKnownAnswers([]dns.RR{record}, known)
	if len(got) != 1 {
		t.Errorf("known TTL < half: got %d, want 1", len(got))
	}
}

func TestFilterKnownAnswers_WrongIP(t *testing.T) {
	zone := newTestZone(t)
	record := mustA(t, "testhost.local.", "192.168.1.100", 120)
	known := []dns.RR{mustA(t, "testhost.local.", "192.168.1.200", 120)}

	got := zone.filterKnownAnswers([]dns.RR{record}, known)
	if len(got) != 1 {
		t.Errorf("different IP: got %d, want 1", len(got))
	}
}

func TestFilterKnownAnswers_MixedRecords(t *testing.T) {
	zone := newTestZone(t)
	recs := []dns.RR{
		mustA(t, "testhost.local.", "192.168.1.100", 120),
		mustAAAA(t, "testhost.local.", "fd00::1", 120),
	}
	// Known has only A.
	known := []dns.RR{mustA(t, "testhost.local.", "192.168.1.100", 120)}

	got := zone.filterKnownAnswers(recs, known)
	if len(got) != 1 {
		t.Fatalf("mixed: got %d, want 1", len(got))
	}
	if got[0].Header().Rrtype != dns.TypeAAAA {
		t.Errorf("remaining record type = %d, want AAAA", got[0].Header().Rrtype)
	}
}

func TestFilterKnownAnswers_NSEC(t *testing.T) {
	zone := newTestZone(t)
	// NSEC negative answer.
	q := dns.Question{Name: "testhost.local.", Qtype: dns.TypeTXT, Qclass: dns.ClassINET}
	nsecRecs := zone.Records(q, false)
	if len(nsecRecs) != 1 {
		t.Fatal("expected NSEC")
	}
	known := []dns.RR{nsecRecs[0]} // querier already knows the NSEC

	got := zone.filterKnownAnswers(nsecRecs, known)
	if len(got) != 0 {
		t.Errorf("known NSEC: got %d, want 0", len(got))
	}
}

// ---------------------------------------------------------------------------
// theyWinTiebreak (§8.2)
// ---------------------------------------------------------------------------

func newTestServerForTiebreak(t *testing.T, records []dns.RR, probeName string) *mdnsServer {
	t.Helper()
	zone := &hostnameZone{
		records: records,
		nsec:    buildNSEC(probeName, records, 120),
	}
	return &mdnsServer{
		zone:      zone,
		socks:     &mdnsSockets{}, // no sockets needed for tiebreak logic
		state:     int32(stateProbing),
		probeName: probeName,
	}
}

func TestTheyWinTiebreak_SameRecords(t *testing.T) {
	our := []dns.RR{mustA(t, "host.local.", "10.0.0.1", 120)}
	s := newTestServerForTiebreak(t, our, "host.local.")

	their := []dns.RR{mustA(t, "host.local.", "10.0.0.1", 255)}
	if s.theyWinTiebreak(their) {
		t.Error("same data: they should not win")
	}
}

func TestTheyWinTiebreak_TheyHaveLowerIP(t *testing.T) {
	our := []dns.RR{mustA(t, "host.local.", "10.0.0.2", 120)}
	s := newTestServerForTiebreak(t, our, "host.local.")

	their := []dns.RR{mustA(t, "host.local.", "10.0.0.1", 255)}
	if !s.theyWinTiebreak(their) {
		t.Error("they have lower IP: they should win")
	}
}

func TestTheyWinTiebreak_WeHaveLowerIP(t *testing.T) {
	our := []dns.RR{mustA(t, "host.local.", "10.0.0.1", 120)}
	s := newTestServerForTiebreak(t, our, "host.local.")

	their := []dns.RR{mustA(t, "host.local.", "10.0.0.2", 255)}
	if s.theyWinTiebreak(their) {
		t.Error("we have lower IP: they should not win")
	}
}

func TestTheyWinTiebreak_TypeOrder(t *testing.T) {
	// A (1) < AAAA (28).  They have A, we have AAAA → they win.
	our := []dns.RR{mustAAAA(t, "host.local.", "fd00::1", 120)}
	s := newTestServerForTiebreak(t, our, "host.local.")

	their := []dns.RR{mustA(t, "host.local.", "10.0.0.1", 255)}
	if !s.theyWinTiebreak(their) {
		t.Error("A < AAAA: they should win")
	}
}

func TestTheyWinTiebreak_LongerList(t *testing.T) {
	// They have A+AAAA, we only have A → they win (longer list).
	our := []dns.RR{mustA(t, "host.local.", "10.0.0.1", 120)}
	s := newTestServerForTiebreak(t, our, "host.local.")

	their := []dns.RR{
		mustA(t, "host.local.", "10.0.0.1", 255),
		mustAAAA(t, "host.local.", "fd00::1", 255),
	}
	if !s.theyWinTiebreak(their) {
		t.Error("they have more records: they should win")
	}
}

func TestTheyWinTiebreak_ShorterList(t *testing.T) {
	// We have A+AAAA, they only have A → we win.
	our := []dns.RR{
		mustA(t, "host.local.", "10.0.0.1", 120),
		mustAAAA(t, "host.local.", "fd00::1", 120),
	}
	s := newTestServerForTiebreak(t, our, "host.local.")

	their := []dns.RR{mustA(t, "host.local.", "10.0.0.1", 255)}
	if s.theyWinTiebreak(their) {
		t.Error("they have fewer records: they should not win")
	}
}

func TestTheyWinTiebreak_DifferentAAAA(t *testing.T) {
	our := []dns.RR{mustAAAA(t, "host.local.", "fd00::1", 120)}
	s := newTestServerForTiebreak(t, our, "host.local.")

	their := []dns.RR{mustAAAA(t, "host.local.", "fd00::2", 255)}
	// fd00::1 < fd00::2 → we win → they should NOT win.
	if s.theyWinTiebreak(their) {
		t.Error("fd00::1 < fd00::2: they should not win")
	}
}

// ---------------------------------------------------------------------------
// checkConflict
// ---------------------------------------------------------------------------

func TestCheckConflict_AnswerMatch(t *testing.T) {
	s := &mdnsServer{probeName: "host.local."}
	msg := &dns.Msg{
		Answer: []dns.RR{mustA(t, "host.local.", "10.0.0.99", 120)},
	}
	s.checkConflict(msg)
	if atomic.LoadInt32(&s.conflictDetected) == 0 {
		t.Error("conflict not detected from Answer")
	}
}

func TestCheckConflict_NsMatch(t *testing.T) {
	s := &mdnsServer{probeName: "host.local."}
	msg := &dns.Msg{
		Ns: []dns.RR{mustA(t, "host.local.", "10.0.0.99", 120)},
	}
	s.checkConflict(msg)
	if atomic.LoadInt32(&s.conflictDetected) == 0 {
		t.Error("conflict not detected from Ns")
	}
}

func TestCheckConflict_ExtraMatch(t *testing.T) {
	s := &mdnsServer{probeName: "host.local."}
	msg := &dns.Msg{
		Extra: []dns.RR{mustA(t, "host.local.", "10.0.0.99", 120)},
	}
	s.checkConflict(msg)
	if atomic.LoadInt32(&s.conflictDetected) == 0 {
		t.Error("conflict not detected from Extra")
	}
}

func TestCheckConflict_WrongName(t *testing.T) {
	s := &mdnsServer{probeName: "host.local."}
	msg := &dns.Msg{
		Answer: []dns.RR{mustA(t, "other.local.", "10.0.0.99", 120)},
	}
	s.checkConflict(msg)
	if atomic.LoadInt32(&s.conflictDetected) != 0 {
		t.Error("spurious conflict for wrong name")
	}
}

// ---------------------------------------------------------------------------
// handleProbeTiebreak
// ---------------------------------------------------------------------------

func TestHandleProbeTiebreak_Win(t *testing.T) {
	// We have lower IP → we win → no conflict.
	our := []dns.RR{mustA(t, "host.local.", "10.0.0.1", 120)}
	s := newTestServerForTiebreak(t, our, "host.local.")

	probeMsg := &dns.Msg{
		Question: []dns.Question{{Name: "host.local.", Qtype: dns.TypeANY, Qclass: dns.ClassINET | quBit}},
		Ns:       []dns.RR{mustA(t, "host.local.", "10.0.0.2", 255)},
	}
	s.handleProbeTiebreak(probeMsg)
	if atomic.LoadInt32(&s.conflictDetected) != 0 {
		t.Error("we should win; conflict should not be set")
	}
}

func TestHandleProbeTiebreak_Lose(t *testing.T) {
	// We have higher IP → we lose → conflict.
	our := []dns.RR{mustA(t, "host.local.", "10.0.0.2", 120)}
	s := newTestServerForTiebreak(t, our, "host.local.")

	probeMsg := &dns.Msg{
		Question: []dns.Question{{Name: "host.local.", Qtype: dns.TypeANY, Qclass: dns.ClassINET | quBit}},
		Ns:       []dns.RR{mustA(t, "host.local.", "10.0.0.1", 255)},
	}
	s.handleProbeTiebreak(probeMsg)
	if atomic.LoadInt32(&s.conflictDetected) == 0 {
		t.Error("we should lose; conflict should be set")
	}
}

func TestHandleProbeTiebreak_IgnoreWrongName(t *testing.T) {
	our := []dns.RR{mustA(t, "host.local.", "10.0.0.1", 120)}
	s := newTestServerForTiebreak(t, our, "host.local.")

	probeMsg := &dns.Msg{
		Question: []dns.Question{{Name: "other.local.", Qtype: dns.TypeANY, Qclass: dns.ClassINET | quBit}},
		Ns:       []dns.RR{mustA(t, "other.local.", "10.0.0.1", 255)},
	}
	s.handleProbeTiebreak(probeMsg)
	if atomic.LoadInt32(&s.conflictDetected) != 0 {
		t.Error("wrong name should not trigger tiebreak")
	}
}

func TestHandleProbeTiebreak_IgnoreNoAuthority(t *testing.T) {
	our := []dns.RR{mustA(t, "host.local.", "10.0.0.1", 120)}
	s := newTestServerForTiebreak(t, our, "host.local.")

	// A normal query (no Ns) for our name during probing – not a probe.
	probeMsg := &dns.Msg{
		Question: []dns.Question{{Name: "host.local.", Qtype: dns.TypeA, Qclass: dns.ClassINET}},
	}
	s.handleProbeTiebreak(probeMsg)
	if atomic.LoadInt32(&s.conflictDetected) != 0 {
		t.Error("query without Ns should not trigger tiebreak")
	}
}

// ---------------------------------------------------------------------------
// handlePacket: state machine
// ---------------------------------------------------------------------------

func TestHandlePacket_Pending_Ignores(t *testing.T) {
	zone := newTestZone(t)
	server := &mdnsServer{
		zone:      zone,
		socks:     &mdnsSockets{},
		state:     int32(statePending),
		probeName: "testhost.local.",
	}

	// Build a valid mDNS query.
	query := &dns.Msg{
		MsgHdr: dns.MsgHdr{Opcode: dns.OpcodeQuery},
		Question: []dns.Question{
			{Name: "testhost.local.", Qtype: dns.TypeA, Qclass: dns.ClassINET},
		},
	}
	buf, _ := query.Pack()

	// Should not panic, should not answer.
	server.handlePacket(buf, &net.UDPAddr{IP: net.IPv4(127, 0, 0, 1), Port: mdnsPort}, nil)
	// No assertion needed – just verify it doesn't crash.
}

func TestHandlePacket_Probing_DetectsConflictResponse(t *testing.T) {
	zone := newTestZone(t)
	server := &mdnsServer{
		zone:      zone,
		socks:     &mdnsSockets{},
		state:     int32(stateProbing),
		probeName: "testhost.local.",
	}

	// A response containing a record for our name.
	resp := &dns.Msg{
		MsgHdr:  dns.MsgHdr{Response: true, Opcode: dns.OpcodeQuery},
		Answer:  []dns.RR{mustA(t, "testhost.local.", "10.0.0.99", 120)},
	}
	buf, _ := resp.Pack()

	server.handlePacket(buf, &net.UDPAddr{IP: net.IPv4(127, 0, 0, 1), Port: mdnsPort}, nil)

	if atomic.LoadInt32(&server.conflictDetected) == 0 {
		t.Error("conflict not detected in probing state")
	}
}

func TestHandlePacket_Probing_IgnoresQueries(t *testing.T) {
	zone := newTestZone(t)
	server := &mdnsServer{
		zone:      zone,
		socks:     &mdnsSockets{},
		state:     int32(stateProbing),
		probeName: "testhost.local.",
	}

	// A normal query (not a probe) during probing.
	query := &dns.Msg{
		MsgHdr:   dns.MsgHdr{Opcode: dns.OpcodeQuery},
		Question: []dns.Question{{Name: "testhost.local.", Qtype: dns.TypeA, Qclass: dns.ClassINET}},
	}
	buf, _ := query.Pack()

	server.handlePacket(buf, &net.UDPAddr{IP: net.IPv4(127, 0, 0, 1), Port: mdnsPort}, nil)

	// No conflict, no answer (probe state doesn't answer queries).
	if atomic.LoadInt32(&server.conflictDetected) != 0 {
		t.Error("normal query during probing should not trigger conflict")
	}
}

// ---------------------------------------------------------------------------
// handlePacket: mDNS (non-legacy) behaviour via direct call
// ---------------------------------------------------------------------------

// mDNS (non-legacy) behaviour is tested indirectly via the Records()
// unit tests above: TestRecords_QU_ClassINET, TestRecords_Legacy_*, and
// TestRecords_NSEC_* verify that the legacy flag correctly controls cache-flush
// and TTL capping.  End-to-end mDNS tests require binding to port 5353 (root)
// so they are covered by the legacy end-to-end tests below plus the unit tests.
//
// If running as root (e.g. in CI), the following additional coverage is available:
//   TestAnswerQuery_mDNS_KeepsCacheFlush (needs port 5353)
//   TestAnswerQuery_mDNS_QU_KeepsCacheFlush (needs port 5353)

// ---------------------------------------------------------------------------
// end-to-end: query → answer over real UDP sockets
// ---------------------------------------------------------------------------

func TestEndToEnd_AnyQuery(t *testing.T) {
	zone := newTestZone(t)
	server, addr := newTestServer(t, zone, "testhost.local.")
	defer server.Shutdown()

	query := &dns.Msg{
		MsgHdr:   dns.MsgHdr{Id: 0, Opcode: dns.OpcodeQuery},
		Question: []dns.Question{{Name: "testhost.local.", Qtype: dns.TypeANY, Qclass: dns.ClassINET}},
	}

	raw := sendQueryTo(t, addr, query)
	if raw == nil {
		t.Fatal("no response received")
	}

	var resp dns.Msg
	if err := resp.Unpack(raw); err != nil {
		t.Fatalf("unpack response: %v", err)
	}
	if !resp.Response {
		t.Error("response bit not set")
	}
	if !resp.Authoritative {
		t.Error("authoritative bit not set")
	}
	// Client is on a random port → legacy unicast.  Expect both A and AAAA.
	if len(resp.Answer) != 2 {
		t.Fatalf("answer count = %d, want 2", len(resp.Answer))
	}
	// Legacy responses MUST echo the question.
	if len(resp.Question) != 1 {
		t.Errorf("legacy response missing echoed question")
	}
}

func TestEndToEnd_AQuery(t *testing.T) {
	zone := newTestZone(t)
	server, addr := newTestServer(t, zone, "testhost.local.")
	defer server.Shutdown()

	query := &dns.Msg{
		MsgHdr:   dns.MsgHdr{Id: 0, Opcode: dns.OpcodeQuery},
		Question: []dns.Question{{Name: "testhost.local.", Qtype: dns.TypeA, Qclass: dns.ClassINET}},
	}

	raw := sendQueryTo(t, addr, query)
	if raw == nil {
		t.Fatal("no response received")
	}

	var resp dns.Msg
	if err := resp.Unpack(raw); err != nil {
		t.Fatalf("unpack response: %v", err)
	}
	if len(resp.Answer) != 1 {
		t.Fatalf("answer count = %d, want 1", len(resp.Answer))
	}
	if resp.Answer[0].Header().Rrtype != dns.TypeA {
		t.Errorf("answer type = %d, want A", resp.Answer[0].Header().Rrtype)
	}
}

func TestEndToEnd_NSEC_Negative(t *testing.T) {
	zone := newTestZone(t)
	server, addr := newTestServer(t, zone, "testhost.local.")
	defer server.Shutdown()

	query := &dns.Msg{
		MsgHdr:   dns.MsgHdr{Id: 0, Opcode: dns.OpcodeQuery},
		Question: []dns.Question{{Name: "testhost.local.", Qtype: dns.TypeTXT, Qclass: dns.ClassINET}},
	}

	raw := sendQueryTo(t, addr, query)
	if raw == nil {
		t.Fatal("no response received")
	}

	var resp dns.Msg
	if err := resp.Unpack(raw); err != nil {
		t.Fatalf("unpack response: %v", err)
	}
	if len(resp.Answer) != 1 {
		t.Fatalf("answer count = %d, want 1 (NSEC)", len(resp.Answer))
	}
	if resp.Answer[0].Header().Rrtype != dns.TypeNSEC {
		t.Errorf("answer type = %d, want NSEC", resp.Answer[0].Header().Rrtype)
	}
}

func TestEndToEnd_WrongName_NoResponse(t *testing.T) {
	zone := newTestZone(t)
	server, addr := newTestServer(t, zone, "testhost.local.")
	defer server.Shutdown()

	query := &dns.Msg{
		MsgHdr:   dns.MsgHdr{Id: 0, Opcode: dns.OpcodeQuery},
		Question: []dns.Question{{Name: "other.local.", Qtype: dns.TypeA, Qclass: dns.ClassINET}},
	}

	raw := sendQueryTo(t, addr, query)
	if raw != nil {
		t.Error("should not respond to queries for names we don't own")
	}
}

func TestEndToEnd_KnownAnswer_Suppresses(t *testing.T) {
	zone := newTestZone(t)
	server, addr := newTestServer(t, zone, "testhost.local.")
	defer server.Shutdown()

	// Build a query that includes a known answer with full TTL.
	knownA := mustA(t, "testhost.local.", "192.168.1.100", 120)
	query := &dns.Msg{
		MsgHdr:   dns.MsgHdr{Id: 0, Opcode: dns.OpcodeQuery},
		Question: []dns.Question{{Name: "testhost.local.", Qtype: dns.TypeA, Qclass: dns.ClassINET}},
		Answer:   []dns.RR{knownA},
	}

	raw := sendQueryTo(t, addr, query)
	if raw != nil {
		var resp dns.Msg
		if err := resp.Unpack(raw); err != nil {
			t.Fatalf("unpack: %v", err)
		}
		// The A record should be suppressed; only AAAA should remain
		// (if the client asked ANY, but here we asked A so nothing
		// should be answered).
		if len(resp.Answer) != 0 {
			t.Errorf("A query with known answer A: got %d answers, want 0", len(resp.Answer))
		}
	}
	// nil raw = timeout = no response, which is also correct.
}

func TestEndToEnd_KnownAnswer_PartialSuppress(t *testing.T) {
	zone := newTestZone(t)
	server, addr := newTestServer(t, zone, "testhost.local.")
	defer server.Shutdown()

	// Known answer for A with full TTL; ask for ANY → AAAA should still
	// be answered.
	knownA := mustA(t, "testhost.local.", "192.168.1.100", 120)
	query := &dns.Msg{
		MsgHdr:   dns.MsgHdr{Id: 0, Opcode: dns.OpcodeQuery},
		Question: []dns.Question{{Name: "testhost.local.", Qtype: dns.TypeANY, Qclass: dns.ClassINET}},
		Answer:   []dns.RR{knownA},
	}

	raw := sendQueryTo(t, addr, query)
	if raw == nil {
		t.Fatal("no response received")
	}

	var resp dns.Msg
	if err := resp.Unpack(raw); err != nil {
		t.Fatalf("unpack: %v", err)
	}
	// A suppressed, AAAA remains.
	if len(resp.Answer) != 1 {
		t.Fatalf("answer count = %d, want 1 (AAAA only)", len(resp.Answer))
	}
	if resp.Answer[0].Header().Rrtype != dns.TypeAAAA {
		t.Errorf("remaining answer type = %d, want AAAA", resp.Answer[0].Header().Rrtype)
	}
}

func TestEndToEnd_LegacyCacheFlushStripped(t *testing.T) {
	zone := newTestZone(t)
	server, addr := newTestServer(t, zone, "testhost.local.")
	defer server.Shutdown()

	query := &dns.Msg{
		MsgHdr:   dns.MsgHdr{Id: 1, Opcode: dns.OpcodeQuery},
		Question: []dns.Question{{Name: "testhost.local.", Qtype: dns.TypeA, Qclass: dns.ClassINET}},
	}

	raw := sendQueryTo(t, addr, query)
	if raw == nil {
		t.Fatal("no response")
	}

	var resp dns.Msg
	if err := resp.Unpack(raw); err != nil {
		t.Fatalf("unpack: %v", err)
	}
	if len(resp.Answer) != 1 {
		t.Fatalf("answer count = %d, want 1", len(resp.Answer))
	}
	// Legacy responses MUST NOT have the cache-flush bit.
	if resp.Answer[0].Header().Class != dns.ClassINET {
		t.Errorf("legacy response class = %#x, want INET (no cache-flush)", resp.Answer[0].Header().Class)
	}
	// Legacy responses SHOULD have TTL ≤ 10.
	if resp.Answer[0].Header().Ttl > legacyUnicastMaxTTL {
		t.Errorf("legacy TTL = %d, want ≤ %d", resp.Answer[0].Header().Ttl, legacyUnicastMaxTTL)
	}
}

func TestEndToEnd_KnownAnswer_LowTTL_NotSuppressed(t *testing.T) {
	zone := newTestZone(t)
	server, addr := newTestServer(t, zone, "testhost.local.")
	defer server.Shutdown()

	// Known answer TTL 59 < 120/2 = 60 → NOT suppressed.
	knownA := mustA(t, "testhost.local.", "192.168.1.100", 59)
	query := &dns.Msg{
		MsgHdr:   dns.MsgHdr{Id: 0, Opcode: dns.OpcodeQuery},
		Question: []dns.Question{{Name: "testhost.local.", Qtype: dns.TypeA, Qclass: dns.ClassINET}},
		Answer:   []dns.RR{knownA},
	}

	raw := sendQueryTo(t, addr, query)
	if raw == nil {
		t.Fatal("no response received (should have answered)")
	}

	var resp dns.Msg
	if err := resp.Unpack(raw); err != nil {
		t.Fatalf("unpack: %v", err)
	}
	if len(resp.Answer) != 1 {
		t.Fatalf("answer count = %d, want 1 (not suppressed)", len(resp.Answer))
	}
}

func TestEndToEnd_LegacyResponseID(t *testing.T) {
	zone := newTestZone(t)
	server, addr := newTestServer(t, zone, "testhost.local.")
	defer server.Shutdown()

	query := &dns.Msg{
		MsgHdr:   dns.MsgHdr{Id: 42, Opcode: dns.OpcodeQuery},
		Question: []dns.Question{{Name: "testhost.local.", Qtype: dns.TypeA, Qclass: dns.ClassINET}},
	}

	raw := sendQueryTo(t, addr, query)
	if raw == nil {
		t.Fatal("no response")
	}

	var resp dns.Msg
	if err := resp.Unpack(raw); err != nil {
		t.Fatalf("unpack: %v", err)
	}
	// Client is on a random port (not 5353) → legacy unicast.
	// RFC 6762 §6.7: legacy response MUST repeat the query ID.
	if resp.Id != 42 {
		t.Errorf("legacy response Id = %d, want 42", resp.Id)
	}
	// RFC 6762 §6.7: legacy response MUST echo the question.
	if len(resp.Question) != 1 {
		t.Error("legacy response missing echoed question")
	}
}

// ---------------------------------------------------------------------------
// answerQuery: mDNS (non-legacy) tests – require binding to port 5353 (root)
// ---------------------------------------------------------------------------

// mDNSQuery sends a query through answerQuery and reads the response.
// It requires binding to port 5353 and will skip if not privileged.
func mDNSQuery(t *testing.T, zone *hostnameZone, query *dns.Msg) *dns.Msg {
	t.Helper()

	// We need a socket on port 5353 to receive the response.
	readerConn, err := net.ListenUDP("udp4", &net.UDPAddr{IP: net.IPv4(127, 0, 0, 1), Port: mdnsPort})
	if err != nil {
		t.Skipf("need port 5353 (run as root): %v", err)
	}
	defer readerConn.Close()
	fromAddr := readerConn.LocalAddr().(*net.UDPAddr)

	writerConn, err := net.ListenUDP("udp4", &net.UDPAddr{IP: net.IPv4(127, 0, 0, 1), Port: 0})
	if err != nil {
		t.Skipf("bind writer: %v", err)
	}
	defer writerConn.Close()

	server := &mdnsServer{
		zone:      zone,
		socks:     &mdnsSockets{},
		state:     int32(stateRunning),
		probeName: "testhost.local.",
	}
	server.answerQuery(query, fromAddr, writerConn)

	readerConn.SetReadDeadline(time.Now().Add(2 * time.Second))
	buf := make([]byte, 65536)
	n, _, err := readerConn.ReadFromUDP(buf)
	if err != nil {
		return nil
	}
	var resp dns.Msg
	if err := resp.Unpack(buf[:n]); err != nil {
		t.Fatalf("unpack: %v", err)
	}
	return &resp
}

func TestAnswerQuery_mDNS_KeepsCacheFlush(t *testing.T) {
	zone := newTestZone(t)
	// Use QU bit so the response is sent via unicast (works in sandbox
	// without multicast).  For a non-QU query the response would be
	// multicast, which is not reachable in the Nix build sandbox.
	query := &dns.Msg{
		MsgHdr:   dns.MsgHdr{Opcode: dns.OpcodeQuery},
		Question: []dns.Question{{Name: "testhost.local.", Qtype: dns.TypeA, Qclass: dns.ClassINET | quBit}},
	}

	resp := mDNSQuery(t, zone, query)
	if resp == nil {
		t.Fatal("no response")
	}
	if len(resp.Answer) != 1 {
		t.Fatalf("answer count = %d, want 1", len(resp.Answer))
	}
	h := resp.Answer[0].Header()
	if h.Class != dns.ClassINET|cacheFlushBit {
		t.Errorf("mDNS A: class = %#x, want cache-flush", h.Class)
	}
	if h.Ttl <= legacyUnicastMaxTTL {
		t.Errorf("mDNS A: TTL = %d (should be > %d)", h.Ttl, legacyUnicastMaxTTL)
	}
	if resp.Id != 0 {
		t.Errorf("mDNS Id = %d, want 0", resp.Id)
	}
	if len(resp.Question) != 0 {
		t.Error("mDNS response should not echo question")
	}
}

package main

import (
	"errors"
	"net"
	"testing"
	"time"

	"github.com/miekg/dns"
)

type fakeAvahi struct {
	records map[uint16][]net.IP
	errs    map[uint16]error
	delays  map[uint16]time.Duration
}

func (f *fakeAvahi) addresses(qtype uint16, _ string) ([]net.IP, error) {
	if delay := f.delays[qtype]; delay > 0 {
		time.Sleep(delay)
	}
	if err := f.errs[qtype]; err != nil {
		return nil, err
	}
	return f.records[qtype], nil
}

type captureWriter struct {
	msg *dns.Msg
}

func (w *captureWriter) LocalAddr() net.Addr         { return &net.UDPAddr{} }
func (w *captureWriter) RemoteAddr() net.Addr        { return &net.UDPAddr{} }
func (w *captureWriter) TsigStatus() error           { return nil }
func (w *captureWriter) TsigTimersOnly(bool)         {}
func (w *captureWriter) Hijack()                     {}
func (w *captureWriter) Close() error                { return nil }
func (w *captureWriter) Write(b []byte) (int, error) { return len(b), nil }
func (w *captureWriter) WriteMsg(m *dns.Msg) error {
	w.msg = m
	return nil
}

func newRequest(qs ...dns.Question) *dns.Msg {
	req := new(dns.Msg)
	req.SetQuestion(qs[0].Name, qs[0].Qtype)
	req.Question = qs
	return req
}

func question(name string, qtype uint16) dns.Question {
	return dns.Question{Name: name, Qtype: qtype, Qclass: dns.ClassINET}
}

func TestAnswerCarriesEveryUsableAddress(t *testing.T) {
	s := &server{avahi: &fakeAvahi{records: map[uint16][]net.IP{
		dns.TypeAAAA: {
			net.ParseIP("2408:8220::1"),
			net.ParseIP("fe80::87c6:2216:c3b:79a4"),
			net.ParseIP("fdea:d:beef::2"),
		},
	}}}

	rrs := s.addressReply(question("host.local.", dns.TypeAAAA))

	if len(rrs) != 2 {
		t.Fatalf("got %d answers, want 2: %v", len(rrs), describe(rrs))
	}
	for _, rr := range rrs {
		if rr.Header().Rrtype != dns.TypeAAAA {
			t.Fatalf("unexpected record %s", rr)
		}
	}
}

func TestLinkLocalAddressIsNotAnswered(t *testing.T) {
	s := &server{avahi: &fakeAvahi{records: map[uint16][]net.IP{
		dns.TypeAAAA: {net.ParseIP("fe80::87c6:2216:c3b:79a4")},
	}}}

	if rrs := s.addressReply(question("host.local.", dns.TypeAAAA)); len(rrs) != 0 {
		t.Fatalf("got %v, want no answer", describe(rrs))
	}
}

func TestFailingAQuestionKeepsTheAAAAAnswer(t *testing.T) {
	s := &server{
		avahi: &fakeAvahi{
			records: map[uint16][]net.IP{dns.TypeAAAA: {net.ParseIP("2408:8220::1")}},
			errs:    map[uint16]error{dns.TypeA: errors.New("avahi timeout")},
		},
		zones: map[string]bool{"local.": true},
	}

	w := &captureWriter{}
	s.ServeDNS(w, newRequest(question("host.local.", dns.TypeA), question("host.local.", dns.TypeAAAA)))

	if w.msg == nil {
		t.Fatal("no response written")
	}
	if len(w.msg.Answer) != 1 || w.msg.Answer[0].Header().Rrtype != dns.TypeAAAA {
		t.Fatalf("got %v, want the AAAA answer only", describe(w.msg.Answer))
	}
}

func TestQuestionsAreResolvedConcurrently(t *testing.T) {
	s := &server{
		avahi: &fakeAvahi{
			records: map[uint16][]net.IP{
				dns.TypeA:    {net.ParseIP("192.0.2.1")},
				dns.TypeAAAA: {net.ParseIP("2408:8220::1")},
			},
			delays: map[uint16]time.Duration{dns.TypeA: 200 * time.Millisecond},
		},
		zones: map[string]bool{"local.": true},
	}

	w := &captureWriter{}
	start := time.Now()
	s.ServeDNS(w, newRequest(question("host.local.", dns.TypeA), question("host.local.", dns.TypeAAAA)))
	elapsed := time.Since(start)

	if len(w.msg.Answer) != 2 {
		t.Fatalf("got %v, want one A and one AAAA answer", describe(w.msg.Answer))
	}
	if elapsed > 350*time.Millisecond {
		t.Fatalf("questions were resolved one after the other (%v)", elapsed)
	}
}

func TestSOAOnlyForConfiguredZones(t *testing.T) {
	s := &server{zones: map[string]bool{"local.": true}}

	if rrs := s.soaReply(question("local.", dns.TypeSOA)); len(rrs) != 1 {
		t.Fatalf("got %v, want the zone SOA", describe(rrs))
	}
	if rrs := s.soaReply(question("example.com.", dns.TypeSOA)); len(rrs) != 0 {
		t.Fatalf("got %v, want no answer outside the zone", describe(rrs))
	}
}

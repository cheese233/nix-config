package main

import (
	"log"
	"sort"
	"strings"
	"sync"

	"github.com/miekg/dns"
)

// recordTTL is how long a bridged record may be cached. It matches the TTL
// mDNS responders usually announce, and is short enough to follow address
// changes on the network.
const recordTTL = 120

type server struct {
	avahi  resolver
	zones  map[string]bool
	v4only bool
	v6only bool
	debug  bool
}

func (s *server) ServeDNS(w dns.ResponseWriter, req *dns.Msg) {
	m := new(dns.Msg)
	m.SetReply(req)
	m.Question = append([]dns.Question(nil), req.Question...)
	m.Authoritative = true
	m.Compress = true

	if req.Opcode != dns.OpcodeQuery {
		m.Rcode = dns.RcodeNotImplemented
		s.write(w, m)
		return
	}
	if opt := req.IsEdns0(); opt != nil {
		m.SetEdns0(opt.UDPSize(), false)
	}

	// Each question is resolved on its own goroutine with its own avahi
	// lookup, so a slow or failing A lookup can neither cancel nor hold up the
	// AAAA answer of the same message.
	answers := make([][]dns.RR, len(req.Question))
	var wg sync.WaitGroup
	for i, q := range req.Question {
		switch {
		case q.Qtype == dns.TypeSOA:
			answers[i] = s.soaReply(q)
		case q.Qtype == dns.TypeA && !s.v6only,
			q.Qtype == dns.TypeAAAA && !s.v4only:
			wg.Add(1)
			go func(i int, q dns.Question) {
				defer wg.Done()
				answers[i] = s.addressReply(q)
			}(i, q)
		default:
			if s.debug {
				log.Printf("%s %s: unsupported question", dns.TypeToString[q.Qtype], q.Name)
			}
		}
	}
	wg.Wait()

	for _, rr := range answers {
		m.Answer = append(m.Answer, rr...)
	}
	s.write(w, m)
}

func (s *server) write(w dns.ResponseWriter, m *dns.Msg) {
	if err := w.WriteMsg(m); err != nil {
		log.Printf("write response: %v", err)
	}
}

// addressReply answers one A or AAAA question with every address avahi knows
// for the name, minus the ones a unicast DNS client cannot use.
func (s *server) addressReply(q dns.Question) []dns.RR {
	addresses, err := s.avahi.addresses(q.Qtype, q.Name)
	if err != nil {
		log.Printf("%s %s: %v", dns.TypeToString[q.Qtype], q.Name, err)
		return nil
	}

	rrs := make([]dns.RR, 0, len(addresses))
	for _, address := range addresses {
		// Link-local addresses are scoped to a single link: a client that
		// receives one cannot route to it.
		if address.IsLinkLocalUnicast() {
			continue
		}
		hdr := dns.RR_Header{Name: q.Name, Rrtype: q.Qtype, Class: dns.ClassINET, Ttl: recordTTL}
		switch {
		case q.Qtype == dns.TypeA && address.To4() != nil:
			rrs = append(rrs, &dns.A{Hdr: hdr, A: address.To4()})
		case q.Qtype == dns.TypeAAAA && address.To4() == nil:
			rrs = append(rrs, &dns.AAAA{Hdr: hdr, AAAA: address})
		}
	}

	if s.debug {
		log.Printf("%s %s: %s", dns.TypeToString[q.Qtype], q.Name, describe(rrs))
	}
	return rrs
}

func (s *server) soaReply(q dns.Question) []dns.RR {
	zone := strings.ToLower(q.Name)
	if !s.zones[zone] {
		return nil
	}
	return []dns.RR{&dns.SOA{
		Hdr:     dns.RR_Header{Name: q.Name, Rrtype: dns.TypeSOA, Class: dns.ClassINET, Ttl: recordTTL},
		Ns:      zone,
		Mbox:    "avahi2dns." + zone,
		Serial:  1,
		Refresh: 60,
		Retry:   60,
		Expire:  3600,
		Minttl:  60,
	}}
}

func (s *server) zonesList() string {
	names := make([]string, 0, len(s.zones))
	for zone := range s.zones {
		names = append(names, zone)
	}
	sort.Strings(names)
	return strings.Join(names, ", ")
}

func describe(rrs []dns.RR) string {
	if len(rrs) == 0 {
		return "no answer"
	}
	parts := make([]string, 0, len(rrs))
	for _, rr := range rrs {
		parts = append(parts, rr.String())
	}
	return strings.Join(parts, " | ")
}

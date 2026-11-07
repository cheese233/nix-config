package main

import (
	"context"
	"flag"
	"log"
	"net"
	"os"
	"os/signal"
	"strconv"
	"strings"
	"syscall"

	"github.com/miekg/dns"
)

// zones collects repeated -d flags.
type zoneList []string

func (z *zoneList) String() string { return strings.Join(*z, ", ") }

func (z *zoneList) Set(value string) error {
	*z = append(*z, value)
	return nil
}

// maxQuestions bounds how many questions one message may carry. unbound, the
// only client this bridge serves, sends one per message, but a message asking
// for A and AAAA together is exactly the case that has to stay independent.
const maxQuestions = 8

// acceptMsg replaces dns.DefaultMsgAcceptFunc, which only lets single question
// queries through.
func acceptMsg(dh dns.Header) dns.MsgAcceptAction {
	if dh.Bits&0x8000 != 0 {
		return dns.MsgIgnore // a response, not a query
	}
	if opcode := int(dh.Bits>>11) & 0xF; opcode != dns.OpcodeQuery {
		return dns.MsgRejectNotImplemented
	}
	if dh.Qdcount == 0 || dh.Qdcount > maxQuestions {
		return dns.MsgReject
	}
	return dns.MsgAccept
}

func main() {
	log.SetFlags(log.LstdFlags | log.Lmicroseconds)

	var (
		addr   = flag.String("a", "localhost", "address to listen on")
		port   = flag.Int("p", 53, "port to listen on")
		zones  zoneList
		v4only = flag.Bool("4", false, "only answer A queries")
		v6only = flag.Bool("6", false, "only answer AAAA queries")
		debug  = flag.Bool("v", false, "log every question and answer")
	)
	flag.Var(&zones, "d", "zone to bridge to mDNS, may be repeated")
	flag.Parse()

	if *v4only && *v6only {
		log.Fatal("cannot set both -4 and -6")
	}
	if len(zones) == 0 {
		zones = zoneList{"local"}
	}

	avahi, err := newAvahiClient()
	if err != nil {
		log.Fatalf("avahi: %v", err)
	}
	defer avahi.Close()

	s := &server{
		avahi:  avahi,
		zones:  make(map[string]bool, len(zones)),
		v4only: *v4only,
		v6only: *v6only,
		debug:  *debug,
	}

	mux := dns.NewServeMux()
	for _, zone := range zones {
		fqdn := dns.Fqdn(zone)
		s.zones[strings.ToLower(fqdn)] = true
		mux.Handle(fqdn, s)
	}

	dnsServer := &dns.Server{
		Addr:          net.JoinHostPort(*addr, strconv.Itoa(*port)),
		Net:           "udp",
		Handler:       mux,
		MsgAcceptFunc: acceptMsg,
	}

	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()

	go func() {
		<-ctx.Done()
		if err := dnsServer.Shutdown(); err != nil {
			log.Printf("shutdown: %v", err)
		}
	}()

	log.Printf("bridging %s to mDNS on %s", s.zonesList(), dnsServer.Addr)
	if err := dnsServer.ListenAndServe(); err != nil {
		log.Printf("stopped: %v", err)
	}
}

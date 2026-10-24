// Benchmarks of the per-message hot path. `make bench` runs them under `perf stat`
// (context switches, page faults, cycles, cache misses) and `perf record` (share of
// memmove/mallocgc), next to Go's own ns/op, B/op and allocs/op.
package main

import (
	"fmt"
	"testing"

	"kapa/whatsapp/pb"

	"go.mau.fi/whatsmeow/proto/waE2E"
	"go.mau.fi/whatsmeow/types"
	"google.golang.org/protobuf/proto"
	"google.golang.org/protobuf/types/known/timestamppb"
)

// sinkStr and sinkOK keep results alive so the compiler cannot drop the benchmarked work.
var (
	sinkStr string
	sinkOK  bool
)

// BenchmarkParsePhone measures phone normalization of an already clean number (no allocation expected).
func BenchmarkParsePhone(b *testing.B) {
	b.ReportAllocs()
	for b.Loop() {
		sinkStr, _ = parsePhone(testPhone)
	}
}

// BenchmarkPeerOf measures chat classification for phone and LID senders.
func BenchmarkPeerOf(b *testing.B) {
	cases := map[string]*types.MessageInfo{
		"phone": {MessageSource: types.MessageSource{Chat: pnJID("34699888777"), Sender: pnJID("34699888777")}},
		"lid":   {MessageSource: types.MessageSource{Chat: lidJID("123"), Sender: lidJID("123"), SenderAlt: pnJID("34699888777")}},
		"group": {MessageSource: types.MessageSource{Chat: types.NewJID("1-2", types.GroupServer), IsGroup: true}},
	}
	for name, info := range cases {
		b.Run(name, func(b *testing.B) {
			b.ReportAllocs()
			for b.Loop() {
				sinkStr, sinkOK = peerOf(info, testPhone, "", false, noResolve)
			}
		})
	}
}

// BenchmarkToMessage measures the conversion of a text message to its API form.
func BenchmarkToMessage(b *testing.B) {
	m := msgEvent("34699888777", &waE2E.Message{Conversation: proto.String("hola, ¿qué tal?")})
	b.ReportAllocs()
	for b.Loop() {
		sinkOK = toMessage(m, testPhone, "34699888777") != nil
	}
}

// drained registers n subscribers, each drained by its own goroutine like a real stream.
func drained(b *testing.B, s *server, n int) {
	b.Helper()
	stop := make(chan struct{})
	b.Cleanup(func() { close(stop) })
	for range n {
		sub, _ := newSubscriber(&pb.SubscribeRequest{})
		s.subs[sub] = struct{}{}
		go func() {
			for {
				select {
				case <-sub.ch:
				case <-stop:
					return
				}
			}
		}()
	}
}

// BenchmarkPublish measures fan-out of one event to N live subscribers (one shared pointer, no copies).
func BenchmarkPublish(b *testing.B) {
	for _, n := range []int{1, 10, 100} {
		b.Run(fmt.Sprintf("subs=%d", n), func(b *testing.B) {
			s := newTestServer()
			drained(b, s, n)
			ev := &pb.SubscribeResponse{Account: testPhone}
			b.ReportAllocs()
			for b.Loop() {
				s.publish(testPhone, "34699888777", ev)
			}
		})
	}
}

// BenchmarkOnMessage measures the whole incoming path of a text message with 10 subscribers,
// and the cost of an ignored message when nobody listens.
func BenchmarkOnMessage(b *testing.B) {
	m := msgEvent("34699888777", &waE2E.Message{Conversation: proto.String("hola")})
	b.Run("subs=10", func(b *testing.B) {
		s := newTestServer()
		drained(b, s, 10)
		b.ReportAllocs()
		for b.Loop() {
			s.onMessage(testPhone, "", nil, m)
		}
	})
	b.Run("subs=0", func(b *testing.B) {
		s := newTestServer()
		b.ReportAllocs()
		for b.Loop() {
			s.onMessage(testPhone, "", nil, m)
		}
	})
}

// BenchmarkEncode measures wire encoding of an event: the one unavoidable copy per subscriber.
func BenchmarkEncode(b *testing.B) {
	text := &pb.SubscribeResponse{Account: testPhone, Event: &pb.SubscribeResponse_Message{Message: &pb.Message{
		Id: "3EB0C767D26A1D6E", From: "34699888777", To: testPhone, SentAt: timestamppb.Now(), ReceivedAt: timestamppb.Now(),
		Chat: "34699888777@s.whatsapp.net", Content: &pb.Message_Text{Text: "hola"},
	}}}
	image := &pb.SubscribeResponse{Account: testPhone, Event: &pb.SubscribeResponse_Message{Message: &pb.Message{
		Id: "3EB0C767D26A1D6F", From: "34699888777", Content: &pb.Message_Image{Image: &pb.Media{Data: make([]byte, 1<<20), MimeType: "image/jpeg"}},
	}}}
	for name, ev := range map[string]*pb.SubscribeResponse{"text": text, "image-1MiB": image} {
		b.Run(name, func(b *testing.B) {
			b.SetBytes(int64(proto.Size(ev)))
			b.ReportAllocs()
			for b.Loop() {
				if _, err := proto.Marshal(ev); err != nil {
					b.Fatal(err)
				}
			}
		})
	}
}

// Unit tests: pure logic and the full gRPC surface over an in-memory connection.
// They need neither PostgreSQL nor WhatsApp and run with -race.
package main

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"net"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync"
	"testing"
	"time"

	"kapa/whatsapp/pb"

	"go.mau.fi/whatsmeow"
	"go.mau.fi/whatsmeow/proto/waE2E"
	"go.mau.fi/whatsmeow/store"
	"go.mau.fi/whatsmeow/types"
	"go.mau.fi/whatsmeow/types/events"
	"go.mau.fi/whatsmeow/util/keys"
	waLog "go.mau.fi/whatsmeow/util/log"
	"google.golang.org/grpc"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/credentials/insecure"
	"google.golang.org/grpc/health"
	healthpb "google.golang.org/grpc/health/grpc_health_v1"
	"google.golang.org/grpc/status"
	"google.golang.org/grpc/test/bufconn"
	"google.golang.org/protobuf/proto"
)

// testPhone is the account used by unit tests (never contacted: no network is involved).
const testPhone = "34600111222"

// newTestServer returns a server without database, enough for everything but pairing.
func newTestServer() *server {
	return &server{
		log: waLog.Noop, health: health.NewServer(),
		accounts: map[string]*whatsmeow.Client{}, subs: map[*subscriber]struct{}{},
	}
}

// fakeClient returns an offline client; linked gives it a device ID like a paired session.
func fakeClient(phone string, linked bool) *whatsmeow.Client {
	dev := &store.Device{NoiseKey: keys.NewKeyPair()}
	if linked {
		jid := types.NewJID(phone, types.DefaultUserServer)
		dev.ID = &jid
	}
	return whatsmeow.NewClient(dev, waLog.Noop)
}

// pnJID builds a phone-number JID.
func pnJID(user string) types.JID { return types.NewJID(user, types.DefaultUserServer) }

// lidJID builds a LID (hidden user) JID.
func lidJID(user string) types.JID { return types.NewJID(user, types.HiddenUserServer) }

// noResolve is a LID resolver that knows nothing.
func noResolve(types.JID) types.JID { return types.EmptyJID }

// addSub registers a subscriber directly, bypassing gRPC.
func addSub(t *testing.T, s *server, req *pb.SubscribeRequest, buffer int) *subscriber {
	t.Helper()
	sub, err := newSubscriber(req)
	if err != nil {
		t.Fatalf("newSubscriber: %v", err)
	}
	sub.ch = make(chan *pb.SubscribeResponse, buffer)
	s.subMu.Lock()
	s.subs[sub] = struct{}{}
	s.subMu.Unlock()
	return sub
}

// next returns the next queued event of sub, failing if none arrives in time.
func next(t *testing.T, sub *subscriber) *pb.SubscribeResponse {
	t.Helper()
	select {
	case ev := <-sub.ch:
		return ev
	case <-time.After(2 * time.Second):
		t.Fatal("no event received")
		return nil
	}
}

// empty fails if sub has a queued event.
func empty(t *testing.T, sub *subscriber) {
	t.Helper()
	select {
	case ev := <-sub.ch:
		t.Fatalf("unexpected event: %v", ev)
	default:
	}
}

// startBuf serves s over an in-memory listener and returns a client.
func startBuf(t *testing.T, s *server) pb.WhatsAppServiceClient {
	t.Helper()
	lis := bufconn.Listen(1 << 20)
	g := s.grpcServer()
	go func() { _ = g.Serve(lis) }()
	t.Cleanup(g.Stop)
	conn, err := grpc.NewClient("passthrough:///bufnet",
		grpc.WithContextDialer(func(ctx context.Context, _ string) (net.Conn, error) { return lis.DialContext(ctx) }),
		grpc.WithTransportCredentials(insecure.NewCredentials()))
	if err != nil {
		t.Fatalf("dial: %v", err)
	}
	t.Cleanup(func() { _ = conn.Close() })
	return pb.NewWhatsAppServiceClient(conn)
}

// waitSubs waits until the server has exactly n subscribers.
func waitSubs(t *testing.T, s *server, n int) {
	t.Helper()
	deadline := time.Now().Add(3 * time.Second)
	for time.Now().Before(deadline) {
		s.subMu.RLock()
		got := len(s.subs)
		s.subMu.RUnlock()
		if got == n {
			return
		}
		time.Sleep(5 * time.Millisecond)
	}
	t.Fatalf("expected %d subscribers", n)
}

// code returns the gRPC status code of err.
func code(err error) codes.Code { return status.Code(err) }

// TestParsePhone checks normalization and E.164 validation.
func TestParsePhone(t *testing.T) {
	cases := []struct {
		in, want string
		ok       bool
	}{
		{"34600111222", "34600111222", true},
		{"+34 600 111 222", "34600111222", true},
		{"+1-555-010-9999", "15550109999", true},
		{"1234567", "1234567", true},
		{"123456789012345", "123456789012345", true},
		{"123456", "", false},           // too short
		{"1234567890123456", "", false}, // too long for E.164
		{"0034600111222", "", false},    // national/trunk prefix
		{"34600abc222", "", false},
		{"34(600)111222", "", false},
		{"", "", false},
	}
	for _, c := range cases {
		got, err := parsePhone(c.in)
		if c.ok != (err == nil) || got != c.want {
			t.Errorf("parsePhone(%q) = %q, %v; want %q ok=%v", c.in, got, err, c.want, c.ok)
		}
		if err != nil && code(err) != codes.InvalidArgument {
			t.Errorf("parsePhone(%q) code = %v, want InvalidArgument", c.in, code(err))
		}
	}
}

// TestPhoneSet checks filter parsing, including verbatim LID entries.
func TestPhoneSet(t *testing.T) {
	set, err := phoneSet([]string{"+34 600 111 222", "lid:987654321", "34600111222"})
	if err != nil {
		t.Fatal(err)
	}
	if len(set) != 2 {
		t.Fatalf("want 2 unique entries, got %v", set)
	}
	for _, k := range []string{"34600111222", "lid:987654321"} {
		if _, ok := set[k]; !ok {
			t.Errorf("missing %q in %v", k, set)
		}
	}
	if _, err = phoneSet([]string{"nope"}); code(err) != codes.InvalidArgument {
		t.Errorf("invalid entry: got %v", err)
	}
}

// TestPeerOf checks which chats are delivered and how the peer phone is resolved.
func TestPeerOf(t *testing.T) {
	const own, ownLID = testPhone, "111111111111111"
	resolver := func(l types.JID) types.JID {
		if l.User == "222" {
			return pnJID("34611222333")
		}
		return types.EmptyJID
	}
	sender := pnJID("34699888777")
	sender.Device = 5
	cases := []struct {
		name     string
		src      types.MessageSource
		selfChat bool
		want     string
		ok       bool
	}{
		{"phone sender", types.MessageSource{Chat: pnJID("34699888777"), Sender: sender}, false, "34699888777", true},
		{"lid sender with alt", types.MessageSource{Chat: lidJID("123"), Sender: lidJID("123"), SenderAlt: pnJID("34622000111")}, false, "34622000111", true},
		{"lid sender resolved", types.MessageSource{Chat: lidJID("222"), Sender: lidJID("222")}, false, "34611222333", true},
		{"lid sender unknown", types.MessageSource{Chat: lidJID("333"), Sender: lidJID("333")}, false, "lid:333", true},
		{"group", types.MessageSource{Chat: types.NewJID("123-456", types.GroupServer), Sender: sender, IsGroup: true}, false, "", false},
		{"status update", types.MessageSource{Chat: types.StatusBroadcastJID, Sender: sender}, false, "", false},
		{"broadcast list", types.MessageSource{Chat: types.NewJID("777", types.BroadcastServer), Sender: sender}, false, "", false},
		{"channel", types.MessageSource{Chat: types.NewJID("888", types.NewsletterServer), Sender: sender}, false, "", false},
		{"own message to contact", types.MessageSource{Chat: pnJID("34699888777"), Sender: pnJID(own), IsFromMe: true}, true, "", false},
		{"self chat disabled", types.MessageSource{Chat: pnJID(own), Sender: pnJID(own), IsFromMe: true}, false, "", false},
		{"self chat enabled", types.MessageSource{Chat: pnJID(own), Sender: pnJID(own), IsFromMe: true}, true, own, true},
		{"self chat via own lid", types.MessageSource{Chat: lidJID(ownLID), Sender: lidJID(ownLID), IsFromMe: true}, true, own, true},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			info := &types.MessageInfo{MessageSource: c.src}
			got, ok := peerOf(info, own, ownLID, c.selfChat, resolver)
			if ok != c.ok || (ok && got != c.want) {
				t.Fatalf("got %q,%v want %q,%v", got, ok, c.want, c.ok)
			}
		})
	}
}

// TestPeerOfUnknownOwnLID ensures an empty own LID never matches an empty chat user.
func TestPeerOfUnknownOwnLID(t *testing.T) {
	info := &types.MessageInfo{MessageSource: types.MessageSource{Chat: lidJID(""), IsFromMe: true}}
	if _, ok := peerOf(info, testPhone, "", true, noResolve); ok {
		t.Fatal("empty LID chat must not count as self chat")
	}
}

// msgEvent builds an incoming message event from a phone-number sender.
func msgEvent(from string, content *waE2E.Message) *events.Message {
	return &events.Message{
		Info: types.MessageInfo{
			MessageSource: types.MessageSource{Chat: pnJID(from), Sender: pnJID(from)},
			ID:            "MSGID", PushName: "Ana", Timestamp: time.Unix(1_700_000_000, 0),
		},
		Message: content,
	}
}

// TestToMessage checks the conversion of every supported and some unsupported payloads.
func TestToMessage(t *testing.T) {
	cases := []struct {
		name  string
		in    *waE2E.Message
		check func(*pb.Message) bool
	}{
		{
			"conversation", &waE2E.Message{Conversation: proto.String("hola")},
			func(m *pb.Message) bool { return m.GetText() == "hola" },
		},
		{
			"extended text", &waE2E.Message{ExtendedTextMessage: &waE2E.ExtendedTextMessage{Text: proto.String("https://kapa.dev")}},
			func(m *pb.Message) bool { return m.GetText() == "https://kapa.dev" },
		},
		{
			"image", &waE2E.Message{ImageMessage: &waE2E.ImageMessage{Mimetype: proto.String("image/jpeg"), Caption: proto.String("cap")}},
			func(m *pb.Message) bool {
				return m.GetImage().GetMimeType() == "image/jpeg" && m.GetImage().GetCaption() == "cap" && m.GetImage().GetData() == nil
			},
		},
		{
			"voice note", &waE2E.Message{AudioMessage: &waE2E.AudioMessage{Mimetype: proto.String("audio/ogg; codecs=opus"), PTT: proto.Bool(true), Seconds: proto.Uint32(7)}},
			func(m *pb.Message) bool { return m.GetAudio().GetVoiceNote() && m.GetAudio().GetSeconds() == 7 },
		},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			m := toMessage(msgEvent("34699888777", c.in), "34699888777")
			if m == nil || !c.check(m) {
				t.Fatalf("unexpected conversion: %v", m)
			}
			if m.GetId() != "MSGID" || m.GetFrom() != "34699888777" || m.GetPushName() != "Ana" || m.GetTimestamp() != 1_700_000_000 || m.GetFromMe() {
				t.Fatalf("bad envelope: %v", m)
			}
		})
	}
	unsupported := []*waE2E.Message{
		nil,
		{},
		{ReactionMessage: &waE2E.ReactionMessage{Text: proto.String("👍")}},
		{StickerMessage: &waE2E.StickerMessage{}},
		{VideoMessage: &waE2E.VideoMessage{}},
		{DocumentMessage: &waE2E.DocumentMessage{}},
		{ProtocolMessage: &waE2E.ProtocolMessage{}},
		{ExtendedTextMessage: &waE2E.ExtendedTextMessage{}},
	}
	for i, in := range unsupported {
		if m := toMessage(msgEvent("34699888777", in), "34699888777"); m != nil {
			t.Errorf("unsupported #%d converted: %v", i, m)
		}
	}
}

// TestSubscriberMatches checks account and peer filters.
func TestSubscriberMatches(t *testing.T) {
	all := &subscriber{}
	acc := &subscriber{accounts: map[string]struct{}{"1111111": {}}}
	peer := &subscriber{peers: map[string]struct{}{"2222222": {}}}
	both := &subscriber{accounts: map[string]struct{}{"1111111": {}}, peers: map[string]struct{}{"2222222": {}}}
	cases := []struct {
		sub           *subscriber
		account, peer string
		want          bool
	}{
		{all, "1111111", "2222222", true},
		{all, "9999999", "", true},
		{acc, "1111111", "8888888", true},
		{acc, "9999999", "2222222", false},
		{peer, "9999999", "2222222", true},
		{peer, "9999999", "8888888", false},
		{peer, "9999999", "", true}, // account events ignore peer filters
		{both, "1111111", "2222222", true},
		{both, "1111111", "8888888", false},
		{both, "9999999", "", false},
	}
	for i, c := range cases {
		if got := c.sub.matches(c.account, c.peer); got != c.want {
			t.Errorf("case %d: matches(%q,%q) = %v, want %v", i, c.account, c.peer, got, c.want)
		}
	}
}

// TestPublishZeroCopyAndFilters checks that every matching subscriber gets the very same
// event pointer and that non-matching subscribers get nothing.
func TestPublishZeroCopyAndFilters(t *testing.T) {
	s := newTestServer()
	a := addSub(t, s, &pb.SubscribeRequest{}, 4)
	b := addSub(t, s, &pb.SubscribeRequest{Peers: []string{"34699888777"}}, 4)
	c := addSub(t, s, &pb.SubscribeRequest{Accounts: []string{"34600999999"}}, 4)
	ev := &pb.SubscribeResponse{Account: testPhone}
	s.publish(testPhone, "34699888777", ev)
	if got := next(t, a); got != ev {
		t.Fatal("subscriber a got a copy, want the same pointer")
	}
	if got := next(t, b); got != ev {
		t.Fatal("subscriber b got a copy, want the same pointer")
	}
	empty(t, c)
}

// TestPublishDropsSlowSubscriber checks that a full queue drops that subscriber only and never blocks.
func TestPublishDropsSlowSubscriber(t *testing.T) {
	s := newTestServer()
	slow := addSub(t, s, &pb.SubscribeRequest{}, 1)
	fast := addSub(t, s, &pb.SubscribeRequest{}, 8)
	done := make(chan struct{})
	go func() {
		for range 3 {
			s.publish(testPhone, "", &pb.SubscribeResponse{})
		}
		close(done)
	}()
	select {
	case <-done:
	case <-time.After(2 * time.Second):
		t.Fatal("publish blocked on a slow subscriber")
	}
	select {
	case <-slow.gone:
	default:
		t.Fatal("slow subscriber was not dropped")
	}
	select {
	case <-fast.gone:
		t.Fatal("fast subscriber was dropped")
	default:
	}
	if len(fast.ch) != 3 {
		t.Fatalf("fast subscriber got %d events, want 3", len(fast.ch))
	}
	slow.drop() // idempotent
}

// TestOnMessageNoWorkWithoutSubscribers proves media is neither converted nor downloaded
// when nobody listens: the client is nil, so any download attempt would panic.
func TestOnMessageNoWorkWithoutSubscribers(t *testing.T) {
	s := newTestServer()
	img := msgEvent("34699888777", &waE2E.Message{ImageMessage: &waE2E.ImageMessage{Mimetype: proto.String("image/jpeg")}})
	s.onMessage(testPhone, "", nil, img)
	other := addSub(t, s, &pb.SubscribeRequest{Peers: []string{"34611000000"}}, 4)
	s.onMessage(testPhone, "", nil, img)
	empty(t, other)
}

// TestOnMessageDelivers checks the full path for a text message and the discard of groups.
func TestOnMessageDelivers(t *testing.T) {
	s := newTestServer()
	sub := addSub(t, s, &pb.SubscribeRequest{Accounts: []string{testPhone}}, 4)
	s.onMessage(testPhone, "", nil, msgEvent("34699888777", &waE2E.Message{Conversation: proto.String("hola")}))
	ev := next(t, sub)
	if ev.GetAccount() != testPhone || ev.GetMessage().GetText() != "hola" || ev.GetMessage().GetFrom() != "34699888777" {
		t.Fatalf("unexpected event: %v", ev)
	}
	group := msgEvent("34699888777", &waE2E.Message{Conversation: proto.String("x")})
	group.Info.IsGroup = true
	group.Info.Chat = types.NewJID("123-456", types.GroupServer)
	s.onMessage(testPhone, "", nil, group)
	unsupported := msgEvent("34699888777", &waE2E.Message{StickerMessage: &waE2E.StickerMessage{}})
	s.onMessage(testPhone, "", nil, unsupported)
	empty(t, sub)
}

// TestHandlerLifecycle checks every lifecycle event the handler translates.
func TestHandlerLifecycle(t *testing.T) {
	s := newTestServer()
	cli := s.attach(testPhone, fakeClient(testPhone, true))
	sub := addSub(t, s, &pb.SubscribeRequest{}, 16)
	h := s.handler(testPhone, cli)
	cases := []struct {
		evt  any
		want pb.AccountState
	}{
		{&events.Connected{}, pb.AccountState_ACCOUNT_STATE_CONNECTED},
		{&events.Disconnected{}, pb.AccountState_ACCOUNT_STATE_DISCONNECTED},
		{&events.StreamReplaced{}, pb.AccountState_ACCOUNT_STATE_DISCONNECTED},
		{&events.TemporaryBan{Expire: time.Hour}, pb.AccountState_ACCOUNT_STATE_BANNED},
		{&events.ClientOutdated{}, pb.AccountState_ACCOUNT_STATE_OUTDATED},
		{&events.LoggedOut{Reason: events.ConnectFailureLoggedOut}, pb.AccountState_ACCOUNT_STATE_LOGGED_OUT},
	}
	for _, c := range cases {
		h(c.evt)
		ev := next(t, sub)
		if got := ev.GetAccountEvent().GetState(); got != c.want || ev.GetAccount() != testPhone {
			t.Fatalf("%T: got %v for %q, want %v", c.evt, got, ev.GetAccount(), c.want)
		}
	}
	if !s.outdated.Load() {
		t.Fatal("ClientOutdated did not flag the server")
	}
	resp, err := s.health.Check(context.Background(), &healthpb.HealthCheckRequest{})
	if err != nil || resp.GetStatus() != healthpb.HealthCheckResponse_NOT_SERVING {
		t.Fatalf("health after ClientOutdated = %v, %v; want NOT_SERVING", resp, err)
	}
	if _, _, err = s.lookup(testPhone); code(err) != codes.NotFound {
		t.Fatalf("LoggedOut must forget the account, lookup err = %v", err)
	}
	h(&events.Receipt{}) // ignored event types are a no-op
	empty(t, sub)
}

// TestWatchPairing checks readiness signalling and cleanup of failed pairings.
func TestWatchPairing(t *testing.T) {
	t.Run("code then timeout", func(t *testing.T) {
		s := newTestServer()
		cli := s.attach(testPhone, fakeClient(testPhone, false))
		sub := addSub(t, s, &pb.SubscribeRequest{}, 4)
		qr, ready := make(chan whatsmeow.QRChannelItem, 4), make(chan error, 1)
		qr <- whatsmeow.QRChannelItem{Event: whatsmeow.QRChannelEventCode, Code: "c1"}
		qr <- whatsmeow.QRChannelItem{Event: whatsmeow.QRChannelEventCode, Code: "c2"}
		qr <- whatsmeow.QRChannelTimeout
		close(qr)
		s.watchPairing(testPhone, cli, qr, ready)
		if err := <-ready; err != nil {
			t.Fatalf("first code must signal ready, got %v", err)
		}
		select {
		case err := <-ready:
			t.Fatalf("ready must receive exactly one value, got another: %v", err)
		default:
		}
		// The second rotation tried to refresh the code; offline, that fails and publishes nothing.
		if ev := next(t, sub); ev.GetAccountEvent().GetState() != pb.AccountState_ACCOUNT_STATE_PAIR_FAILED {
			t.Fatalf("want PAIR_FAILED (and no PAIRING from a failed refresh), got %v", ev)
		}
		if _, _, err := s.lookup(testPhone); code(err) != codes.NotFound {
			t.Fatal("failed pairing must forget the account")
		}
	})
	t.Run("error before code", func(t *testing.T) {
		s := newTestServer()
		cli := s.attach(testPhone, fakeClient(testPhone, false))
		qr, ready := make(chan whatsmeow.QRChannelItem, 1), make(chan error, 1)
		qr <- whatsmeow.QRChannelClientOutdated
		close(qr)
		s.watchPairing(testPhone, cli, qr, ready)
		if err := <-ready; err == nil {
			t.Fatal("an early failure must be reported to the caller")
		}
	})
	t.Run("success keeps account", func(t *testing.T) {
		s := newTestServer()
		cli := s.attach(testPhone, fakeClient(testPhone, false))
		qr, ready := make(chan whatsmeow.QRChannelItem, 2), make(chan error, 1)
		qr <- whatsmeow.QRChannelItem{Event: whatsmeow.QRChannelEventCode}
		qr <- whatsmeow.QRChannelSuccess
		s.watchPairing(testPhone, cli, qr, ready)
		if _, _, err := s.lookup(testPhone); err != nil {
			t.Fatalf("successful pairing must keep the account: %v", err)
		}
	})
}

// TestPairingError checks WhatsApp's rate limit is reported as RESOURCE_EXHAUSTED.
func TestPairingError(t *testing.T) {
	limited := []error{
		whatsmeow.ErrIQRateOverLimit,
		&whatsmeow.IQError{Code: 429, Text: "rate-overlimit"}, // what the server actually returns
		fmt.Errorf("pair: %w", whatsmeow.ErrIQRateOverLimit),
	}
	for _, err := range limited {
		got := pairingError(testPhone, err)
		if code(got) != codes.ResourceExhausted || !strings.Contains(got.Error(), "rate-limiting") {
			t.Errorf("pairingError(%v) = %v, want RESOURCE_EXHAUSTED", err, got)
		}
	}
	if got := pairingError(testPhone, errors.New("socket closed")); code(got) != codes.Unavailable {
		t.Errorf("other errors must be UNAVAILABLE, got %v", got)
	}
}

// TestForgetOnlyOwnClient ensures a stale client cannot unregister its replacement.
func TestForgetOnlyOwnClient(t *testing.T) {
	s := newTestServer()
	old := fakeClient(testPhone, false)
	cur := s.attach(testPhone, fakeClient(testPhone, true))
	s.forget(testPhone, old)
	if _, got, _ := s.lookup(testPhone); got != cur {
		t.Fatal("forget removed a different client")
	}
}

// TestRPCValidation covers every error path reachable without WhatsApp.
func TestRPCValidation(t *testing.T) {
	s := newTestServer()
	s.attach("34600000001", fakeClient("34600000001", false)) // pairing
	s.attach("34600000002", fakeClient("34600000002", true))  // linked, offline
	c := startBuf(t, s)
	ctx := context.Background()
	img := &pb.SendRequest_Image{Image: &pb.Media{Data: []byte{1}}}
	cases := []struct {
		name string
		call func() error
		want codes.Code
	}{
		{"login bad phone", func() error { _, err := c.Login(ctx, &pb.LoginRequest{Phone: "12"}); return err }, codes.InvalidArgument},
		{"logout unknown", func() error { _, err := c.Logout(ctx, &pb.LogoutRequest{Phone: "34600000009"}); return err }, codes.NotFound},
		{"logout pairing", func() error { _, err := c.Logout(ctx, &pb.LogoutRequest{Phone: "34600000001"}); return err }, codes.FailedPrecondition},
		{"logout bad phone", func() error { _, err := c.Logout(ctx, &pb.LogoutRequest{Phone: "x"}); return err }, codes.InvalidArgument},
		{"delete unknown", func() error { _, err := c.Delete(ctx, &pb.DeleteRequest{Phone: "34600000009"}); return err }, codes.NotFound},
		{"send unknown", func() error {
			_, err := c.Send(ctx, &pb.SendRequest{Account: "34600000009", To: testPhone})
			return err
		}, codes.NotFound},
		{"send pairing", func() error {
			_, err := c.Send(ctx, &pb.SendRequest{Account: "34600000001", To: testPhone})
			return err
		}, codes.FailedPrecondition},
		{"send bad to", func() error { _, err := c.Send(ctx, &pb.SendRequest{Account: "34600000002", To: "1"}); return err }, codes.InvalidArgument},
		{"send no content", func() error {
			_, err := c.Send(ctx, &pb.SendRequest{Account: "34600000002", To: testPhone})
			return err
		}, codes.InvalidArgument},
		{"send empty text", func() error {
			_, err := c.Send(ctx, &pb.SendRequest{Account: "34600000002", To: testPhone, Content: &pb.SendRequest_Text{}})
			return err
		}, codes.InvalidArgument},
		{"send image without mime", func() error {
			_, err := c.Send(ctx, &pb.SendRequest{Account: "34600000002", To: testPhone, Content: img})
			return err
		}, codes.InvalidArgument},
		{"send audio without data", func() error {
			_, err := c.Send(ctx, &pb.SendRequest{Account: "34600000002", To: testPhone, Content: &pb.SendRequest_Audio{Audio: &pb.Media{MimeType: "audio/ogg"}}})
			return err
		}, codes.InvalidArgument},
	}
	for _, tc := range cases {
		if got := code(tc.call()); got != tc.want {
			t.Errorf("%s: code %v, want %v", tc.name, got, tc.want)
		}
	}
}

// TestLoginExistingAccount checks Login is idempotent and never re-pairs an existing account.
func TestLoginExistingAccount(t *testing.T) {
	s := newTestServer()
	s.attach("34600000001", fakeClient("34600000001", false))
	s.attach("34600000002", fakeClient("34600000002", true))
	c := startBuf(t, s)
	for phone, want := range map[string]pb.AccountState{
		"+34 600 000 001": pb.AccountState_ACCOUNT_STATE_PAIRING,
		"34600000002":     pb.AccountState_ACCOUNT_STATE_DISCONNECTED,
	} {
		resp, err := c.Login(context.Background(), &pb.LoginRequest{Phone: phone})
		if err != nil || resp.GetPairingCode() != "" || resp.GetState() != want {
			t.Errorf("Login(%s) = %v, %v; want state %v and no code", phone, resp, err, want)
		}
	}
}

// TestDeletePendingPairing checks Delete cancels a pairing and notifies subscribers.
func TestDeletePendingPairing(t *testing.T) {
	s := newTestServer()
	s.attach(testPhone, fakeClient(testPhone, false))
	sub := addSub(t, s, &pb.SubscribeRequest{}, 4)
	c := startBuf(t, s)
	if _, err := c.Delete(context.Background(), &pb.DeleteRequest{Phone: "+" + testPhone}); err != nil {
		t.Fatal(err)
	}
	if ev := next(t, sub); ev.GetAccountEvent().GetState() != pb.AccountState_ACCOUNT_STATE_DELETED {
		t.Fatalf("want DELETED, got %v", ev)
	}
	st, err := c.Status(context.Background(), &pb.StatusRequest{})
	if err != nil || len(st.GetAccounts()) != 0 {
		t.Fatalf("account still listed: %v, %v", st, err)
	}
}

// TestStatus checks sorting, states and versions.
func TestStatus(t *testing.T) {
	s := newTestServer()
	s.attach("34600000003", fakeClient("34600000003", true))
	s.attach("34600000001", fakeClient("34600000001", false))
	st, err := startBuf(t, s).Status(context.Background(), &pb.StatusRequest{})
	if err != nil {
		t.Fatal(err)
	}
	if len(st.GetAccounts()) != 2 || st.GetAccounts()[0].GetPhone() != "34600000001" ||
		st.GetAccounts()[0].GetState() != pb.AccountState_ACCOUNT_STATE_PAIRING ||
		st.GetAccounts()[1].GetState() != pb.AccountState_ACCOUNT_STATE_DISCONNECTED {
		t.Fatalf("unexpected accounts: %v", st.GetAccounts())
	}
	if !strings.HasPrefix(st.GetWhatsmeowVersion(), "v") || len(versionParts(st.GetWaVersion())) != 3 || st.GetOutdated() {
		t.Fatalf("unexpected versions: %v", st)
	}
}

// TestSubscribeStream checks filters, delivery, slow-consumer and shutdown endings over gRPC.
func TestSubscribeStream(t *testing.T) {
	s := newTestServer()
	c := startBuf(t, s)
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	bad, err := c.Subscribe(ctx, &pb.SubscribeRequest{Peers: []string{"nope"}})
	if err == nil {
		_, err = bad.Recv()
	}
	if code(err) != codes.InvalidArgument {
		t.Fatalf("invalid filter: %v", err)
	}

	stream, err := c.Subscribe(ctx, &pb.SubscribeRequest{Accounts: []string{testPhone}})
	if err != nil {
		t.Fatal(err)
	}
	waitSubs(t, s, 1)
	s.notify("34600999999", pb.AccountState_ACCOUNT_STATE_CONNECTED, "") // filtered out
	s.notify(testPhone, pb.AccountState_ACCOUNT_STATE_CONNECTED, "hi")
	ev, err := stream.Recv()
	if err != nil || ev.GetAccount() != testPhone || ev.GetAccountEvent().GetDetail() != "hi" {
		t.Fatalf("Recv = %v, %v", ev, err)
	}
	s.shutdown()
	if _, err = stream.Recv(); code(err) != codes.ResourceExhausted {
		t.Fatalf("after shutdown: %v, want ResourceExhausted", err)
	}
	waitSubs(t, s, 0)
}

// TestSubscribeClientCancel checks the subscriber is removed when the client goes away.
func TestSubscribeClientCancel(t *testing.T) {
	s := newTestServer()
	c := startBuf(t, s)
	ctx, cancel := context.WithCancel(context.Background())
	if _, err := c.Subscribe(ctx, &pb.SubscribeRequest{}); err != nil {
		t.Fatal(err)
	}
	waitSubs(t, s, 1)
	cancel()
	waitSubs(t, s, 0)
}

// TestHealthAndStatusProbes checks the container probes against a real TCP listener and
// a fake upstream, covering every exit code.
func TestHealthAndStatusProbes(t *testing.T) {
	s := newTestServer()
	lis, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	g := s.grpcServer()
	go func() { _ = g.Serve(lis) }()
	t.Cleanup(g.Stop)
	addr := lis.Addr().String()
	ours := store.GetWAVersion()

	upstream := fmt.Sprintf("var waVersion = WAVersionContainer{%d, %d, %d}", ours[0], ours[1], ours[2])
	fake := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path == "/missing" {
			http.NotFound(w, r)
			return
		}
		_, _ = w.Write([]byte(upstream))
	}))
	t.Cleanup(fake.Close)
	defer func(u string) { upstreamVersionURL = u }(upstreamVersionURL)

	if got := probeHealth(addr); got != 0 {
		t.Fatalf("probeHealth = %d, want 0", got)
	}
	upstreamVersionURL = fake.URL
	assertStatus(t, addr, 0, "up to date")
	upstream = fmt.Sprintf("var waVersion = WAVersionContainer{%d, %d, %d}", ours[0], ours[1], ours[2]+1)
	assertStatus(t, addr, 2, "UPDATE AVAILABLE")
	upstreamVersionURL = fake.URL + "/missing"
	assertStatus(t, addr, 0, "check skipped")
	s.setOutdated()
	assertStatus(t, addr, 1, "OUTDATED")
	if got := probeHealth(addr); got != 1 {
		t.Fatalf("probeHealth after outdated = %d, want 1", got)
	}
	if got := probeHealth("127.0.0.1:1"); got != 1 {
		t.Fatalf("probeHealth without server = %d, want 1", got)
	}
}

// assertStatus runs printStatus and checks its exit code and output.
func assertStatus(t *testing.T, addr string, want int, contains string) {
	t.Helper()
	var out bytes.Buffer
	if got := printStatus(addr, &out); got != want || !strings.Contains(out.String(), contains) {
		t.Fatalf("printStatus = %d %q; want %d containing %q", got, out.String(), want, contains)
	}
}

// TestVersionHelpers checks protocol version parsing and comparison.
func TestVersionHelpers(t *testing.T) {
	got, err := parseWAVersion([]byte("x\nvar waVersion = WAVersionContainer{2, 3000, 1050728964}\ny"))
	if err != nil || got != "2.3000.1050728964" {
		t.Fatalf("parseWAVersion = %q, %v", got, err)
	}
	if _, err = parseWAVersion([]byte("nothing here")); err == nil {
		t.Fatal("missing version must fail")
	}
	cases := []struct {
		a, b string
		want bool
	}{
		{"2.3000.11", "2.3000.10", true},
		{"2.3000.10", "2.3000.10", false},
		{"2.3000.9", "2.3000.10", false},
		{"3.0.0", "2.9999.9999", true},
		{"2.3001.0", "2.3000.99", true},
	}
	for _, c := range cases {
		if got := newer(c.a, c.b); got != c.want {
			t.Errorf("newer(%s,%s) = %v", c.a, c.b, got)
		}
	}
}

// TestSmallHelpers covers env, localAddr and whatsmeowVersion.
func TestSmallHelpers(t *testing.T) {
	t.Setenv("KAPA_TEST_ENV", "")
	if env("KAPA_TEST_ENV", "def") != "def" {
		t.Fatal("empty env must fall back to default")
	}
	t.Setenv("KAPA_TEST_ENV", "set")
	if env("KAPA_TEST_ENV", "def") != "set" {
		t.Fatal("env must return the variable")
	}
	if localAddr(":50051") != "localhost:50051" || localAddr("10.0.0.1:1") != "10.0.0.1:1" {
		t.Fatal("localAddr")
	}
	if v := whatsmeowVersion(); !strings.HasPrefix(v, "v") {
		t.Fatalf("whatsmeowVersion = %q", v)
	}
}

// TestConcurrentAccountsAndSubscribers hammers every shared structure at once (run with -race):
// many accounts appear and disappear while subscribers come and go and events flow.
func TestConcurrentAccountsAndSubscribers(t *testing.T) {
	s := newTestServer()
	c := startBuf(t, s)
	var wg sync.WaitGroup
	stop := make(chan struct{})
	for i := range 8 {
		wg.Go(func() {
			phone := fmt.Sprintf("3460000%04d", i)
			for {
				select {
				case <-stop:
					return
				default:
				}
				cli := s.attach(phone, fakeClient(phone, i%2 == 0))
				s.notify(phone, pb.AccountState_ACCOUNT_STATE_CONNECTED, "")
				s.publish(phone, "34699888777", &pb.SubscribeResponse{Account: phone})
				s.forget(phone, cli)
			}
		})
	}
	for range 4 {
		wg.Go(func() {
			for {
				select {
				case <-stop:
					return
				default:
				}
				ctx, cancel := context.WithTimeout(context.Background(), 20*time.Millisecond)
				if stream, err := c.Subscribe(ctx, &pb.SubscribeRequest{}); err == nil {
					_, _ = stream.Recv()
				}
				_, _ = c.Status(ctx, &pb.StatusRequest{})
				cancel()
			}
		})
	}
	time.Sleep(500 * time.Millisecond)
	close(stop)
	wg.Wait()
	waitSubs(t, s, 0)
}

// TestOutgoingText checks text messages reference the request without copying.
func TestOutgoingText(t *testing.T) {
	req := &pb.SendRequest{Content: &pb.SendRequest_Text{Text: "hola"}}
	msg, err := outgoing(context.Background(), nil, req)
	if err != nil || msg.GetConversation() != "hola" {
		t.Fatalf("outgoing = %v, %v", msg, err)
	}
	if msg.Conversation != &req.GetContent().(*pb.SendRequest_Text).Text {
		t.Fatal("text was copied instead of referenced")
	}
}

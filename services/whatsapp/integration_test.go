//go:build integration

// Integration tests. They always need a real PostgreSQL (TEST_PG_DSN). With
// WA_TEST_PHONE set they also talk to the real WhatsApp network using a single
// phone: the phone is linked twice (devices A and B, see `make pair-test`) and
// messages sent by A to the "Message yourself" chat are received by B, so the
// whole send -> upload -> deliver -> download path is checked end to end.
package main

import (
	"bytes"
	"context"
	"crypto/rand"
	"database/sql"
	"encoding/hex"
	"errors"
	"fmt"
	"image"
	"image/color"
	"image/jpeg"
	"net"
	"os"
	"regexp"
	"strconv"
	"testing"
	"time"

	"kapa/whatsapp/pb"

	"github.com/lib/pq"
	"go.mau.fi/whatsmeow"
	"go.mau.fi/whatsmeow/proto/waAdv"
	"go.mau.fi/whatsmeow/store"
	"go.mau.fi/whatsmeow/types"
	waLog "go.mau.fi/whatsmeow/util/log"
	"google.golang.org/grpc"
	"google.golang.org/grpc/credentials/insecure"
)

// Persistent databases holding the sessions of the two linked test devices.
const (
	liveDBA = "wa_test_a"
	liveDBB = "wa_test_b"
)

// pairingCodeRe is the format of a WhatsApp phone pairing code.
var pairingCodeRe = regexp.MustCompile(`^[A-Z0-9]{4}-[A-Z0-9]{4}$`)

// pgDSN returns the DSN of database db on the test server.
func pgDSN(t *testing.T, db string) string {
	t.Helper()
	base := os.Getenv("TEST_PG_DSN")
	if base == "" {
		t.Fatal("TEST_PG_DSN is required (`make test-integration` sets it)")
	}
	return base + " dbname=" + db
}

// adminExec runs a statement on the maintenance database.
func adminExec(t *testing.T, stmt string) error {
	t.Helper()
	admin, err := sql.Open("postgres", pgDSN(t, "postgres"))
	if err != nil {
		t.Fatal(err)
	}
	defer admin.Close()
	_, err = admin.Exec(stmt)
	return err
}

// ensureDB creates database name unless it exists.
func ensureDB(t *testing.T, name string) {
	t.Helper()
	err := adminExec(t, "CREATE DATABASE "+pq.QuoteIdentifier(name))
	var pqErr *pq.Error
	if err != nil && (!errors.As(err, &pqErr) || pqErr.Code != "42P04") {
		t.Fatalf("create database %s: %v", name, err)
	}
}

// tempDB creates a throwaway database, dropped when the test ends.
func tempDB(t *testing.T) string {
	t.Helper()
	name := "t_" + strconv.FormatInt(time.Now().UnixNano(), 36)
	ensureDB(t, name)
	t.Cleanup(func() {
		if err := adminExec(t, "DROP DATABASE "+pq.QuoteIdentifier(name)+" WITH (FORCE)"); err != nil {
			t.Errorf("drop %s: %v", name, err)
		}
	})
	return name
}

// testLog returns the whatsmeow logger for tests (WA_LOG_LEVEL, default WARN).
func testLog() waLog.Logger { return waLog.Stdout("test", env("WA_LOG_LEVEL", "WARN"), false) }

// startTCP opens the store of db, restores its sessions and serves gRPC on a random port.
func startTCP(t *testing.T, db string, selfChat bool) (*server, pb.WhatsAppServiceClient, string) {
	t.Helper()
	ctx := context.Background()
	s, err := newServer(ctx, pgDSN(t, db), testLog(), selfChat)
	if err != nil {
		t.Fatal(err)
	}
	if err = s.restore(ctx); err != nil {
		t.Fatal(err)
	}
	lis, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	g := s.grpcServer()
	go func() { _ = g.Serve(lis) }()
	t.Cleanup(func() { s.shutdown(); g.Stop() })
	conn, err := grpc.NewClient(lis.Addr().String(), grpc.WithTransportCredentials(insecure.NewCredentials()),
		grpc.WithDefaultCallOptions(grpc.MaxCallRecvMsgSize(maxMsgSize), grpc.MaxCallSendMsgSize(maxMsgSize)))
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = conn.Close() })
	return s, pb.NewWhatsAppServiceClient(conn), lis.Addr().String()
}

// TestStoreMigrationsAndProbes checks the schema is created and the empty gateway serves.
func TestStoreMigrationsAndProbes(t *testing.T) {
	db := tempDB(t)
	_, c, addr := startTCP(t, db, false)
	st, err := c.Status(context.Background(), &pb.StatusRequest{})
	if err != nil || len(st.GetAccounts()) != 0 || st.GetOutdated() {
		t.Fatalf("Status = %v, %v", st, err)
	}
	conn, err := sql.Open("postgres", pgDSN(t, db))
	if err != nil {
		t.Fatal(err)
	}
	defer conn.Close()
	var tables int
	if err = conn.QueryRow(`SELECT count(*) FROM information_schema.tables WHERE table_name LIKE 'whatsmeow\_%'`).Scan(&tables); err != nil || tables == 0 {
		t.Fatalf("whatsmeow tables = %d, %v", tables, err)
	}
	if probeHealth(addr) != 0 {
		t.Fatal("health probe failed")
	}
}

// TestDeleteWipesStoredSession checks Delete removes a stored session even when WhatsApp
// cannot be reached (the client is never connected), and that it stays gone after a restart.
func TestDeleteWipesStoredSession(t *testing.T) {
	ctx := context.Background()
	db := tempDB(t)
	s, err := newServer(ctx, pgDSN(t, db), testLog(), false)
	if err != nil {
		t.Fatal(err)
	}
	dev := s.db.NewDevice()
	jid := types.NewADJID(testPhone, 0, 7)
	dev.ID = &jid
	dev.Account = &waAdv.ADVSignedDeviceIdentity{Details: []byte{1}, AccountSignature: make([]byte, 64), AccountSignatureKey: make([]byte, 32), DeviceSignature: make([]byte, 64)}
	if err = dev.Save(ctx); err != nil {
		t.Fatalf("save device: %v", err)
	}
	if devices, _ := s.db.GetAllDevices(ctx); len(devices) != 1 {
		t.Fatalf("stored devices = %d, want 1", len(devices))
	}
	s.attach(testPhone, whatsmeow.NewClient(dev, waLog.Noop))
	c := startBuf(t, s)
	if _, err = c.Delete(ctx, &pb.DeleteRequest{Phone: testPhone}); err != nil {
		t.Fatalf("Delete: %v", err)
	}
	again, err := newServer(ctx, pgDSN(t, db), testLog(), false)
	if err != nil {
		t.Fatal(err)
	}
	if devices, _ := again.db.GetAllDevices(ctx); len(devices) != 0 {
		t.Fatalf("session survived Delete: %d devices", len(devices))
	}
}

// TestUpstreamProtocol reports when whatsmeow upstream announces a newer WhatsApp protocol
// than the pinned build. It warns instead of failing: upgrading whatsmeow blindly can break
// pairing, so upgrades are deliberate (`make update-whatsmeow WHATSMEOW=<commit>`). A real
// break (WhatsApp rejecting the client) fails the live tests and the health check instead.
func TestUpstreamProtocol(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 15*time.Second)
	defer cancel()
	upstream, err := upstreamVersion(ctx)
	if err != nil {
		t.Skipf("GitHub unreachable, protocol drift not checked: %v", err)
	}
	ours := store.GetWAVersion().String()
	if newer(upstream, ours) {
		t.Logf("UPDATE AVAILABLE: whatsmeow upstream announces protocol %s, this pinned build %s. "+
			"Plan `make update-whatsmeow WHATSMEOW=<commit>` and re-validate pairing.", upstream, ours)
		return
	}
	t.Logf("protocol %s matches whatsmeow upstream", ours)
}

// requirePhone returns WA_TEST_PHONE or skips the live test.
func requirePhone(t *testing.T) string {
	t.Helper()
	raw := os.Getenv("WA_TEST_PHONE")
	if raw == "" {
		t.Skip("WA_TEST_PHONE not set: live WhatsApp tests skipped")
	}
	phone, err := parsePhone(raw)
	if err != nil {
		t.Fatalf("WA_TEST_PHONE: %v", err)
	}
	return phone
}

// TestPairTestDevices links the test phone as devices A and B. Interactive, one-time:
// run it with `make pair-test` (it only runs when WA_PAIR=1).
func TestPairTestDevices(t *testing.T) {
	phone := requirePhone(t)
	if os.Getenv("WA_PAIR") != "1" {
		t.Skip("pairing only runs with `make pair-test`")
	}
	for _, db := range []string{liveDBA, liveDBB} {
		// Stop at the first failure: every pairing attempt counts against WhatsApp's rate limit.
		if !t.Run(db, func(t *testing.T) {
			ensureDB(t, db)
			_, c, _ := startTCP(t, db, false)
			if st, _ := c.Status(context.Background(), &pb.StatusRequest{}); len(st.GetAccounts()) > 0 {
				t.Logf("%s already paired", db)
				return
			}
			pairInteractively(t, c, phone, db)
		}) {
			return
		}
	}
}

// pairInteractively prints a pairing code and waits for the operator to enter it on the phone.
func pairInteractively(t *testing.T, c pb.WhatsAppServiceClient, phone, db string) {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), 3*time.Minute)
	defer cancel()
	stream, err := c.Subscribe(ctx, &pb.SubscribeRequest{Accounts: []string{phone}})
	if err != nil {
		t.Fatal(err)
	}
	resp, err := c.Login(ctx, &pb.LoginRequest{Phone: phone})
	if err != nil {
		t.Fatal(err)
	}
	fmt.Printf("\n  >>> Device %s: on the phone open WhatsApp > Linked devices > Link a device >\n"+
		"      Link with phone number instead, and type:   %s\n"+
		"      (a code lives ~1 minute: a new one is printed here automatically, always use the latest)\n"+
		"      Do not stop this until it prints 'linked': the session is saved only then.\n\n",
		db, resp.GetPairingCode())
	for {
		ev, err := stream.Recv()
		if err != nil {
			t.Fatalf("pairing %s: %v", db, err)
		}
		switch ev.GetAccountEvent().GetState() {
		case pb.AccountState_ACCOUNT_STATE_PAIRING:
			if code := ev.GetAccountEvent().GetPairingCode(); code != "" {
				fmt.Printf("  >>> New code for %s (the previous one expired):   %s\n", db, code)
			} else {
				fmt.Printf("  >>> %s: %s\n", db, ev.GetAccountEvent().GetDetail())
			}
		case pb.AccountState_ACCOUNT_STATE_CONNECTED:
			fmt.Printf("  >>> Device %s linked.\n", db)
			return
		case pb.AccountState_ACCOUNT_STATE_PAIR_FAILED:
			t.Fatalf("pairing %s failed: %s", db, ev.GetAccountEvent().GetDetail())
		}
	}
}

// waitConnected waits until phone is CONNECTED on the gateway of test device db.
func waitConnected(t *testing.T, c pb.WhatsAppServiceClient, phone, db string) {
	t.Helper()
	deadline := time.Now().Add(60 * time.Second)
	last := "no account"
	for time.Now().Before(deadline) {
		st, err := c.Status(context.Background(), &pb.StatusRequest{})
		if err == nil && len(st.GetAccounts()) == 0 {
			t.Fatalf("test device %s not paired: run `make pair-test` (it only pairs the missing devices)", db)
		}
		for _, a := range st.GetAccounts() {
			if a.GetPhone() == phone && a.GetState() == pb.AccountState_ACCOUNT_STATE_CONNECTED {
				return
			}
			last = a.GetState().String()
		}
		time.Sleep(500 * time.Millisecond)
	}
	t.Fatalf("test device %s (%s) not connected after 60s (last state %s)", db, phone, last)
}

// TestLive runs the end-to-end checks against WhatsApp with the two paired test devices.
func TestLive(t *testing.T) {
	phone := requirePhone(t)
	ensureDB(t, liveDBA)
	ensureDB(t, liveDBB)
	_, a, _ := startTCP(t, liveDBA, true)
	sb, b, _ := startTCP(t, liveDBB, true)
	waitConnected(t, a, phone, liveDBA)
	waitConnected(t, b, phone, liveDBB)

	t.Run("status", func(t *testing.T) {
		st, err := a.Status(context.Background(), &pb.StatusRequest{})
		if err != nil || st.GetOutdated() {
			t.Fatalf("Status = %v, %v (outdated means WhatsApp rejected the client)", st, err)
		}
	})
	t.Run("login is idempotent", func(t *testing.T) {
		resp, err := a.Login(context.Background(), &pb.LoginRequest{Phone: phone})
		if err != nil || resp.GetPairingCode() != "" || resp.GetState() != pb.AccountState_ACCOUNT_STATE_CONNECTED {
			t.Fatalf("Login on a linked account = %v, %v", resp, err)
		}
	})
	t.Run("text", func(t *testing.T) {
		text := "kapa test " + nonce(t)
		got := roundTrip(t, sb, a, b, phone, &pb.SendRequest{Content: &pb.SendRequest_Text{Text: text}})
		if got.GetText() != text {
			t.Fatalf("text = %q, want %q", got.GetText(), text)
		}
	})
	t.Run("image", func(t *testing.T) {
		data, caption := testJPEG(t), "kapa image "+nonce(t)
		got := roundTrip(t, sb, a, b, phone, &pb.SendRequest{Content: &pb.SendRequest_Image{Image: &pb.Media{Data: data, MimeType: "image/jpeg", Caption: caption}}})
		img := got.GetImage()
		if !bytes.Equal(img.GetData(), data) || img.GetCaption() != caption || img.GetMimeType() != "image/jpeg" {
			t.Fatalf("image mismatch: %d bytes (want %d), caption %q, mime %q", len(img.GetData()), len(data), img.GetCaption(), img.GetMimeType())
		}
	})
	t.Run("voice note", func(t *testing.T) {
		data := make([]byte, 4096)
		_, _ = rand.Read(data)
		got := roundTrip(t, sb, a, b, phone, &pb.SendRequest{Content: &pb.SendRequest_Audio{Audio: &pb.Media{Data: data, MimeType: "audio/ogg; codecs=opus", VoiceNote: true, Seconds: 1}}})
		audio := got.GetAudio()
		if !bytes.Equal(audio.GetData(), data) || !audio.GetVoiceNote() || audio.GetSeconds() != 1 {
			t.Fatalf("audio mismatch: %d bytes (want %d), voice %v, seconds %d", len(audio.GetData()), len(data), audio.GetVoiceNote(), audio.GetSeconds())
		}
	})
}

// roundTrip sends req from device A to the self chat and returns it as received by device B.
func roundTrip(t *testing.T, sb *server, a, b pb.WhatsAppServiceClient, phone string, req *pb.SendRequest) *pb.Message {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), 90*time.Second)
	stream, err := b.Subscribe(ctx, &pb.SubscribeRequest{Accounts: []string{phone}, Peers: []string{phone}})
	if err != nil {
		cancel()
		t.Fatal(err)
	}
	waitSubs(t, sb, 1)
	defer waitSubs(t, sb, 0) // runs after cancel: the subscription must be gone
	defer cancel()
	req.Account, req.To = phone, phone
	sent, err := a.Send(ctx, req)
	if err != nil {
		t.Fatalf("Send: %v", err)
	}
	for {
		ev, err := stream.Recv()
		if err != nil {
			t.Fatalf("message %s not received by device B: %v", sent.GetId(), err)
		}
		if m := ev.GetMessage(); m.GetId() == sent.GetId() {
			if !m.GetFromMe() || m.GetFrom() != phone {
				t.Fatalf("unexpected envelope: %v", m)
			}
			return m
		}
	}
}

// TestLivePairingHandshake requests a real pairing code (the phone shows a notification)
// and cancels it with Delete: it proves the pairing protocol still works. Opt-in with
// WA_TEST_PAIRING=1: every code request counts against WhatsApp's per-number rate limit.
func TestLivePairingHandshake(t *testing.T) {
	phone := requirePhone(t)
	if os.Getenv("WA_TEST_PAIRING") != "1" {
		t.Skip("pairing handshake is opt-in (WA_TEST_PAIRING=1): code requests are rate-limited by WhatsApp")
	}
	_, c, _ := startTCP(t, tempDB(t), false)
	ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	defer cancel()
	stream, err := c.Subscribe(ctx, &pb.SubscribeRequest{Accounts: []string{phone}})
	if err != nil {
		t.Fatal(err)
	}
	resp, err := c.Login(ctx, &pb.LoginRequest{Phone: phone})
	if err != nil || !pairingCodeRe.MatchString(resp.GetPairingCode()) || resp.GetState() != pb.AccountState_ACCOUNT_STATE_PAIRING {
		t.Fatalf("Login = %v, %v", resp, err)
	}
	if _, err = c.Delete(ctx, &pb.DeleteRequest{Phone: phone}); err != nil {
		t.Fatalf("Delete pending pairing: %v", err)
	}
	for {
		ev, err := stream.Recv()
		if err != nil {
			t.Fatalf("no DELETED event: %v", err)
		}
		if ev.GetAccountEvent().GetState() == pb.AccountState_ACCOUNT_STATE_DELETED {
			break
		}
	}
	if st, _ := c.Status(ctx, &pb.StatusRequest{}); len(st.GetAccounts()) != 0 {
		t.Fatalf("account still listed after Delete: %v", st.GetAccounts())
	}
}

// nonce returns a short random identifier for test messages.
func nonce(t *testing.T) string {
	t.Helper()
	b := make([]byte, 6)
	if _, err := rand.Read(b); err != nil {
		t.Fatal(err)
	}
	return hex.EncodeToString(b)
}

// testJPEG encodes a small random-noise JPEG, unique per call.
func testJPEG(t *testing.T) []byte {
	t.Helper()
	img := image.NewRGBA(image.Rect(0, 0, 64, 64))
	noise := make([]byte, 64*64)
	_, _ = rand.Read(noise)
	for i, v := range noise {
		img.Set(i%64, i/64, color.RGBA{R: v, G: v / 2, B: 255 - v, A: 255})
	}
	var buf bytes.Buffer
	if err := jpeg.Encode(&buf, img, nil); err != nil {
		t.Fatal(err)
	}
	return buf.Bytes()
}

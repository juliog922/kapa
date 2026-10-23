// Package main is the Kapa WhatsApp gateway: a gRPC service that links WhatsApp
// accounts through whatsmeow, keeps their sessions in PostgreSQL and streams their
// one-to-one messages (text, images, audio) to the other services of the platform.
package main

import (
	"context"
	"errors"
	"flag"
	"fmt"
	"io"
	"net"
	"net/http"
	"os"
	"os/signal"
	"regexp"
	"runtime/debug"
	"slices"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"syscall"
	"time"

	"kapa/whatsapp/pb"

	_ "github.com/lib/pq" // PostgreSQL driver for the whatsmeow session store.
	"go.mau.fi/whatsmeow"
	"go.mau.fi/whatsmeow/proto/waE2E"
	"go.mau.fi/whatsmeow/store"
	"go.mau.fi/whatsmeow/store/sqlstore"
	"go.mau.fi/whatsmeow/types"
	"go.mau.fi/whatsmeow/types/events"
	waLog "go.mau.fi/whatsmeow/util/log"
	"google.golang.org/grpc"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/credentials/insecure"
	"google.golang.org/grpc/health"
	healthpb "google.golang.org/grpc/health/grpc_health_v1"
	"google.golang.org/grpc/status"
)

// eventBuffer is the per-subscriber queue length. A subscriber that falls this far
// behind is disconnected instead of stalling the events of every account.
const eventBuffer = 256

// maxMsgSize bounds gRPC messages: WhatsApp media is at most 16 MiB plus framing.
const maxMsgSize = 32 << 20

// upstreamVersionURL is whatsmeow's source of truth for the WhatsApp Web protocol version
// (a variable so tests can point it at a local server).
var upstreamVersionURL = "https://raw.githubusercontent.com/tulir/whatsmeow/main/store/clientpayload.go"

// upstreamVersionRe extracts the protocol version from upstreamVersionURL.
var upstreamVersionRe = regexp.MustCompile(`var waVersion = WAVersionContainer\{(\d+), (\d+), (\d+)\}`)

// server implements WhatsAppService: one whatsmeow client per phone number plus the stream subscribers.
type server struct {
	pb.UnimplementedWhatsAppServiceServer
	db       *sqlstore.Container
	log      waLog.Logger
	health   *health.Server
	selfChat bool
	outdated atomic.Bool

	mu       sync.RWMutex
	accounts map[string]*whatsmeow.Client

	subMu sync.RWMutex
	subs  map[*subscriber]struct{}
}

// subscriber is one Subscribe stream and its filters; an empty filter matches everything.
type subscriber struct {
	accounts map[string]struct{}
	peers    map[string]struct{}
	ch       chan *pb.SubscribeResponse
	gone     chan struct{}
	once     sync.Once
}

// main runs the gateway, or a one-shot probe when -healthcheck or -status is given.
func main() {
	healthcheck := flag.Bool("healthcheck", false, "probe the gRPC health endpoint and exit (0 serving)")
	statusFlag := flag.Bool("status", false, "print accounts and protocol versions and exit (0 ok, 1 outdated, 2 update available)")
	flag.Parse()
	addr := env("GRPC_ADDR", ":50051")
	switch {
	case *healthcheck:
		os.Exit(probeHealth(addr))
	case *statusFlag:
		os.Exit(printStatus(addr, os.Stdout))
	}
	if err := run(addr); err != nil {
		fmt.Fprintln(os.Stderr, "whatsapp:", err)
		os.Exit(1)
	}
}

// env returns the environment variable key, or def when it is unset or empty.
func env(key, def string) string {
	if v := os.Getenv(key); v != "" {
		return v
	}
	return def
}

// run serves gRPC on addr until SIGINT/SIGTERM, then shuts down gracefully.
func run(addr string) error {
	ctx, stop := signal.NotifyContext(context.Background(), syscall.SIGINT, syscall.SIGTERM)
	defer stop()
	store.SetOSInfo("Kapa", [3]uint32{1, 0, 0})
	log := waLog.Stdout("whatsapp", env("WA_LOG_LEVEL", "WARN"), false)
	s, err := newServer(ctx, os.Getenv("DATABASE_URL"), log, os.Getenv("WA_SELF_CHAT") == "1")
	if err != nil {
		return err
	}
	lis, err := net.Listen("tcp", addr)
	if err != nil {
		return err
	}
	g := s.grpcServer()
	go func() {
		<-ctx.Done()
		s.shutdown()
		g.GracefulStop()
	}()
	if err = s.restore(ctx); err != nil {
		return err
	}
	log.Infof("listening on %s", lis.Addr())
	return g.Serve(lis)
}

// newServer opens (and migrates) the session store. An empty dsn uses the PG* environment variables.
func newServer(ctx context.Context, dsn string, log waLog.Logger, selfChat bool) (*server, error) {
	db, err := sqlstore.New(ctx, "postgres", dsn, log.Sub("db"))
	if err != nil {
		return nil, fmt.Errorf("session store: %w", err)
	}
	return &server{
		db: db, log: log, health: health.NewServer(), selfChat: selfChat,
		accounts: map[string]*whatsmeow.Client{}, subs: map[*subscriber]struct{}{},
	}, nil
}

// grpcServer builds the gRPC server exposing WhatsAppService and the standard health service.
func (s *server) grpcServer() *grpc.Server {
	g := grpc.NewServer(grpc.MaxRecvMsgSize(maxMsgSize), grpc.MaxSendMsgSize(maxMsgSize))
	pb.RegisterWhatsAppServiceServer(g, s)
	healthpb.RegisterHealthServer(g, s.health)
	return g
}

// restore reconnects every account whose session is stored in the database.
func (s *server) restore(ctx context.Context) error {
	devices, err := s.db.GetAllDevices(ctx)
	if err != nil {
		return fmt.Errorf("load sessions: %w", err)
	}
	for _, d := range devices {
		cli := s.attach(d.ID.User, whatsmeow.NewClient(d, s.log.Sub(d.ID.User)))
		if err = cli.Connect(); err != nil {
			s.log.Warnf("connect %s: %v", d.ID.User, err)
		}
	}
	return nil
}

// shutdown ends every subscription and disconnects every account.
func (s *server) shutdown() {
	s.health.Shutdown()
	s.subMu.RLock()
	for sub := range s.subs {
		sub.drop()
	}
	s.subMu.RUnlock()
	s.mu.RLock()
	for _, cli := range s.accounts {
		cli.Disconnect()
	}
	s.mu.RUnlock()
}

// attach registers cli as the account for phone and routes its events.
func (s *server) attach(phone string, cli *whatsmeow.Client) *whatsmeow.Client {
	cli.AddEventHandler(s.handler(phone, cli))
	s.mu.Lock()
	s.accounts[phone] = cli
	s.mu.Unlock()
	return cli
}

// claim returns the account for phone, creating an unpaired one when none exists (fresh is true then).
func (s *server) claim(phone string) (cli *whatsmeow.Client, fresh bool) {
	s.mu.Lock()
	defer s.mu.Unlock()
	if cli = s.accounts[phone]; cli != nil {
		return cli, false
	}
	cli = whatsmeow.NewClient(s.db.NewDevice(), s.log.Sub(phone))
	cli.AddEventHandler(s.handler(phone, cli))
	s.accounts[phone] = cli
	return cli, true
}

// forget unregisters cli (only if it is still the account for phone) and disconnects it.
func (s *server) forget(phone string, cli *whatsmeow.Client) {
	s.mu.Lock()
	if s.accounts[phone] == cli {
		delete(s.accounts, phone)
	}
	s.mu.Unlock()
	cli.Disconnect()
}

// lookup parses phone and returns its account, or a gRPC NotFound error.
func (s *server) lookup(raw string) (string, *whatsmeow.Client, error) {
	phone, err := parsePhone(raw)
	if err != nil {
		return "", nil, err
	}
	s.mu.RLock()
	cli := s.accounts[phone]
	s.mu.RUnlock()
	if cli == nil {
		return "", nil, status.Errorf(codes.NotFound, "account %s not found", phone)
	}
	return phone, cli, nil
}

// handler returns the whatsmeow event handler of the account phone.
func (s *server) handler(phone string, cli *whatsmeow.Client) func(any) {
	return func(evt any) {
		switch v := evt.(type) {
		case *events.Message:
			s.onMessage(phone, cli.Store.LID.User, cli, v)
		case *events.Connected:
			s.notify(phone, pb.AccountState_ACCOUNT_STATE_CONNECTED, "")
		case *events.Disconnected:
			s.notify(phone, pb.AccountState_ACCOUNT_STATE_DISCONNECTED, "")
		case *events.StreamReplaced:
			s.notify(phone, pb.AccountState_ACCOUNT_STATE_DISCONNECTED, "session opened by another instance")
		case *events.LoggedOut:
			s.forget(phone, cli)
			s.notify(phone, pb.AccountState_ACCOUNT_STATE_LOGGED_OUT, v.Reason.String())
		case *events.TemporaryBan:
			s.notify(phone, pb.AccountState_ACCOUNT_STATE_BANNED, v.String())
		case *events.ClientOutdated:
			s.setOutdated()
			s.notify(phone, pb.AccountState_ACCOUNT_STATE_OUTDATED, "WhatsApp rejected the client version: update whatsmeow")
		}
	}
}

// setOutdated flags the gateway as rejected by WhatsApp and turns the health check to NOT_SERVING.
func (s *server) setOutdated() {
	s.outdated.Store(true)
	s.health.SetServingStatus("", healthpb.HealthCheckResponse_NOT_SERVING)
	s.log.Errorf("WhatsApp rejected the client as outdated: update whatsmeow and redeploy")
}

// onMessage forwards a one-to-one message of account to its subscribers. Nothing is
// converted or downloaded unless at least one subscriber wants it.
func (s *server) onMessage(account, ownLID string, cli *whatsmeow.Client, m *events.Message) {
	resolve := func(lid types.JID) types.JID {
		pn, _ := cli.Store.LIDs.GetPNForLID(context.Background(), lid)
		return pn
	}
	peer, ok := peerOf(&m.Info, account, ownLID, s.selfChat, resolve)
	if !ok || !s.wanted(account, peer) {
		return
	}
	msg := toMessage(m, peer)
	if msg == nil {
		return
	}
	if err := fetchMedia(cli, m.Message, msg); err != nil {
		s.log.Warnf("download media %s/%s: %v", account, m.Info.ID, err)
		return
	}
	s.publish(account, peer, &pb.SubscribeResponse{Account: account, Event: &pb.SubscribeResponse_Message{Message: msg}})
}

// peerOf returns the phone number of the other party of a one-to-one chat ("lid:<id>" when
// WhatsApp hides it and it cannot be resolved) and false for groups, broadcasts, status
// updates, channels and own messages (except the self chat when selfChat is enabled).
func peerOf(info *types.MessageInfo, own, ownLID string, selfChat bool, resolve func(types.JID) types.JID) (string, bool) {
	if info.IsGroup || !isUserServer(info.Chat.Server) {
		return "", false
	}
	if info.IsFromMe {
		self := info.Chat.User == own || (ownLID != "" && info.Chat.User == ownLID)
		return own, selfChat && self
	}
	return phoneOf(info.Sender, info.SenderAlt, resolve), true
}

// isUserServer reports whether server addresses a single user (by phone number or by LID).
func isUserServer(server string) bool {
	return server == types.DefaultUserServer || server == types.HiddenUserServer
}

// phoneOf maps a sender to its phone number, using its alternate address or the LID store.
func phoneOf(sender, alt types.JID, resolve func(types.JID) types.JID) string {
	switch {
	case sender.Server == types.DefaultUserServer:
		return sender.User
	case alt.Server == types.DefaultUserServer:
		return alt.User
	}
	if pn := resolve(sender.ToNonAD()); pn.Server == types.DefaultUserServer {
		return pn.User
	}
	return "lid:" + sender.User
}

// toMessage converts a text, image or audio message to its API form; nil means unsupported.
// Media bytes are filled in later by fetchMedia.
func toMessage(m *events.Message, peer string) *pb.Message {
	c := m.Message
	out := &pb.Message{Id: m.Info.ID, From: peer, PushName: m.Info.PushName, Timestamp: m.Info.Timestamp.Unix(), FromMe: m.Info.IsFromMe}
	switch {
	case c.GetConversation() != "":
		out.Content = &pb.Message_Text{Text: c.GetConversation()}
	case c.GetExtendedTextMessage().GetText() != "":
		out.Content = &pb.Message_Text{Text: c.GetExtendedTextMessage().GetText()}
	case c.GetImageMessage() != nil:
		img := c.GetImageMessage()
		out.Content = &pb.Message_Image{Image: &pb.Media{MimeType: img.GetMimetype(), Caption: img.GetCaption()}}
	case c.GetAudioMessage() != nil:
		a := c.GetAudioMessage()
		out.Content = &pb.Message_Audio{Audio: &pb.Media{MimeType: a.GetMimetype(), VoiceNote: a.GetPTT(), Seconds: a.GetSeconds()}}
	default:
		return nil
	}
	return out
}

// fetchMedia downloads and decrypts the media of c into out (text needs nothing, not even a
// context). The decrypted buffer is handed to the event as is (no copy).
func fetchMedia(cli *whatsmeow.Client, c *waE2E.Message, out *pb.Message) error {
	var src whatsmeow.DownloadableMessage
	var dst *pb.Media
	switch v := out.GetContent().(type) {
	case *pb.Message_Image:
		src, dst = c.GetImageMessage(), v.Image
	case *pb.Message_Audio:
		src, dst = c.GetAudioMessage(), v.Audio
	default:
		return nil
	}
	ctx, cancel := context.WithTimeout(context.Background(), time.Minute)
	defer cancel()
	data, err := cli.Download(ctx, src)
	dst.Data = data
	return err
}

// notify publishes a lifecycle event of account to its subscribers (peer filters do not apply).
func (s *server) notify(account string, state pb.AccountState, detail string) {
	ev := &pb.AccountEvent{State: state, Detail: detail}
	s.publish(account, "", &pb.SubscribeResponse{Account: account, Event: &pb.SubscribeResponse_AccountEvent{AccountEvent: ev}})
}

// wanted reports whether any subscriber would receive a message of account from peer.
func (s *server) wanted(account, peer string) bool {
	s.subMu.RLock()
	defer s.subMu.RUnlock()
	for sub := range s.subs {
		if sub.matches(account, peer) {
			return true
		}
	}
	return false
}

// publish hands the same event pointer to every matching subscriber without blocking;
// subscribers whose queue is full are dropped.
func (s *server) publish(account, peer string, ev *pb.SubscribeResponse) {
	s.subMu.RLock()
	defer s.subMu.RUnlock()
	for sub := range s.subs {
		if !sub.matches(account, peer) {
			continue
		}
		select {
		case sub.ch <- ev:
		default:
			sub.drop()
		}
	}
}

// matches reports whether the subscriber selects account and peer ("" = account event).
func (sub *subscriber) matches(account, peer string) bool {
	if len(sub.accounts) > 0 {
		if _, ok := sub.accounts[account]; !ok {
			return false
		}
	}
	if peer == "" || len(sub.peers) == 0 {
		return true
	}
	_, ok := sub.peers[peer]
	return ok
}

// drop ends the subscription; safe to call many times and from many goroutines.
func (sub *subscriber) drop() {
	sub.once.Do(func() { close(sub.gone) })
}

// newSubscriber validates the filters of req.
func newSubscriber(req *pb.SubscribeRequest) (*subscriber, error) {
	accounts, err := phoneSet(req.GetAccounts())
	if err != nil {
		return nil, err
	}
	peers, err := phoneSet(req.GetPeers())
	if err != nil {
		return nil, err
	}
	return &subscriber{accounts: accounts, peers: peers, ch: make(chan *pb.SubscribeResponse, eventBuffer), gone: make(chan struct{})}, nil
}

// phoneSet parses a list of phone numbers ("lid:<id>" entries are kept verbatim).
func phoneSet(raw []string) (map[string]struct{}, error) {
	set := make(map[string]struct{}, len(raw))
	for _, r := range raw {
		p := r
		if !strings.HasPrefix(r, "lid:") {
			var err error
			if p, err = parsePhone(r); err != nil {
				return nil, err
			}
		}
		set[p] = struct{}{}
	}
	return set, nil
}

// parsePhone normalizes an international phone number to digits ("+", spaces and dashes are
// dropped) and returns a gRPC InvalidArgument error when it is not a valid E.164 number.
func parsePhone(raw string) (string, error) {
	digits := strings.Map(func(r rune) rune {
		switch {
		case r >= '0' && r <= '9':
			return r
		case r == '+' || r == ' ' || r == '-':
			return -1
		}
		return 'x'
	}, raw)
	if len(digits) < 7 || len(digits) > 15 || digits[0] == '0' || strings.ContainsRune(digits, 'x') {
		return "", status.Errorf(codes.InvalidArgument, "invalid phone number %q: use international format, e.g. +34600111222", raw)
	}
	return digits, nil
}

// Login starts pairing a new account, or reports the state of an existing one.
func (s *server) Login(ctx context.Context, req *pb.LoginRequest) (*pb.LoginResponse, error) {
	phone, err := parsePhone(req.GetPhone())
	if err != nil {
		return nil, err
	}
	cli, fresh := s.claim(phone)
	if !fresh {
		return &pb.LoginResponse{State: stateOf(cli)}, nil
	}
	code, err := s.pair(ctx, phone, cli)
	if err != nil {
		s.forget(phone, cli)
		return nil, status.Errorf(codes.Unavailable, "pairing %s: %v", phone, err)
	}
	return &pb.LoginResponse{PairingCode: code, State: pb.AccountState_ACCOUNT_STATE_PAIRING}, nil
}

// pair connects the unpaired client and requests a phone pairing code.
func (s *server) pair(ctx context.Context, phone string, cli *whatsmeow.Client) (string, error) {
	qr, err := cli.GetQRChannel(context.WithoutCancel(ctx))
	if err != nil {
		return "", err
	}
	if err = cli.Connect(); err != nil {
		return "", err
	}
	ready := make(chan error, 1)
	go s.watchPairing(phone, cli, qr, ready)
	select {
	case err = <-ready:
	case <-ctx.Done():
		err = ctx.Err()
	}
	if err != nil {
		return "", err
	}
	return cli.PairPhone(ctx, phone, true, whatsmeow.PairClientChrome, "Chrome (Linux)")
}

// watchPairing drains the pairing channel: the first code means the socket is ready;
// any failure forgets the account and is published as PAIR_FAILED.
func (s *server) watchPairing(phone string, cli *whatsmeow.Client, qr <-chan whatsmeow.QRChannelItem, ready chan<- error) {
	for item := range qr {
		switch item.Event {
		case whatsmeow.QRChannelEventCode:
			select {
			case ready <- nil:
			default:
			}
		case whatsmeow.QRChannelSuccess.Event:
			return
		default:
			select {
			case ready <- errors.New(item.Event):
			default:
			}
			s.forget(phone, cli)
			s.notify(phone, pb.AccountState_ACCOUNT_STATE_PAIR_FAILED, item.Event)
			return
		}
	}
}

// Logout unlinks a linked account from WhatsApp and wipes its session.
func (s *server) Logout(ctx context.Context, req *pb.LogoutRequest) (*pb.LogoutResponse, error) {
	phone, cli, err := s.lookup(req.GetPhone())
	if err != nil {
		return nil, err
	}
	if cli.Store.ID == nil {
		return nil, status.Error(codes.FailedPrecondition, "account is still pairing: use Delete to cancel it")
	}
	if err = cli.Logout(ctx); err != nil {
		return nil, status.Errorf(codes.Unavailable, "logout: %v", err)
	}
	s.forget(phone, cli)
	s.notify(phone, pb.AccountState_ACCOUNT_STATE_LOGGED_OUT, "logout requested")
	return &pb.LogoutResponse{}, nil
}

// Delete removes an account unconditionally: best-effort unlink, then local wipe.
func (s *server) Delete(ctx context.Context, req *pb.DeleteRequest) (*pb.DeleteResponse, error) {
	phone, cli, err := s.lookup(req.GetPhone())
	if err != nil {
		return nil, err
	}
	if cli.Store.ID != nil && cli.Logout(ctx) != nil {
		cli.Disconnect()
		if err = cli.Store.Delete(ctx); err != nil {
			return nil, status.Errorf(codes.Internal, "wipe session: %v", err)
		}
	}
	s.forget(phone, cli)
	s.notify(phone, pb.AccountState_ACCOUNT_STATE_DELETED, "")
	return &pb.DeleteResponse{}, nil
}

// Send delivers a text, image or audio message from a linked account.
func (s *server) Send(ctx context.Context, req *pb.SendRequest) (*pb.SendResponse, error) {
	_, cli, err := s.lookup(req.GetAccount())
	if err != nil {
		return nil, err
	}
	if cli.Store.ID == nil {
		return nil, status.Error(codes.FailedPrecondition, "account is still pairing")
	}
	to, err := parsePhone(req.GetTo())
	if err != nil {
		return nil, err
	}
	msg, err := outgoing(ctx, cli, req)
	if err != nil {
		return nil, err
	}
	resp, err := cli.SendMessage(ctx, types.NewJID(to, types.DefaultUserServer), msg)
	if err != nil {
		return nil, status.Errorf(codes.Unavailable, "send: %v", err)
	}
	return &pb.SendResponse{Id: resp.ID, Timestamp: resp.Timestamp.Unix()}, nil
}

// outgoing builds the WhatsApp message of req, uploading media first. Request fields are
// referenced, not copied.
func outgoing(ctx context.Context, cli *whatsmeow.Client, req *pb.SendRequest) (*waE2E.Message, error) {
	switch c := req.GetContent().(type) {
	case *pb.SendRequest_Text:
		if c.Text == "" {
			break
		}
		return &waE2E.Message{Conversation: &c.Text}, nil
	case *pb.SendRequest_Image:
		up, err := upload(ctx, cli, c.Image, whatsmeow.MediaImage)
		return &waE2E.Message{ImageMessage: &waE2E.ImageMessage{
			URL: &up.URL, DirectPath: &up.DirectPath, MediaKey: up.MediaKey, FileEncSHA256: up.FileEncSHA256,
			FileSHA256: up.FileSHA256, FileLength: &up.FileLength, Mimetype: &c.Image.MimeType, Caption: &c.Image.Caption,
		}}, err
	case *pb.SendRequest_Audio:
		up, err := upload(ctx, cli, c.Audio, whatsmeow.MediaAudio)
		return &waE2E.Message{AudioMessage: &waE2E.AudioMessage{
			URL: &up.URL, DirectPath: &up.DirectPath, MediaKey: up.MediaKey, FileEncSHA256: up.FileEncSHA256,
			FileSHA256: up.FileSHA256, FileLength: &up.FileLength, Mimetype: &c.Audio.MimeType,
			PTT: &c.Audio.VoiceNote, Seconds: &c.Audio.Seconds,
		}}, err
	}
	return nil, status.Error(codes.InvalidArgument, "content is required: text, image or audio")
}

// upload validates and uploads media to WhatsApp.
func upload(ctx context.Context, cli *whatsmeow.Client, m *pb.Media, kind whatsmeow.MediaType) (whatsmeow.UploadResponse, error) {
	if len(m.GetData()) == 0 || m.GetMimeType() == "" {
		return whatsmeow.UploadResponse{}, status.Error(codes.InvalidArgument, "media data and mime_type are required")
	}
	up, err := cli.Upload(ctx, m.GetData(), kind)
	if err != nil {
		err = status.Errorf(codes.Unavailable, "upload: %v", err)
	}
	return up, err
}

// Status reports the accounts and the protocol versions in use.
func (s *server) Status(context.Context, *pb.StatusRequest) (*pb.StatusResponse, error) {
	out := &pb.StatusResponse{WhatsmeowVersion: whatsmeowVersion(), WaVersion: store.GetWAVersion().String(), Outdated: s.outdated.Load()}
	s.mu.RLock()
	for phone, cli := range s.accounts {
		out.Accounts = append(out.Accounts, &pb.Account{Phone: phone, State: stateOf(cli)})
	}
	s.mu.RUnlock()
	slices.SortFunc(out.Accounts, func(a, b *pb.Account) int { return strings.Compare(a.Phone, b.Phone) })
	return out, nil
}

// Subscribe streams the selected events until the client leaves, falls behind or the server stops.
func (s *server) Subscribe(req *pb.SubscribeRequest, stream pb.WhatsAppService_SubscribeServer) error {
	sub, err := newSubscriber(req)
	if err != nil {
		return err
	}
	s.subMu.Lock()
	s.subs[sub] = struct{}{}
	s.subMu.Unlock()
	defer func() {
		s.subMu.Lock()
		delete(s.subs, sub)
		s.subMu.Unlock()
	}()
	for {
		select {
		case ev := <-sub.ch:
			if err = stream.Send(ev); err != nil {
				return err
			}
		case <-sub.gone:
			return status.Error(codes.ResourceExhausted, "subscription ended (consumer too slow or server stopping): resubscribe")
		case <-stream.Context().Done():
			return nil
		}
	}
}

// stateOf derives the lifecycle state of an account from its client.
func stateOf(cli *whatsmeow.Client) pb.AccountState {
	switch {
	case cli.Store.ID == nil:
		return pb.AccountState_ACCOUNT_STATE_PAIRING
	case cli.IsLoggedIn():
		return pb.AccountState_ACCOUNT_STATE_CONNECTED
	}
	return pb.AccountState_ACCOUNT_STATE_DISCONNECTED
}

// whatsmeowVersion returns the whatsmeow module version compiled into the binary.
func whatsmeowVersion() string {
	info, ok := debug.ReadBuildInfo()
	if !ok {
		return "unknown"
	}
	for _, dep := range info.Deps {
		if dep.Path == "go.mau.fi/whatsmeow" {
			if dep.Replace != nil {
				return dep.Replace.Version
			}
			return dep.Version
		}
	}
	return "unknown"
}

// localAddr turns a listen address such as ":50051" into a dialable one.
func localAddr(addr string) string {
	if strings.HasPrefix(addr, ":") {
		return "localhost" + addr
	}
	return addr
}

// dial opens a client connection to the local gateway.
func dial(addr string) (*grpc.ClientConn, error) {
	return grpc.NewClient(localAddr(addr), grpc.WithTransportCredentials(insecure.NewCredentials()))
}

// probeHealth exits 0 when the local gateway reports SERVING (used by the container healthcheck).
func probeHealth(addr string) int {
	conn, err := dial(addr)
	if err != nil {
		return 1
	}
	defer conn.Close()
	ctx, cancel := context.WithTimeout(context.Background(), 3*time.Second)
	defer cancel()
	resp, err := healthpb.NewHealthClient(conn).Check(ctx, &healthpb.HealthCheckRequest{})
	if err != nil || resp.GetStatus() != healthpb.HealthCheckResponse_SERVING {
		return 1
	}
	return 0
}

// printStatus prints the gateway status for operators (make monitor) and returns
// 0 when up to date, 1 when WhatsApp rejected the client and 2 when upstream moved on.
func printStatus(addr string, w io.Writer) int {
	st, err := fetchStatus(addr)
	if err != nil {
		_, _ = io.WriteString(w, "status: "+err.Error()+"\n")
		return 1
	}
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	upstream, uerr := upstreamVersion(ctx)
	report, code := statusReport(st, upstream, uerr)
	if _, err = io.WriteString(w, report); err != nil {
		return 1
	}
	return code
}

// fetchStatus calls Status on the local gateway.
func fetchStatus(addr string) (*pb.StatusResponse, error) {
	conn, err := dial(addr)
	if err != nil {
		return nil, err
	}
	defer conn.Close()
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	return pb.NewWhatsAppServiceClient(conn).Status(ctx, &pb.StatusRequest{})
}

// statusReport renders the status and the protocol diagnosis, with the printStatus exit code.
func statusReport(st *pb.StatusResponse, upstream string, upstreamErr error) (string, int) {
	var b strings.Builder
	fmt.Fprintf(&b, "whatsmeow %s · protocol %s · %d account(s)\n", st.GetWhatsmeowVersion(), st.GetWaVersion(), len(st.GetAccounts()))
	for _, a := range st.GetAccounts() {
		b.WriteString("  +" + a.GetPhone() + " " + strings.TrimPrefix(a.GetState().String(), "ACCOUNT_STATE_") + "\n")
	}
	code := 0
	switch {
	case st.GetOutdated():
		b.WriteString("OUTDATED: WhatsApp rejected this client. Run `make update-whatsmeow`, open a PR and deploy.\n")
		code = 1
	case upstreamErr != nil:
		b.WriteString("upstream protocol check skipped: " + upstreamErr.Error() + "\n")
	case newer(upstream, st.GetWaVersion()):
		b.WriteString("UPDATE AVAILABLE: whatsmeow upstream speaks protocol " + upstream + " (deployed " + st.GetWaVersion() + "). Run `make update-whatsmeow`.\n")
		code = 2
	default:
		b.WriteString("protocol up to date with whatsmeow upstream (" + upstream + ")\n")
	}
	return b.String(), code
}

// upstreamVersion fetches the protocol version of whatsmeow's main branch.
func upstreamVersion(ctx context.Context) (string, error) {
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, upstreamVersionURL, http.NoBody)
	if err != nil {
		return "", err
	}
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		return "", err
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return "", fmt.Errorf("upstream returned %s", resp.Status)
	}
	src, err := io.ReadAll(io.LimitReader(resp.Body, 1<<20))
	if err != nil {
		return "", err
	}
	return parseWAVersion(src)
}

// parseWAVersion extracts "major.minor.patch" from whatsmeow's clientpayload.go source.
func parseWAVersion(src []byte) (string, error) {
	m := upstreamVersionRe.FindSubmatch(src)
	if m == nil {
		return "", errors.New("protocol version not found upstream")
	}
	return string(m[1]) + "." + string(m[2]) + "." + string(m[3]), nil
}

// newer reports whether version a is strictly greater than version b ("x.y.z").
func newer(a, b string) bool {
	return slices.Compare(versionParts(a), versionParts(b)) > 0
}

// versionParts splits "x.y.z" into numbers; non-numeric parts count as 0.
func versionParts(v string) []int {
	fields := strings.Split(v, ".")
	parts := make([]int, len(fields))
	for i, f := range fields {
		parts[i], _ = strconv.Atoi(f)
	}
	return parts
}

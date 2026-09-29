//! The loopback bridge (BRG-03 to BRG-06): a WebSocket server on the first
//! free port of 127.0.0.1:47815–47819.
//!
//! Each connection must pass two checks before any frame is read: `Host` is
//! exactly `127.0.0.1:<port>` and `Origin` exactly equals the origin of a
//! live pending pairing. Then comes the mutual handshake (`hello`,
//! `challenge`, `prove`, `welcome`) within 10 s; nothing about the survey is
//! sent before it completes. Every later frame is sealed ([`super::channel`])
//! and every message is checked against the protocol schemas
//! ([`super::protocol`]). Any failure closes the connection.
//!
//! The desktop never serves HTTP content, files or CORS responses: a request
//! that is not an acceptable WebSocket upgrade gets `403` and nothing else.

use std::collections::{HashMap, HashSet};
use std::io::ErrorKind;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use serde_json::Value;
use tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tungstenite::protocol::WebSocketConfig;
use tungstenite::{Message, WebSocket};

use super::channel::{self, FrameKind, Opener, Sealer};
use super::pairing::{Intent, LaunchRequest, PendingPairings};
use super::protocol::{self, Inbound};
use super::redact::Redacted;
use super::transfer::{self, Completed, Incoming};

/// The fixed bridge port range (BRG-03); the CSP allows exactly these.
pub const PORTS: [u16; 5] = [47815, 47816, 47817, 47818, 47819];
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
pub const PING_INTERVAL: Duration = Duration::from_secs(30);
/// How often a connection thread wakes to send queued frames and pings.
const POLL: Duration = Duration::from_millis(50);
/// Messages held for a re-pair that is waiting for the user's confirmation.
const MAX_HELD_EVENTS: usize = 64;
/// The web version the desktop reports it supports.
const SUPPORTED_PROTOCOL: u64 = 1;

pub type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

/// Listener settings; tests shorten the timings and use ephemeral ports.
#[derive(Clone)]
pub struct BridgeConfig {
    pub ports: Vec<u16>,
    pub handshake_timeout: Duration,
    pub ping_interval: Duration,
    pub clock: Clock,
}

impl Default for BridgeConfig {
    fn default() -> Self {
        Self {
            ports: PORTS.to_vec(),
            handshake_timeout: HANDSHAKE_TIMEOUT,
            ping_interval: PING_INTERVAL,
            clock: Arc::new(Instant::now),
        }
    }
}

pub type SessionId = u64;

/// What the bridge reports to the application.
#[derive(Debug, Clone)]
pub enum BridgeEvent {
    /// A handshake completed. `needs_confirmation` is set when the survey's
    /// drawing is already open: the user must confirm the re-pair
    /// ([`Bridge::confirm`]) before its messages are delivered.
    Opened {
        session: SessionId,
        origin: String,
        survey: Redacted<String>,
        intent: Intent,
        needs_confirmation: bool,
    },
    Message { session: SessionId, message: Inbound },
    Transfer { session: SessionId, transfer: Arc<Completed> },
    Closed { session: SessionId, reason: String },
}

/// What the application asks a session to send.
enum Outbound {
    Message(Value),
    /// `sent` counts the bytes handed to the socket, for Apply's progress.
    Transfer { id: u32, name: String, media_type: String, bytes: Arc<Vec<u8>>, sent: Option<Arc<AtomicU64>> },
    Close(&'static str),
    Confirmed,
}

struct SessionEntry {
    origin: String,
    survey: String,
    confirmed: bool,
    /// The next desktop-to-web transfer id; ids are assigned when a transfer
    /// is queued so the message that references it can name it.
    next_transfer: u32,
    outbound: mpsc::Sender<Outbound>,
}

struct Shared {
    config: BridgeConfig,
    port: u16,
    /// Origins the user trusts (and may pair from), or `None` before the
    /// application first says; every pairing and session is checked against it.
    trusted: Mutex<Option<HashSet<String>>>,
    pending: Mutex<PendingPairings>,
    sessions: Mutex<HashMap<SessionId, SessionEntry>>,
    open_documents: Mutex<HashSet<(String, String)>>,
    events: Mutex<mpsc::Sender<BridgeEvent>>,
    next_session: AtomicU64,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

impl Shared {
    fn now(&self) -> Instant {
        (self.config.clock)()
    }

    fn origin_allowed(&self, origin: &str) -> bool {
        lock(&self.trusted).as_ref().is_none_or(|trusted| trusted.contains(origin))
    }

    fn emit(&self, event: BridgeEvent) {
        let _ = lock(&self.events).send(event);
    }

    /// Close every other session for the same origin and survey.
    fn supersede(&self, keep: SessionId) {
        let sessions = lock(&self.sessions);
        let Some(kept) = sessions.get(&keep) else { return };
        for (id, entry) in sessions.iter() {
            if *id != keep && entry.origin == kept.origin && entry.survey == kept.survey {
                let _ = entry.outbound.send(Outbound::Close("superseded"));
            }
        }
    }
}

/// The running listener.
pub struct Bridge {
    shared: Arc<Shared>,
    events: Mutex<Option<mpsc::Receiver<BridgeEvent>>>,
}

impl Bridge {
    /// Listen on the first free configured port of 127.0.0.1.
    pub fn bind(config: BridgeConfig) -> std::io::Result<Self> {
        let mut last_error = std::io::Error::new(ErrorKind::AddrInUse, "no bridge port is free");
        for port in &config.ports {
            match TcpListener::bind(("127.0.0.1", *port)) {
                Ok(listener) => return Ok(Self::serve(listener, config)),
                Err(error) => last_error = error,
            }
        }
        Err(last_error)
    }

    fn serve(listener: TcpListener, config: BridgeConfig) -> Self {
        let port = listener.local_addr().map(|a| a.port()).unwrap_or_default();
        let (sender, receiver) = mpsc::channel();
        let shared = Arc::new(Shared {
            config,
            port,
            trusted: Mutex::default(),
            pending: Mutex::default(),
            sessions: Mutex::default(),
            open_documents: Mutex::default(),
            events: Mutex::new(sender),
            next_session: AtomicU64::new(1),
        });
        let accept = Arc::clone(&shared);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let shared = Arc::clone(&accept);
                std::thread::spawn(move || serve_connection(shared, stream));
            }
        });
        Self { shared, events: Mutex::new(Some(receiver)) }
    }

    pub fn port(&self) -> u16 {
        self.shared.port
    }

    /// Add a pending pairing for a trusted origin (silently).
    pub fn add_pending(&self, request: LaunchRequest) {
        self.add_pending_received(request, self.shared.now());
    }

    /// Add a pending pairing whose launch arrived at `received`; it expires
    /// 120 s after that, not after this call.
    pub fn add_pending_received(&self, request: LaunchRequest, received: Instant) {
        if self.shared.origin_allowed(&request.origin) {
            lock(&self.shared.pending).insert_received(request, received, self.shared.now());
        }
    }

    /// Set the origins that may pair. Pending pairings and handshakes from
    /// any other origin are dropped at once, and its open sessions closed:
    /// revoking trust takes effect immediately (BRG-02).
    pub fn set_trusted_origins(&self, origins: HashSet<String>) {
        *lock(&self.shared.trusted) = Some(origins);
        lock(&self.shared.pending).retain_origins(|origin| self.shared.origin_allowed(origin));
        for entry in lock(&self.shared.sessions).values() {
            if !self.shared.origin_allowed(&entry.origin) {
                let _ = entry.outbound.send(Outbound::Close("userCancelled"));
            }
        }
    }

    /// The event stream; available once.
    pub fn take_events(&self) -> Option<mpsc::Receiver<BridgeEvent>> {
        lock(&self.events).take()
    }

    fn outbound(&self, session: SessionId, message: Outbound) -> bool {
        lock(&self.shared.sessions)
            .get(&session)
            .is_some_and(|entry| entry.outbound.send(message).is_ok())
    }

    /// Send a desktop-to-web control message (checked before sealing).
    pub fn send(&self, session: SessionId, message: Value) -> bool {
        self.outbound(session, Outbound::Message(message))
    }

    /// Send bytes as an announced transfer. Returns its transfer id, which a
    /// later message can reference: transfers and messages are sent in the
    /// order they are queued, so the transfer always precedes that message.
    pub fn send_transfer(&self, session: SessionId, name: &str, media_type: &str, bytes: Arc<Vec<u8>>) -> Option<u32> {
        self.send_transfer_counted(session, name, media_type, bytes, None)
    }

    /// [`Self::send_transfer`], adding each chunk's payload bytes to `sent`
    /// once it is handed to the socket.
    pub fn send_transfer_counted(
        &self,
        session: SessionId,
        name: &str,
        media_type: &str,
        bytes: Arc<Vec<u8>>,
        sent: Option<Arc<AtomicU64>>,
    ) -> Option<u32> {
        let mut sessions = lock(&self.shared.sessions);
        let entry = sessions.get_mut(&session)?;
        let id = entry.next_transfer;
        let transfer = Outbound::Transfer { id, name: name.to_string(), media_type: media_type.to_string(), bytes, sent };
        entry.outbound.send(transfer).ok()?;
        entry.next_transfer = id.checked_add(1)?;
        Some(id)
    }

    /// End a session with `close{reason}`.
    pub fn close(&self, session: SessionId, reason: &'static str) -> bool {
        self.outbound(session, Outbound::Close(reason))
    }

    /// Answer a re-pair confirmation: accepting supersedes the older session
    /// and delivers the held messages; declining closes the new session.
    pub fn confirm(&self, session: SessionId, accept: bool) -> bool {
        if !accept {
            return self.close(session, "userCancelled");
        }
        if let Some(entry) = lock(&self.shared.sessions).get_mut(&session) {
            entry.confirmed = true;
        }
        self.shared.supersede(session);
        self.outbound(session, Outbound::Confirmed)
    }

    /// Record whether the drawing for `(origin, survey)` is open, so a new
    /// pairing for it asks the user first (BRG-05).
    pub fn set_document_open(&self, origin: &str, survey: &str, open: bool) {
        let key = (origin.to_string(), survey.to_string());
        let mut documents = lock(&self.shared.open_documents);
        if open {
            documents.insert(key);
        } else {
            documents.remove(&key);
        }
    }
}

impl std::fmt::Debug for Bridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bridge").field("port", &self.port()).finish()
    }
}

fn global_cell() -> &'static Option<Arc<Bridge>> {
    static BRIDGE: OnceLock<Option<Arc<Bridge>>> = OnceLock::new();
    BRIDGE.get_or_init(|| Bridge::bind(BridgeConfig::default()).ok().map(Arc::new))
}

/// The application's bridge, listening on the fixed port range. `None` when
/// every bridge port is taken.
pub fn global() -> Option<&'static Bridge> {
    global_cell().as_deref()
}

pub fn global_arc() -> Option<Arc<Bridge>> {
    global_cell().clone()
}

fn forbidden() -> ErrorResponse {
    let mut response = ErrorResponse::new(None);
    *response.status_mut() = tungstenite::http::StatusCode::FORBIDDEN;
    response
}

fn ws_config() -> WebSocketConfig {
    // A sealed control frame is base64 of ≤ 1 MiB + tag; a chunk is
    // ≤ 1 MiB + 8-byte header + tag.
    let limit = 2 * 1024 * 1024;
    WebSocketConfig::default().max_message_size(Some(limit)).max_frame_size(Some(limit))
}

fn is_timeout(error: &tungstenite::Error) -> bool {
    matches!(error, tungstenite::Error::Io(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut))
}

type Socket = WebSocket<TcpStream>;

/// Read the next plaintext text frame before `deadline`.
fn read_handshake_text(socket: &mut Socket, shared: &Shared, deadline: Instant) -> Option<String> {
    loop {
        if shared.now() >= deadline {
            return None;
        }
        match socket.read() {
            Ok(Message::Text(text)) => return Some(text.to_string()),
            Ok(Message::Ping(_) | Message::Pong(_)) => continue,
            Ok(_) => return None, // binary or close before the handshake completes
            Err(error) if is_timeout(&error) => continue,
            Err(_) => return None,
        }
    }
}

fn close_socket(socket: &mut Socket) {
    let _ = socket.close(None);
    let _ = socket.flush();
}

fn next_request_id(counter: &mut u64) -> String {
    *counter += 1;
    format!("d{counter}")
}

// The upgrade check's error type is fixed by tungstenite's `Callback`.
#[allow(clippy::result_large_err)]
fn serve_connection(shared: Arc<Shared>, stream: TcpStream) {
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(shared.config.handshake_timeout));
    let _ = stream.set_write_timeout(Some(shared.config.handshake_timeout));
    let port = shared.port;
    let mut upgrade_origin: Option<String> = None;
    let check = |request: &Request, response: Response| -> Result<Response, ErrorResponse> {
        let single = |name: &str| {
            let mut values = request.headers().get_all(name).iter();
            match (values.next(), values.next()) {
                (Some(value), None) => value.to_str().ok().map(str::to_string),
                _ => None,
            }
        };
        let host_ok = single("host").is_some_and(|host| host == format!("127.0.0.1:{port}"));
        let origin = single("origin")
            .filter(|origin| shared.origin_allowed(origin) && lock(&shared.pending).has_origin(origin, shared.now()));
        match (host_ok, origin) {
            (true, Some(origin)) => {
                upgrade_origin = Some(origin);
                Ok(response)
            }
            _ => Err(forbidden()),
        }
    };
    let deadline = shared.now() + shared.config.handshake_timeout;
    let mut socket = match tungstenite::accept_hdr_with_config(stream, check, Some(ws_config())) {
        Ok(socket) => socket,
        Err(_) => return,
    };
    let Some(origin) = upgrade_origin else { return };
    // From here the thread wakes regularly to send queued frames and pings.
    let _ = socket.get_mut().set_read_timeout(Some(POLL));

    // BRG-04 handshake. Only `challenge` is sent before both proofs pass.
    let Some(session) = handshake(&shared, &mut socket, &origin, deadline) else {
        close_socket(&mut socket);
        return;
    };
    run_session(shared, socket, session);
}

struct Established {
    launch: LaunchRequest,
    sealer: Sealer,
    opener: Opener,
    request_ids: u64,
}

fn handshake(shared: &Shared, socket: &mut Socket, origin: &str, deadline: Instant) -> Option<Established> {
    let hello = protocol::parse_hello(&read_handshake_text(socket, shared, deadline)?).ok()?;
    if !hello.protocols.contains(&SUPPORTED_PROTOCOL) {
        return None;
    }
    let launch = lock(&shared.pending).find(&hello.pairing, origin, shared.now())?;
    let token = *launch.token.expose();
    let mut nonce_d = [0u8; 32];
    getrandom::fill(&mut nonce_d).ok()?;
    let proof_d = channel::desktop_proof(&token, &hello.nonce_w, &nonce_d);
    let mut request_ids = 0;
    let challenge = protocol::encode_outbound(&protocol::challenge(&next_request_id(&mut request_ids), &nonce_d, &proof_d)).ok()?;
    socket.send(Message::text(challenge)).ok()?;

    let proof_w = protocol::parse_prove(&read_handshake_text(socket, shared, deadline)?).ok()?;
    if !channel::verify_web_proof(&token, &hello.nonce_w, &nonce_d, &proof_w) {
        return None;
    }
    // Consume the pairing exactly once, even if two connections raced, and
    // only while its origin is still trusted (trust can be revoked mid-handshake).
    {
        let mut pending = lock(&shared.pending);
        pending.find(&hello.pairing, origin, shared.now())?;
        pending.consume(&hello.pairing);
        if !shared.origin_allowed(origin) {
            return None;
        }
    }
    let keys = channel::derive_keys(&token, &hello.nonce_w, &nonce_d);
    let mut established = Established {
        launch,
        sealer: Sealer::new(&keys.desktop_to_web),
        opener: Opener::new(&keys.web_to_desktop),
        request_ids,
    };
    let welcome = protocol::welcome(&next_request_id(&mut established.request_ids));
    send_control(socket, &mut established.sealer, &welcome).ok()?;
    Some(established)
}

fn send_control(socket: &mut Socket, sealer: &mut Sealer, message: &Value) -> Result<(), ()> {
    let text = protocol::encode_outbound(message).map_err(|_| ())?;
    let sealed = sealer.seal(FrameKind::Control, text.as_bytes()).map_err(|_| ())?;
    socket.send(Message::text(channel::b64url_encode(&sealed))).map_err(|_| ())
}

/// Why a session ended; the web gets `close{reason}` when it can.
enum End {
    /// The web closed or the socket dropped.
    Remote(String),
    /// The desktop ends it with this `close` reason.
    Local(&'static str),
}

fn run_session(shared: Arc<Shared>, mut socket: Socket, mut established: Established) {
    let session = shared.next_session.fetch_add(1, Ordering::SeqCst);
    let (outbound_sender, outbound) = mpsc::channel();
    let origin = established.launch.origin.clone();
    let trusted_origin = origin.clone();
    let survey = established.launch.survey.expose().clone();
    let needs_confirmation = lock(&shared.open_documents).contains(&(origin.clone(), survey.clone()));
    lock(&shared.sessions).insert(
        session,
        SessionEntry { origin: origin.clone(), survey, confirmed: !needs_confirmation, next_transfer: 1, outbound: outbound_sender.clone() },
    );
    // Trust revoked between the handshake and registration: end it at once.
    if !shared.origin_allowed(&origin) {
        let _ = outbound_sender.send(Outbound::Close("userCancelled"));
    }
    if !needs_confirmation {
        shared.supersede(session);
    }
    shared.emit(BridgeEvent::Opened {
        session,
        origin,
        survey: established.launch.survey.clone(),
        intent: established.launch.intent,
        needs_confirmation,
    });

    let mut confirmed = !needs_confirmation;
    let mut held: Vec<BridgeEvent> = Vec::new();
    let mut incoming = Incoming::default();
    let mut last_ping = shared.now();
    let mut queued: std::collections::VecDeque<Outbound> = std::collections::VecDeque::new();
    let end = loop {
        // A close ends the session at once, ahead of anything still queued,
        // whatever the transfers are doing (revocation, supersede, explicit).
        let mut local_end = None;
        queued.extend(outbound.try_iter());
        if let Some(reason) = queued.iter().find_map(|item| if let Outbound::Close(reason) = item { Some(*reason) } else { None }) {
            break End::Local(reason);
        }
        // Then the application's frames, in order. Each transfer is sent
        // whole before the next, which keeps the desktop's share of in-flight
        // transfers (BRG-06) without waiting on incoming ones, and no message
        // ever precedes the transfer it references.
        while let Some(item) = queued.pop_front() {
            let result = match item {
                Outbound::Message(message) => send_control(&mut socket, &mut established.sealer, &message),
                Outbound::Transfer { id, name, media_type, bytes, sent } => {
                    let start = protocol::transfer_start(&next_request_id(&mut established.request_ids), id, &name, &media_type, &bytes);
                    send_control(&mut socket, &mut established.sealer, &start).and_then(|()| {
                        transfer::chunks(id, &bytes).zip(bytes.chunks(transfer::MAX_CHUNK_PAYLOAD)).try_for_each(|(chunk, payload)| {
                            let sealed = established.sealer.seal(FrameKind::Chunk, &chunk).map_err(|_| ())?;
                            socket.send(Message::binary(sealed)).map_err(|_| ())?;
                            if let Some(sent) = &sent {
                                sent.fetch_add(payload.len() as u64, Ordering::Relaxed);
                            }
                            Ok(())
                        })
                    })
                }
                Outbound::Close(reason) => {
                    local_end = Some(End::Local(reason));
                    Ok(())
                }
                Outbound::Confirmed if !shared.origin_allowed(&trusted_origin) => {
                    local_end = Some(End::Local("userCancelled"));
                    Ok(())
                }
                Outbound::Confirmed => {
                    confirmed = true;
                    held.drain(..).for_each(|event| shared.emit(event));
                    Ok(())
                }
            };
            if result.is_err() {
                local_end = Some(End::Remote("send failed".into()));
            }
            if local_end.is_some() {
                break;
            }
        }
        if let Some(end) = local_end {
            break end;
        }
        let now = shared.now();
        if now.saturating_duration_since(last_ping) >= shared.config.ping_interval {
            last_ping = now;
            let ping = protocol::ping(&next_request_id(&mut established.request_ids));
            if send_control(&mut socket, &mut established.sealer, &ping).is_err() {
                break End::Remote("send failed".into());
            }
        }
        if incoming.check_progress(now).is_err() {
            break End::Local("transferFailed");
        }
        let event = match socket.read() {
            Ok(Message::Text(text)) => {
                let Some(sealed) = channel::b64url_decode(text.as_str()) else { break End::Local("protocolError") };
                let Ok(plaintext) = established.opener.open(FrameKind::Control, &sealed) else {
                    break End::Local("protocolError");
                };
                let Ok(text) = String::from_utf8(plaintext) else { break End::Local("protocolError") };
                match protocol::parse_inbound(&text) {
                    Ok(Inbound::Close { reason }) => break End::Remote(reason),
                    Ok(Inbound::TransferStart(start)) => match incoming.start(start, now) {
                        Ok(done) => done.map(|transfer| BridgeEvent::Transfer { session, transfer: Arc::new(transfer) }),
                        Err(_) => break End::Local("transferFailed"),
                    },
                    Ok(message) => Some(BridgeEvent::Message { session, message }),
                    Err(_) => break End::Local("protocolError"),
                }
            }
            Ok(Message::Binary(sealed)) => {
                let Ok(plaintext) = established.opener.open(FrameKind::Chunk, &sealed) else {
                    break End::Local("protocolError");
                };
                match incoming.chunk(&plaintext, now) {
                    Ok(done) => done.map(|transfer| BridgeEvent::Transfer { session, transfer: Arc::new(transfer) }),
                    Err(_) => break End::Local("transferFailed"),
                }
            }
            Ok(Message::Close(_)) => break End::Remote("socket closed".into()),
            Ok(_) => None,
            Err(error) if is_timeout(&error) => None,
            Err(_) => break End::Remote("socket closed".into()),
        };
        if let Some(event) = event {
            // Trust may have been revoked while this frame was read: nothing
            // more from the origin reaches the application.
            if !shared.origin_allowed(&trusted_origin) {
                break End::Local("userCancelled");
            }
            if confirmed {
                shared.emit(event);
            } else if held.len() < MAX_HELD_EVENTS {
                held.push(event);
            } else {
                break End::Local("protocolError");
            }
        }
    };

    incoming.dispose();
    lock(&shared.sessions).remove(&session);
    let reason = match end {
        End::Local(reason) => {
            let close = protocol::close(&next_request_id(&mut established.request_ids), reason);
            let _ = send_control(&mut socket, &mut established.sealer, &close);
            reason.to_string()
        }
        End::Remote(reason) => reason,
    };
    close_socket(&mut socket);
    shared.emit(BridgeEvent::Closed { session, reason });
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::app::secureplan::channel::{b64url_decode, b64url_encode, DirectionalKeys};
    use crate::app::secureplan::pairing::tests::launch;
    use serde_json::json;
    use std::io::{Read, Write};
    use tungstenite::client::IntoClientRequest;

    pub(crate) const ORIGIN: &str = "https://secureplan.example";

    pub(crate) fn test_bridge(ping_interval: Duration) -> Bridge {
        Bridge::bind(BridgeConfig {
            ports: vec![0],
            handshake_timeout: Duration::from_secs(5),
            ping_interval,
            clock: Arc::new(Instant::now),
        })
        .expect("bind an ephemeral port")
    }

    fn request(port: u16, host: Option<&str>, origin: Option<&str>) -> tungstenite::handshake::client::Request {
        let mut request = format!("ws://127.0.0.1:{port}/").into_client_request().unwrap();
        let headers = request.headers_mut();
        match host {
            Some(host) => headers.insert("host", host.parse().unwrap()),
            None => headers.remove("host"),
        };
        if let Some(origin) = origin {
            headers.insert("origin", origin.parse().unwrap());
        }
        request
    }

    pub(crate) struct Web {
        pub socket: WebSocket<TcpStream>,
        pub sealer: Sealer,
        pub opener: Opener,
    }

    /// Connect and complete the handshake as the web does.
    pub(crate) fn connect_web(port: u16, request: &LaunchRequest) -> Result<Web, String> {
        let (mut socket, _) = upgrade(port, Some(&format!("127.0.0.1:{port}")), Some(&request.origin))?;
        let token = request.token.expose();
        let nonce_w = [0x42u8; 32];
        let hello = json!({ "type": "hello", "requestId": "h1", "pairing": b64url_encode(request.pairing.expose()), "nonceW": b64url_encode(&nonce_w), "webVersion": "0.1.0", "minDesktopVersion": "0.1.0", "protocols": [1] });
        socket.send(Message::text(hello.to_string())).map_err(|e| e.to_string())?;
        let challenge = read_text(&mut socket).ok_or("no challenge")?;
        let challenge: Value = serde_json::from_str(&challenge).unwrap();
        assert!(protocol::validate_message(protocol::Direction::DesktopToWeb, &challenge).is_ok());
        let nonce_d: [u8; 32] = channel::b64url_array(challenge["nonceD"].as_str().unwrap()).unwrap();
        let proof_d = b64url_decode(challenge["proofD"].as_str().unwrap()).unwrap();
        if !channel::verify_desktop_proof(token, &nonce_w, &nonce_d, &proof_d) {
            return Err("bad desktop proof".into());
        }
        let proof_w = channel::web_proof(token, &nonce_w, &nonce_d);
        let prove = json!({ "type": "prove", "requestId": "p1", "proofW": b64url_encode(&proof_w) });
        socket.send(Message::text(prove.to_string())).map_err(|e| e.to_string())?;
        let DirectionalKeys { web_to_desktop, desktop_to_web } = channel::derive_keys(token, &nonce_w, &nonce_d);
        let mut web = Web { socket, sealer: Sealer::new(&web_to_desktop), opener: Opener::new(&desktop_to_web) };
        let welcome = web.receive().ok_or("no welcome")?;
        assert_eq!(welcome["type"], "welcome");
        assert!(protocol::validate_message(protocol::Direction::DesktopToWeb, &welcome).is_ok());
        Ok(web)
    }

    fn upgrade(port: u16, host: Option<&str>, origin: Option<&str>) -> Result<(WebSocket<TcpStream>, ()), String> {
        let stream = TcpStream::connect(("127.0.0.1", port)).map_err(|e| e.to_string())?;
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let (socket, _) = tungstenite::client(request(port, host, origin), stream).map_err(|e| e.to_string())?;
        Ok((socket, ()))
    }

    fn read_text(socket: &mut WebSocket<TcpStream>) -> Option<String> {
        loop {
            match socket.read() {
                Ok(Message::Text(text)) => return Some(text.to_string()),
                Ok(Message::Ping(_) | Message::Pong(_)) => continue,
                _ => return None,
            }
        }
    }

    impl Web {
        pub(crate) fn send(&mut self, message: &Value) {
            let sealed = self.sealer.seal(FrameKind::Control, message.to_string().as_bytes()).unwrap();
            self.socket.send(Message::text(b64url_encode(&sealed))).unwrap();
        }

        pub(crate) fn send_raw(&mut self, message: Message) {
            self.socket.send(message).unwrap();
        }

        /// The next sealed control message, or `None` once the socket closes.
        pub(crate) fn receive(&mut self) -> Option<Value> {
            loop {
                match self.socket.read() {
                    Ok(Message::Text(text)) => {
                        let sealed = b64url_decode(text.as_str())?;
                        let plaintext = self.opener.open(FrameKind::Control, &sealed).ok()?;
                        return serde_json::from_slice(&plaintext).ok();
                    }
                    // Chunks are opened (and dropped) so the frame counter stays in step.
                    Ok(Message::Binary(sealed)) => {
                        self.opener.open(FrameKind::Chunk, &sealed).ok()?;
                    }
                    Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => continue,
                    _ => return None,
                }
            }
        }

        /// The next sealed control message other than a ping.
        pub(crate) fn receive_non_ping(&mut self) -> Option<Value> {
            loop {
                let message = self.receive()?;
                if message["type"] != "ping" {
                    return Some(message);
                }
            }
        }
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        use sha2_011::{Digest, Sha256};
        Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
    }

    pub(crate) fn next_event(events: &mpsc::Receiver<BridgeEvent>) -> BridgeEvent {
        events.recv_timeout(Duration::from_secs(5)).expect("bridge event")
    }

    #[test]
    fn upgrade_checks_follow_the_vectors() {
        let bridge = test_bridge(PING_INTERVAL);
        bridge.add_pending(launch(ORIGIN, 1));
        let port = bridge.port();
        let upgrades = &crate::app::secureplan::vectors::json("handshake.json")["upgrades"];
        for case in upgrades["cases"].as_array().unwrap() {
            let host = case["host"].as_str().map(|h| h.replace("47815", &port.to_string()).replace("47816", &(port.wrapping_add(1)).to_string()));
            let result = upgrade(port, host.as_deref(), case["origin"].as_str());
            assert_eq!(result.is_ok(), case["accept"].as_bool().unwrap(), "{}", case["name"]);
        }
    }

    #[test]
    fn no_upgrade_is_accepted_without_a_pending_pairing() {
        let bridge = test_bridge(PING_INTERVAL);
        let port = bridge.port();
        assert!(upgrade(port, Some(&format!("127.0.0.1:{port}")), Some(ORIGIN)).is_err());
        // Plain HTTP gets nothing but a refusal: no content, files or CORS.
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        write!(stream, "GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: {ORIGIN}\r\n\r\n").unwrap();
        let mut response = String::new();
        let _ = stream.read_to_string(&mut response);
        assert!(!response.contains("Access-Control"), "{response}");
        assert!(!response.starts_with("HTTP/1.1 200"), "{response}");
    }

    #[test]
    fn full_handshake_opens_a_session_and_sends_no_survey_data_first() {
        let bridge = test_bridge(PING_INTERVAL);
        let events = bridge.take_events().unwrap();
        let request = launch(ORIGIN, 2);
        bridge.add_pending(request.clone());
        let mut web = connect_web(bridge.port(), &request).expect("handshake");
        let BridgeEvent::Opened { session, origin, needs_confirmation, .. } = next_event(&events) else {
            panic!("expected Opened")
        };
        assert_eq!(origin, ORIGIN);
        assert!(!needs_confirmation);
        // The pairing was consumed: the same launch cannot pair again.
        assert!(connect_web(bridge.port(), &request).is_err());
        web.send(&json!({ "type": "sessionMode", "requestId": "m1", "mode": "view", "reason": "leaseLost" }));
        let BridgeEvent::Message { message: Inbound::Session { kind, .. }, .. } = next_event(&events) else {
            panic!("expected a session message")
        };
        assert_eq!(kind, "sessionMode");
        assert!(bridge.send(session, protocol::session_state("s1", false, json!(null), json!(null), json!(null))));
        assert_eq!(web.receive_non_ping().unwrap()["type"], "sessionState");
        assert!(bridge.close(session, "documentClosed"));
        assert_eq!(web.receive_non_ping().unwrap()["reason"], "documentClosed");
        assert!(matches!(next_event(&events), BridgeEvent::Closed { .. }));
    }

    #[test]
    fn handshake_rejections() {
        let bridge = test_bridge(PING_INTERVAL);
        let port = bridge.port();
        let handshake = crate::app::secureplan::vectors::json("handshake.json");
        let hello = &handshake["steps"][0]["message"];
        // Unknown pairing, wrong protocol: no challenge.
        let request = launch(ORIGIN, 3);
        bridge.add_pending(request.clone());
        for bad in [
            json!({ "pairing": b64url_encode(&[9u8; 16]) }),
            json!({ "pairing": b64url_encode(request.pairing.expose()), "protocols": [2] }),
            json!({ "pairing": b64url_encode(request.pairing.expose()), "extra": true }),
        ] {
            let (mut socket, _) = upgrade(port, Some(&format!("127.0.0.1:{port}")), Some(ORIGIN)).unwrap();
            let mut message = hello.clone();
            for (key, value) in bad.as_object().unwrap() {
                message[key] = value.clone();
            }
            socket.send(Message::text(message.to_string())).unwrap();
            assert!(read_text(&mut socket).is_none(), "challenge sent for {bad}");
        }
        // A binary frame before the handshake closes the connection.
        let (mut socket, _) = upgrade(port, Some(&format!("127.0.0.1:{port}")), Some(ORIGIN)).unwrap();
        socket.send(Message::binary(vec![1, 2, 3])).unwrap();
        assert!(read_text(&mut socket).is_none());
        // A wrong proofW gets no welcome, and the pairing stays usable.
        let (mut socket, _) = upgrade(port, Some(&format!("127.0.0.1:{port}")), Some(ORIGIN)).unwrap();
        let mut message = hello.clone();
        message["pairing"] = json!(b64url_encode(request.pairing.expose()));
        socket.send(Message::text(message.to_string())).unwrap();
        assert!(read_text(&mut socket).is_some(), "challenge");
        socket.send(Message::text(json!({ "type": "prove", "requestId": "p1", "proofW": b64url_encode(&[0u8; 32]) }).to_string())).unwrap();
        assert!(read_text(&mut socket).is_none(), "welcome after a bad proof");
        assert!(connect_web(port, &request).is_ok());
    }

    #[test]
    fn revoking_trust_invalidates_pending_pairings_handshakes_and_sessions() {
        let bridge = test_bridge(PING_INTERVAL);
        let events = bridge.take_events().unwrap();
        let port = bridge.port();
        let trusted = |origins: &[&str]| origins.iter().map(|o| o.to_string()).collect::<HashSet<_>>();
        bridge.set_trusted_origins(trusted(&[ORIGIN]));

        // launch → revoke → handshake: refused.
        let request = launch(ORIGIN, 70);
        bridge.add_pending(request.clone());
        bridge.set_trusted_origins(trusted(&[]));
        assert!(connect_web(port, &request).is_err(), "a revoked origin still paired");

        // A launch for an untrusted origin is never added.
        bridge.add_pending(launch(ORIGIN, 71));
        assert!(connect_web(port, &launch(ORIGIN, 71)).is_err());

        // Revoked in the middle of a handshake (after hello, before prove).
        bridge.set_trusted_origins(trusted(&[ORIGIN]));
        let request = launch(ORIGIN, 72);
        bridge.add_pending(request.clone());
        let (mut socket, _) = upgrade(port, Some(&format!("127.0.0.1:{port}")), Some(ORIGIN)).unwrap();
        let nonce_w = [0x42u8; 32];
        let hello = json!({ "type": "hello", "requestId": "h1", "pairing": b64url_encode(request.pairing.expose()), "nonceW": b64url_encode(&nonce_w), "webVersion": "0.1.0", "minDesktopVersion": "0.1.0", "protocols": [1] });
        socket.send(Message::text(hello.to_string())).unwrap();
        let challenge: Value = serde_json::from_str(&read_text(&mut socket).expect("challenge")).unwrap();
        bridge.set_trusted_origins(trusted(&[]));
        let nonce_d: [u8; 32] = channel::b64url_array(challenge["nonceD"].as_str().unwrap()).unwrap();
        let proof_w = channel::web_proof(request.token.expose(), &nonce_w, &nonce_d);
        socket.send(Message::text(json!({ "type": "prove", "requestId": "p1", "proofW": b64url_encode(&proof_w) }).to_string())).unwrap();
        assert!(read_text(&mut socket).is_none(), "welcome after the origin was revoked");

        // An open session from the origin is closed.
        bridge.set_trusted_origins(trusted(&[ORIGIN]));
        let request = launch(ORIGIN, 73);
        bridge.add_pending(request.clone());
        let mut web = connect_web(port, &request).unwrap();
        assert!(matches!(next_event(&events), BridgeEvent::Opened { .. }));
        bridge.set_trusted_origins(trusted(&[]));
        assert_eq!(web.receive_non_ping().unwrap()["reason"], "userCancelled");
    }

    #[test]
    fn expired_pairings_are_refused() {
        let offset = Arc::new(AtomicU64::new(0));
        let clock_offset = Arc::clone(&offset);
        let base = Instant::now();
        let bridge = Bridge::bind(BridgeConfig {
            ports: vec![0],
            clock: Arc::new(move || base + Duration::from_millis(clock_offset.load(Ordering::SeqCst))),
            ..BridgeConfig::default()
        })
        .unwrap();
        let request = launch(ORIGIN, 4);
        bridge.add_pending(request.clone());
        offset.store(120_001, Ordering::SeqCst);
        assert!(connect_web(bridge.port(), &request).is_err());
    }

    #[test]
    fn tampered_or_unknown_frames_close_the_session() {
        let bridge = test_bridge(PING_INTERVAL);
        let events = bridge.take_events().unwrap();
        for (index, frame) in [
            // unknown message type
            Some(json!({ "type": "shell", "requestId": "z" })),
            // unknown field
            Some(json!({ "type": "overlayUpdate", "requestId": "o", "overlayTransferId": 1, "x": 1 })),
            // tampered frame
            None,
        ]
        .into_iter()
        .enumerate()
        {
            let request = launch(ORIGIN, 10 + index as u8);
            bridge.add_pending(request.clone());
            let mut web = connect_web(bridge.port(), &request).unwrap();
            assert!(matches!(next_event(&events), BridgeEvent::Opened { .. }));
            match frame {
                Some(message) => web.send(&message),
                None => {
                    let mut sealed = web.sealer.seal(FrameKind::Control, b"{}").unwrap();
                    sealed[0] ^= 1;
                    web.send_raw(Message::text(b64url_encode(&sealed)));
                }
            }
            let close = web.receive_non_ping();
            if let Some(close) = close {
                assert_eq!(close["type"], "close");
            }
            let BridgeEvent::Closed { reason, .. } = next_event(&events) else { panic!("expected Closed") };
            assert_eq!(reason, "protocolError");
        }
    }

    #[test]
    fn pings_are_sent_and_transfers_arrive() {
        let bridge = test_bridge(Duration::from_millis(100));
        let events = bridge.take_events().unwrap();
        let request = launch(ORIGIN, 20);
        bridge.add_pending(request.clone());
        let mut web = connect_web(bridge.port(), &request).unwrap();
        let BridgeEvent::Opened { session, .. } = next_event(&events) else { panic!() };
        assert_eq!(web.receive().unwrap()["type"], "ping");

        let bytes = b"0123456789".to_vec();
        let start = json!({ "type": "transferStart", "requestId": "t1", "transferId": 5, "name": "synthetic.dxf", "mediaType": "image/vnd.dxf", "byteLength": bytes.len(), "sha256": sha256_hex(&bytes) });
        web.send(&start);
        for (seq, piece) in bytes.chunks(4).enumerate() {
            let mut chunk = 5u32.to_le_bytes().to_vec();
            chunk.extend_from_slice(&(seq as u32).to_le_bytes());
            chunk.extend_from_slice(piece);
            let sealed = web.sealer.seal(FrameKind::Chunk, &chunk).unwrap();
            web.send_raw(Message::binary(sealed));
        }
        let BridgeEvent::Transfer { transfer, .. } = next_event(&events) else { panic!("expected a transfer") };
        assert_eq!(transfer.bytes.expose(), &bytes);

        // Desktop-to-web transfer: transferStart, then sealed chunks.
        assert_eq!(bridge.send_transfer(session, "synthetic.pdf", "application/pdf", Arc::new(b"%PDF-1.7".to_vec())), Some(1));
        let start = web.receive_non_ping().unwrap();
        assert_eq!(start["transferId"], 1);
        assert_eq!(start["type"], "transferStart");
        assert_eq!(start["byteLength"], 8);
    }

    /// Announce `count` incoming transfers from the web that stay incomplete.
    fn start_incomplete(web: &mut Web, ids: std::ops::RangeInclusive<u32>) -> Vec<u8> {
        let payload = b"0123456789".to_vec();
        for id in ids {
            web.send(&json!({ "type": "transferStart", "requestId": format!("t{id}"), "transferId": id, "name": "synthetic.dxf", "mediaType": "image/vnd.dxf", "byteLength": payload.len(), "sha256": sha256_hex(&payload) }));
        }
        payload
    }

    fn complete_chunk(web: &mut Web, id: u32, payload: &[u8]) {
        let mut chunk = id.to_le_bytes().to_vec();
        chunk.extend_from_slice(&0u32.to_le_bytes());
        chunk.extend_from_slice(payload);
        let sealed = web.sealer.seal(FrameKind::Chunk, &chunk).unwrap();
        web.send_raw(Message::binary(sealed));
    }

    fn open_session(bridge: &Bridge, events: &mpsc::Receiver<BridgeEvent>, pairing: u8) -> (Web, SessionId) {
        let request = launch(ORIGIN, pairing);
        bridge.add_pending(request.clone());
        let web = connect_web(bridge.port(), &request).unwrap();
        let BridgeEvent::Opened { session, .. } = next_event(events) else { panic!("no session") };
        (web, session)
    }

    #[test]
    fn the_desktop_sends_while_the_web_has_its_two_in_flight_and_a_third_is_refused() {
        let bridge = test_bridge(PING_INTERVAL);
        let events = bridge.take_events().unwrap();
        let (mut web, session) = open_session(&bridge, &events, 80);
        // Both sides start at the same moment: the web its full share of two,
        // the desktop its one. Nobody waits and nothing is refused.
        let payload = start_incomplete(&mut web, 1..=2);
        assert!(bridge.send_transfer(session, "synthetic.pdf", "application/pdf", Arc::new(b"%PDF-1.7".to_vec())).is_some());
        assert!(bridge.send(session, protocol::session_state("s1", true, json!(null), json!(null), json!(null))));
        assert_eq!(web.receive_non_ping().unwrap()["type"], "transferStart");
        assert_eq!(web.receive_non_ping().unwrap()["type"], "sessionState");
        complete_chunk(&mut web, 1, &payload);
        assert!(matches!(next_event(&events), BridgeEvent::Transfer { .. }));
        // One of the web's is still in flight; a third beyond the share of two is refused.
        start_incomplete(&mut web, 3..=4);
        assert_eq!(web.receive_non_ping().unwrap()["reason"], "transferFailed");
    }

    #[test]
    fn closing_takes_priority_under_transfer_pressure() {
        // The web has its two transfers in flight and the desktop has a
        // transfer and a message queued when the session is told to close.
        let busy = |bridge: &Bridge, web: &mut Web, session: SessionId| {
            start_incomplete(web, 1..=2);
            std::thread::sleep(Duration::from_millis(100));
            assert!(bridge.send_transfer(session, "synthetic.pdf", "application/pdf", Arc::new(vec![7; 3 * transfer::MAX_CHUNK_PAYLOAD])).is_some());
            assert!(bridge.send(session, protocol::session_state("s1", true, json!(null), json!(null), json!(null))));
        };
        let closed_with = |web: &mut Web| loop {
            let message = web.receive_non_ping().expect("a close");
            if message["type"] == "close" {
                return message["reason"].as_str().unwrap().to_string();
            }
        };

        // An explicit close.
        let bridge = test_bridge(PING_INTERVAL);
        let events = bridge.take_events().unwrap();
        let (mut web, session) = open_session(&bridge, &events, 81);
        busy(&bridge, &mut web, session);
        assert!(bridge.close(session, "documentClosed"));
        assert_eq!(closed_with(&mut web), "documentClosed");

        // Superseded by a second pairing for the same survey.
        let bridge = test_bridge(PING_INTERVAL);
        let events = bridge.take_events().unwrap();
        let (mut web, session) = open_session(&bridge, &events, 82);
        busy(&bridge, &mut web, session);
        let second = launch(ORIGIN, 83);
        bridge.add_pending(second.clone());
        let _second = connect_web(bridge.port(), &second).unwrap();
        assert_eq!(closed_with(&mut web), "superseded");

        // Trust revoked.
        let bridge = test_bridge(PING_INTERVAL);
        let events = bridge.take_events().unwrap();
        bridge.set_trusted_origins([ORIGIN.to_string()].into());
        let (mut web, session) = open_session(&bridge, &events, 84);
        busy(&bridge, &mut web, session);
        bridge.set_trusted_origins(HashSet::new());
        assert_eq!(closed_with(&mut web), "userCancelled");
    }

    #[test]
    fn nothing_from_a_revoked_origin_reaches_the_application() {
        let bridge = test_bridge(PING_INTERVAL);
        let events = bridge.take_events().unwrap();
        let (mut web, _) = open_session(&bridge, &events, 85);
        let payload = start_incomplete(&mut web, 1..=2);
        std::thread::sleep(Duration::from_millis(100));
        // Revoked while the session thread is between frames, before its
        // close is queued: the next frame must still not be dispatched.
        *lock(&bridge.shared.trusted) = Some(HashSet::new());
        complete_chunk(&mut web, 1, &payload);
        web.send(&json!({ "type": "sessionMode", "requestId": "m1", "mode": "view", "reason": "leaseLost" }));
        loop {
            match next_event(&events) {
                BridgeEvent::Closed { reason, .. } => {
                    assert_eq!(reason, "userCancelled");
                    break;
                }
                BridgeEvent::Transfer { .. } | BridgeEvent::Message { .. } => panic!("dispatched after revocation"),
                _ => {}
            }
        }
    }

    #[test]
    fn a_second_pairing_for_the_same_survey_supersedes_the_first() {
        let bridge = test_bridge(PING_INTERVAL);
        let events = bridge.take_events().unwrap();
        let first = launch(ORIGIN, 30);
        bridge.add_pending(first.clone());
        let mut web_one = connect_web(bridge.port(), &first).unwrap();
        assert!(matches!(next_event(&events), BridgeEvent::Opened { .. }));
        let second = launch(ORIGIN, 31);
        bridge.add_pending(second.clone());
        let _web_two = connect_web(bridge.port(), &second).unwrap();
        let (a, b) = (next_event(&events), next_event(&events));
        assert!([&a, &b].iter().any(|e| matches!(e, BridgeEvent::Opened { .. })));
        assert!([&a, &b].iter().any(|e| matches!(e, BridgeEvent::Closed { reason, .. } if reason == "superseded")));
        let close = web_one.receive_non_ping().unwrap();
        assert_eq!(close["reason"], "superseded");
    }

    #[test]
    fn re_pairing_an_open_document_waits_for_confirmation() {
        let bridge = test_bridge(PING_INTERVAL);
        let events = bridge.take_events().unwrap();
        let first = launch(ORIGIN, 40);
        bridge.add_pending(first.clone());
        let mut web_one = connect_web(bridge.port(), &first).unwrap();
        let BridgeEvent::Opened { needs_confirmation: false, .. } = next_event(&events) else { panic!() };
        bridge.set_document_open(ORIGIN, first.survey.expose(), true);

        let second = launch(ORIGIN, 41);
        bridge.add_pending(second.clone());
        let mut web_two = connect_web(bridge.port(), &second).unwrap();
        let BridgeEvent::Opened { session, needs_confirmation: true, .. } = next_event(&events) else {
            panic!("expected a confirmation request")
        };
        web_two.send(&json!({ "type": "sessionMode", "requestId": "m1", "mode": "edit", "reason": "leaseAcquired" }));
        assert!(events.recv_timeout(Duration::from_millis(300)).is_err(), "delivered before confirmation");
        assert!(bridge.confirm(session, true));
        // The held message arrives, and the first session closes (either order).
        let (first, second) = (next_event(&events), next_event(&events));
        assert!([&first, &second].iter().any(|e| matches!(e, BridgeEvent::Message { session: s, .. } if *s == session)));
        assert!([&first, &second].iter().any(|e| matches!(e, BridgeEvent::Closed { reason, .. } if reason == "superseded")));
        assert_eq!(web_one.receive_non_ping().unwrap()["reason"], "superseded");

        // Declining closes the new session instead.
        let third = launch(ORIGIN, 42);
        bridge.add_pending(third.clone());
        let mut web_three = connect_web(bridge.port(), &third).unwrap();
        let BridgeEvent::Opened { session, needs_confirmation: true, .. } = next_event(&events) else { panic!() };
        assert!(bridge.confirm(session, false));
        assert_eq!(web_three.receive_non_ping().unwrap()["reason"], "userCancelled");
    }

    /// A process occupying a lower bridge port relays everything to the
    /// desktop (rewriting `Host`). It sees only ciphertext after the
    /// handshake and cannot alter a frame unnoticed.
    #[test]
    fn a_relay_through_a_squatted_port_sees_only_ciphertext() {
        let bridge = test_bridge(PING_INTERVAL);
        let events = bridge.take_events().unwrap();
        let desktop_port = bridge.port();
        let squatter = TcpListener::bind("127.0.0.1:0").unwrap();
        let relay_port = squatter.local_addr().unwrap().port();
        let captured = Arc::new(Mutex::new(Vec::<u8>::new()));
        let capture = Arc::clone(&captured);
        std::thread::spawn(move || {
            let (mut web_side, _) = squatter.accept().unwrap();
            let mut desktop_side = TcpStream::connect(("127.0.0.1", desktop_port)).unwrap();
            // Forward the upgrade request with Host rewritten, then pipe bytes.
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                web_side.read_exact(&mut byte).unwrap();
                head.push(byte[0]);
            }
            let head = String::from_utf8(head).unwrap().replace(&format!("127.0.0.1:{relay_port}"), &format!("127.0.0.1:{desktop_port}"));
            desktop_side.write_all(head.as_bytes()).unwrap();
            let pipe = |mut from: TcpStream, mut to: TcpStream, log: Arc<Mutex<Vec<u8>>>| {
                std::thread::spawn(move || {
                    let mut buf = [0u8; 4096];
                    while let Ok(n) = from.read(&mut buf) {
                        if n == 0 || to.write_all(&buf[..n]).is_err() {
                            break;
                        }
                        lock(&log).extend_from_slice(&buf[..n]);
                    }
                })
            };
            pipe(web_side.try_clone().unwrap(), desktop_side.try_clone().unwrap(), Arc::clone(&capture));
            pipe(desktop_side, web_side, capture);
        });

        let request = launch(ORIGIN, 50);
        bridge.add_pending(request.clone());
        let mut web = connect_web(relay_port, &request).expect("handshake through the relay");
        let BridgeEvent::Opened { session, .. } = next_event(&events) else { panic!() };
        let label = "Synthetic survey behind a relay";
        web.send(&json!({ "type": "sessionMode", "requestId": "m1", "mode": "view", "reason": "leaseLost" }));
        let open = crate::app::secureplan::vectors::json("messages/valid.json")["samples"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == "openSession-import-empty")
            .unwrap()["message"]
            .clone();
        let mut open = open;
        open["surveyLabel"] = json!(label);
        web.send(&open);
        assert!(matches!(next_event(&events), BridgeEvent::Message { .. }));
        let BridgeEvent::Message { message: Inbound::Session { body, .. }, .. } = next_event(&events) else { panic!() };
        assert_eq!(body["surveyLabel"], label, "the desktop reads the sealed message");
        assert!(bridge.send(session, protocol::session_state("s1", true, json!(null), json!(null), json!(null))));
        assert_eq!(web.receive_non_ping().unwrap()["dirty"], true);
        std::thread::sleep(Duration::from_millis(100));
        let seen = lock(&captured).clone();
        let contains = |needle: &[u8]| seen.windows(needle.len()).any(|w| w == needle);
        assert!(!contains(label.as_bytes()), "the relay read the survey label");
        assert!(!contains(b"sessionState") && !contains(b"sessionMode") && !contains(b"welcome"));
        assert!(contains(b"challenge"), "the relay did carry the handshake");
    }
}

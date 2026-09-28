//! Per-user hand-off of launch URLs between instances (DSK-04), and the
//! single SecurePlan CAD window: any later launch, with or without a URL,
//! goes to the running instance and exits. Without a URL it asks the running
//! instance to bring its window to the front.
//!
//! Pairing data never travels as a forwarded process argument, never through
//! upstream's unauthenticated single-instance port, and never into logs. The
//! running instance listens on an ephemeral loopback port and writes the port
//! and a fresh 32-byte secret to `handoff.json` in SecurePlan CAD's config
//! directory, readable by the current user only (mode 0600 on Unix; the
//! per-user profile ACL on Windows). A second launch reads that file and both
//! sides prove knowledge of the secret before the URL is sent, so a process
//! that has taken over a stale port learns nothing.
//!
//! On Windows the operating system passes the first launch's URL as a process
//! argument; that instance uses it once and never forwards it.
//!
//! One window: the primary holds an exclusive per-user lock on
//! `primary.lock` for its lifetime (the operating system releases it when the
//! process ends, however it ends). A launch that cannot take the lock never
//! becomes a second primary: it keeps handing its request over until the
//! owner answers, and gives up without a window after [`CLAIM_WAIT`].

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use hmac::{Hmac, KeyInit, Mac};
use serde_json::{json, Value};
use sha2_011::Sha256;

const IO_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_LINE: u64 = 16 * 1024;
const SERVER_LABEL: &[u8] = b"secureplan-cad handoff server";
const CLIENT_LABEL: &[u8] = b"secureplan-cad handoff client";

/// Whether a command-line argument is a SecurePlan CAD launch URL.
pub fn is_launch_url(argument: &str) -> bool {
    argument.get(..15).is_some_and(|scheme| scheme.eq_ignore_ascii_case("secureplan-cad:"))
}

/// Split launch URLs out of the positional arguments, so they are used by
/// this process only and never forwarded as paths.
pub fn split_launch_args(arguments: Vec<PathBuf>) -> (Vec<String>, Vec<PathBuf>) {
    let (urls, files): (Vec<PathBuf>, Vec<PathBuf>) =
        arguments.into_iter().partition(|argument| argument.to_str().is_some_and(is_launch_url));
    (urls.into_iter().filter_map(|url| url.into_os_string().into_string().ok()).collect(), files)
}

pub fn descriptor_path() -> Option<PathBuf> {
    crate::config::config_dir().map(|dir| dir.join("handoff.json"))
}

/// Forward launch URLs to the running instance. `true` only when there were
/// URLs and every one was delivered.
pub fn forward_launches(urls: &[String]) -> bool {
    let Some(path) = descriptor_path() else { return false };
    !urls.is_empty() && urls.iter().all(|url| send_launch(&path, url).is_ok())
}

/// The flag the macOS launcher starts the GUI with while a launch URL is on
/// its way over the hand-off: start without the editor window, as a link
/// start does (BRG-02). It carries no pairing data.
pub const AWAITING_LAUNCH_ARG: &str = "--secureplan-awaiting-launch";

/// How long a launch keeps trying to reach the instance that owns the window.
pub const CLAIM_WAIT: Duration = Duration::from_secs(10);
const CLAIM_RETRY: Duration = Duration::from_millis(50);

/// What a launch turned out to be.
#[derive(Debug)]
pub enum Claim {
    /// This process owns the window; the lock is held until it exits (none
    /// when the lock file cannot be opened at all).
    Primary(Option<std::fs::File>),
    /// The running instance took every request (or there were none to send).
    Forwarded,
    /// Another instance owns the window but did not answer in time.
    Unanswered,
}

/// Become the primary, or hand `requests` to the instance that is.
pub fn claim_window(requests: &[Request]) -> Claim {
    match crate::config::config_dir() {
        Some(dir) => claim(&dir, requests, CLAIM_WAIT),
        None => Claim::Primary(None),
    }
}

/// [`claim_window`] in `dir` (tests use their own).
pub fn claim(dir: &Path, requests: &[Request], wait: Duration) -> Claim {
    let descriptor = dir.join("handoff.json");
    let lock = std::fs::create_dir_all(dir).and_then(|_| {
        std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(dir.join("primary.lock"))
    });
    let deadline = Instant::now() + wait;
    let mut pending: Vec<&Request> = requests.iter().collect();
    loop {
        match &lock {
            Ok(file) => match file.try_lock() {
                Ok(()) => return Claim::Primary(lock.ok()),
                Err(std::fs::TryLockError::WouldBlock) => {}
                Err(std::fs::TryLockError::Error(_)) => return Claim::Primary(None),
            },
            // No lock to take: the hand-off alone decides, as before.
            Err(_) => {
                pending.retain(|request| send(&descriptor, request).is_err());
                return if pending.is_empty() && !requests.is_empty() { Claim::Forwarded } else { Claim::Primary(None) };
            }
        }
        pending.retain(|request| send(&descriptor, request).is_err());
        if pending.is_empty() {
            return Claim::Forwarded;
        }
        if Instant::now() >= deadline {
            return Claim::Unanswered;
        }
        std::thread::sleep(CLAIM_RETRY);
    }
}

/// What another launch asks of the running instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    Launch(String),
    /// Bring the window to the front.
    Focus,
}

/// The primary's lock and descriptor, held until the process exits.
static PRIMARY: Mutex<Option<(Option<std::fs::File>, PathBuf)>> = Mutex::new(None);
/// Set once the primary is shutting down: no more hand-offs are taken.
static STOPPING: AtomicBool = AtomicBool::new(false);

/// Called by the instance that shows the editor, holding `lock` from
/// [`claim_window`]: serve later hand-offs, then deliver this process's own
/// launch URLs once.
pub fn start_primary(lock: Option<std::fs::File>, urls: Vec<String>) {
    if let Some(path) = descriptor_path() {
        let _ = serve(&path, |request| match request {
            Request::Launch(url) => super::deliver_launch(url),
            Request::Focus => super::deliver_focus(),
        });
        *PRIMARY.lock().unwrap_or_else(|e| e.into_inner()) = Some((lock, path));
    }
    for url in urls {
        super::deliver_launch(url);
    }
}

/// The primary is exiting: take no more hand-offs and withdraw the
/// descriptor, so a new launch waits for the lock and then starts afresh.
pub fn stop_serving() {
    let primary = PRIMARY.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, path)) = primary.as_ref() {
        STOPPING.store(true, Ordering::SeqCst);
        let _ = std::fs::remove_file(path);
    }
}

/// Why a hand-off failed. Carries no URL content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandoffError {
    NoRunningInstance,
    Unauthenticated,
    Refused,
    Io,
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn from_hex<const N: usize>(text: &str) -> Option<[u8; N]> {
    let mut out = [0u8; N];
    if text.len() != N * 2 {
        return None;
    }
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(text.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

fn proof(secret: &[u8; 32], label: &[u8], nonce: &[u8; 32]) -> Hmac<Sha256> {
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(secret).expect("any key length");
    mac.update(label);
    mac.update(nonce);
    mac
}

fn random32() -> std::io::Result<[u8; 32]> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(std::io::Error::other)?;
    Ok(bytes)
}

fn read_json(reader: &mut BufReader<TcpStream>) -> Option<Value> {
    let mut line = String::new();
    reader.by_ref().take(MAX_LINE).read_line(&mut line).ok()?;
    serde_json::from_str(&line).ok()
}

fn write_json(stream: &mut TcpStream, value: &Value) -> std::io::Result<()> {
    writeln!(stream, "{value}")?;
    stream.flush()
}

/// Write the descriptor readable by the current user only.
fn write_descriptor(path: &Path, port: u16, secret: &[u8; 32]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let temporary = path.with_extension("json.tmp");
    let _ = std::fs::remove_file(&temporary);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    {
        let mut file = options.open(&temporary)?;
        let descriptor = json!({ "protocol": 1, "port": port, "secret": to_hex(secret), "pid": std::process::id() });
        writeln!(file, "{descriptor}")?;
        file.sync_all()?;
    }
    std::fs::rename(&temporary, path)
}

/// Serve hand-offs for this instance: every authenticated request is passed
/// to `deliver`.
pub fn serve(path: &Path, deliver: impl Fn(Request) + Send + Sync + 'static) -> std::io::Result<()> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let secret = random32()?;
    write_descriptor(path, listener.local_addr()?.port(), &secret)?;
    let deliver = std::sync::Arc::new(deliver);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            if STOPPING.load(Ordering::SeqCst) {
                continue;
            }
            let deliver = std::sync::Arc::clone(&deliver);
            std::thread::spawn(move || {
                if let Some(request) = accept_one(stream, &secret) {
                    deliver(request);
                }
            });
        }
    });
    Ok(())
}

/// One hand-off: prove the secret, check the client's proof, take the URL
/// or the focus request.
fn accept_one(stream: TcpStream, secret: &[u8; 32]) -> Option<Request> {
    stream.set_read_timeout(Some(IO_TIMEOUT)).ok()?;
    stream.set_write_timeout(Some(IO_TIMEOUT)).ok()?;
    let mut writer = stream.try_clone().ok()?;
    let mut reader = BufReader::new(stream);
    let hello = read_json(&mut reader)?;
    let client_nonce: [u8; 32] = from_hex(hello["nonce"].as_str()?)?;
    let server_nonce = random32().ok()?;
    let server_proof = proof(secret, SERVER_LABEL, &client_nonce).finalize().into_bytes();
    write_json(&mut writer, &json!({ "proof": to_hex(&server_proof), "nonce": to_hex(&server_nonce) })).ok()?;
    let message = read_json(&mut reader)?;
    let client_proof: [u8; 32] = from_hex(message["proof"].as_str()?)?;
    let request = match message["op"].as_str()? {
        "launch" => Request::Launch(message["url"].as_str()?.to_string()),
        "focus" => Request::Focus,
        _ => return None,
    };
    let accepted = proof(secret, CLIENT_LABEL, &server_nonce).verify_slice(&client_proof).is_ok()
        && match &request {
            Request::Launch(url) => is_launch_url(url),
            Request::Focus => true,
        };
    let _ = write_json(&mut writer, &json!({ "ok": accepted }));
    accepted.then_some(request)
}

/// Pass `url` to the running instance described by `path`.
pub fn send_launch(path: &Path, url: &str) -> Result<(), HandoffError> {
    send(path, &Request::Launch(url.to_string()))
}

/// Pass `request` to the running instance described by `path`.
pub fn send(path: &Path, request: &Request) -> Result<(), HandoffError> {
    let descriptor: Value = std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .ok_or(HandoffError::NoRunningInstance)?;
    let port = descriptor["port"].as_u64().and_then(|p| u16::try_from(p).ok()).ok_or(HandoffError::NoRunningInstance)?;
    let secret: [u8; 32] = descriptor["secret"].as_str().and_then(from_hex).ok_or(HandoffError::NoRunningInstance)?;
    let stream = TcpStream::connect_timeout(&(Ipv4Addr::LOCALHOST, port).into(), IO_TIMEOUT)
        .map_err(|_| HandoffError::NoRunningInstance)?;
    stream.set_read_timeout(Some(IO_TIMEOUT)).map_err(|_| HandoffError::Io)?;
    stream.set_write_timeout(Some(IO_TIMEOUT)).map_err(|_| HandoffError::Io)?;
    let mut writer = stream.try_clone().map_err(|_| HandoffError::Io)?;
    let mut reader = BufReader::new(stream);
    let client_nonce = random32().map_err(|_| HandoffError::Io)?;
    write_json(&mut writer, &json!({ "op": "hello", "nonce": to_hex(&client_nonce) })).map_err(|_| HandoffError::Io)?;
    let answer = read_json(&mut reader).ok_or(HandoffError::Unauthenticated)?;
    let server_proof: [u8; 32] = answer["proof"].as_str().and_then(from_hex).ok_or(HandoffError::Unauthenticated)?;
    let server_nonce: [u8; 32] = answer["nonce"].as_str().and_then(from_hex).ok_or(HandoffError::Unauthenticated)?;
    // Only a listener that knows the secret ever sees the URL.
    proof(&secret, SERVER_LABEL, &client_nonce)
        .verify_slice(&server_proof)
        .map_err(|_| HandoffError::Unauthenticated)?;
    let client_proof = proof(&secret, CLIENT_LABEL, &server_nonce).finalize().into_bytes();
    let message = match request {
        Request::Launch(url) => json!({ "op": "launch", "proof": to_hex(&client_proof), "url": url }),
        Request::Focus => json!({ "op": "focus", "proof": to_hex(&client_proof) }),
    };
    write_json(&mut writer, &message).map_err(|_| HandoffError::Io)?;
    let done = read_json(&mut reader).ok_or(HandoffError::Io)?;
    if done["ok"] == true {
        Ok(())
    } else {
        Err(HandoffError::Refused)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    const URL: &str = "secureplan-cad://pair?v=1&origin=https%3A%2F%2Fsecureplan.example&pairing=AAAAAAAAAAAAAAAAAAAAAA&token=c2VjcmV0LXRva2VuLXNlY3JldC10b2tlbi1zZWNyZXQtdA&survey=7d3c1f6e-2b4a-4c8d-9e0f-1a2b3c4d5e6f&intent=edit";

    fn temp_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("secureplan_handoff_{tag}_{}", std::process::id())).join("handoff.json")
    }

    #[test]
    fn launch_urls_are_never_forwarded_as_arguments() {
        let arguments = vec![PathBuf::from(URL), PathBuf::from("/tmp/plan.dxf"), PathBuf::from("SECUREPLAN-CAD://pair?x")];
        let (urls, files) = split_launch_args(arguments);
        assert_eq!(urls.len(), 2);
        assert_eq!(files, vec![PathBuf::from("/tmp/plan.dxf")]);
        assert!(files.iter().all(|f| !f.to_string_lossy().contains("token=")));
    }

    #[test]
    fn a_windows_first_launch_argument_is_used_once_and_not_re_forwarded() {
        // The OS starts `SecurePlanCAD.exe <url>`: the URL leaves the argument
        // list the editor opens or forwards, and is delivered exactly once.
        let (urls, files) = split_launch_args(vec![PathBuf::from(URL)]);
        assert_eq!(urls, vec![URL.to_string()]);
        assert!(files.is_empty());
    }

    #[test]
    fn hand_off_reaches_the_running_instance_over_a_user_only_channel() {
        let path = temp_path("deliver");
        let (sender, receiver) = mpsc::channel();
        let sender = std::sync::Mutex::new(sender);
        serve(&path, move |request| {
            let _ = sender.lock().unwrap().send(request);
        })
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert_eq!(send_launch(&path, URL), Ok(()));
        assert_eq!(receiver.recv_timeout(Duration::from_secs(5)).unwrap(), Request::Launch(URL.into()));
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn a_listener_without_the_secret_never_sees_the_url() {
        // A stale descriptor whose port another process now holds.
        let path = temp_path("impostor");
        let impostor = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        write_descriptor(&path, impostor.local_addr().unwrap().port(), &[7u8; 32]).unwrap();
        let seen = std::thread::spawn(move || {
            let (stream, _) = impostor.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut writer = stream.try_clone().unwrap();
            let mut reader = BufReader::new(stream);
            let mut everything = String::new();
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            everything.push_str(&line);
            // Answer with a proof made without the secret.
            writeln!(writer, "{}", json!({ "proof": to_hex(&[0u8; 32]), "nonce": to_hex(&[1u8; 32]) })).unwrap();
            let _ = reader.read_to_string(&mut everything);
            everything
        });
        assert_eq!(send_launch(&path, URL), Err(HandoffError::Unauthenticated));
        let seen = seen.join().unwrap();
        assert!(!seen.contains("token="), "the URL reached an impostor: {seen}");
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn a_client_without_the_secret_is_refused() {
        let path = temp_path("client");
        let (sender, receiver) = mpsc::channel::<Request>();
        let sender = std::sync::Mutex::new(sender);
        serve(&path, move |request| {
            let _ = sender.lock().unwrap().send(request);
        })
        .unwrap();
        // Speak the protocol without knowing the secret.
        let descriptor: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let port = descriptor["port"].as_u64().unwrap() as u16;
        let stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
        stream.set_read_timeout(Some(IO_TIMEOUT)).unwrap();
        let mut writer = stream.try_clone().unwrap();
        let mut reader = BufReader::new(stream);
        write_json(&mut writer, &json!({ "op": "hello", "nonce": to_hex(&[3u8; 32]) })).unwrap();
        assert!(read_json(&mut reader).is_some());
        write_json(&mut writer, &json!({ "op": "launch", "proof": to_hex(&[0u8; 32]), "url": URL })).unwrap();
        assert_eq!(read_json(&mut reader).unwrap()["ok"], false);
        // Nor may it ask for the window.
        let stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
        stream.set_read_timeout(Some(IO_TIMEOUT)).unwrap();
        let mut writer = stream.try_clone().unwrap();
        let mut reader = BufReader::new(stream);
        write_json(&mut writer, &json!({ "op": "hello", "nonce": to_hex(&[3u8; 32]) })).unwrap();
        assert!(read_json(&mut reader).is_some());
        write_json(&mut writer, &json!({ "op": "focus", "proof": to_hex(&[0u8; 32]) })).unwrap();
        assert_eq!(read_json(&mut reader).unwrap()["ok"], false);
        assert!(receiver.recv_timeout(Duration::from_millis(300)).is_err(), "delivered without the secret");
        assert_eq!(send_launch(&temp_path("missing"), URL), Err(HandoffError::NoRunningInstance));
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    /// A launch without a URL asks the running instance for its window, and
    /// sends nothing else.
    #[test]
    fn a_plain_launch_asks_the_running_instance_for_its_window() {
        let path = temp_path("focus");
        let (sender, receiver) = mpsc::channel();
        let sender = std::sync::Mutex::new(sender);
        serve(&path, move |request| {
            let _ = sender.lock().unwrap().send(request);
        })
        .unwrap();
        assert_eq!(send(&path, &Request::Focus), Ok(()));
        assert_eq!(receiver.recv_timeout(Duration::from_secs(5)).unwrap(), Request::Focus);
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
        // A crash left the descriptor behind: nothing listens, so this launch
        // starts as the primary.
        let stale = temp_path("stale");
        let closed = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        write_descriptor(&stale, closed.local_addr().unwrap().port(), &[9u8; 32]).unwrap();
        drop(closed);
        assert_eq!(send(&stale, &Request::Focus), Err(HandoffError::NoRunningInstance));
        std::fs::remove_dir_all(stale.parent().unwrap()).ok();
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("secureplan_claim_{tag}_{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        dir
    }

    /// A primary as `main` runs one: it serves once it holds the lock, after
    /// `delay` (its start-up), and keeps the lock until `stop` is set.
    fn primary(dir: PathBuf, delay: Duration, stop: std::sync::Arc<AtomicBool>, seen: mpsc::Sender<Request>) -> std::thread::JoinHandle<bool> {
        std::thread::spawn(move || {
            let Claim::Primary(lock) = claim(&dir, &[], Duration::ZERO) else { return false };
            std::thread::sleep(delay);
            let seen = std::sync::Mutex::new(seen);
            serve(&dir.join("handoff.json"), move |request| {
                let _ = seen.lock().unwrap().send(request);
            })
            .unwrap();
            while !stop.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(10));
            }
            drop(lock);
            true
        })
    }

    /// DSK-04, one window: launches at the same moment make one primary; the
    /// others hand over to it.
    #[test]
    fn simultaneous_launches_make_one_primary() {
        let dir = temp_dir("simultaneous");
        let (sender, receiver) = mpsc::channel();
        let launches: Vec<_> = (0..6)
            .map(|_| {
                let (dir, sender) = (dir.clone(), sender.clone());
                std::thread::spawn(move || match claim(&dir, &[Request::Focus], Duration::from_secs(10)) {
                    Claim::Primary(lock) => {
                        let sender = std::sync::Mutex::new(sender);
                        serve(&dir.join("handoff.json"), move |request| {
                            let _ = sender.lock().unwrap().send(request);
                        })
                        .unwrap();
                        Some(lock)
                    }
                    Claim::Forwarded => None,
                    Claim::Unanswered => panic!("the primary never answered"),
                })
            })
            .collect();
        let locks: Vec<_> = launches.into_iter().map(|launch| launch.join().unwrap()).collect();
        assert_eq!(locks.iter().filter(|lock| lock.is_some()).count(), 1, "one primary");
        for _ in 0..5 {
            assert_eq!(receiver.recv_timeout(Duration::from_secs(5)).unwrap(), Request::Focus);
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A launch while the primary is still starting (its descriptor not yet
    /// written, a crash's stale one in its place) waits for it; a launch
    /// after the primary ended starts afresh; a primary that never answers
    /// is not joined by a second window.
    #[test]
    fn a_launch_waits_for_a_starting_primary_and_takes_over_an_ended_one() {
        let dir = temp_dir("delayed");
        std::fs::create_dir_all(&dir).unwrap();
        let closed = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        write_descriptor(&dir.join("handoff.json"), closed.local_addr().unwrap().port(), &[9u8; 32]).unwrap();
        drop(closed);
        let (sender, receiver) = mpsc::channel();
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let first = primary(dir.clone(), Duration::from_millis(400), stop.clone(), sender.clone());
        std::thread::sleep(Duration::from_millis(100));
        assert!(matches!(claim(&dir, &[Request::Launch(URL.into())], Duration::from_secs(10)), Claim::Forwarded));
        assert_eq!(receiver.recv_timeout(Duration::from_secs(5)).unwrap(), Request::Launch(URL.into()));
        // The macOS launcher's awaiting start has nothing to send: it leaves.
        assert!(matches!(claim(&dir, &[], Duration::from_secs(10)), Claim::Forwarded));
        stop.store(true, Ordering::SeqCst);
        assert!(first.join().unwrap());
        // The primary is gone, its listener with it (its descriptor stale):
        // the next launch is the primary.
        let closed = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        write_descriptor(&dir.join("handoff.json"), closed.local_addr().unwrap().port(), &[9u8; 32]).unwrap();
        drop(closed);
        assert!(matches!(claim(&dir, &[Request::Focus], Duration::from_secs(10)), Claim::Primary(Some(_))));
        // A primary that holds the window but never answers.
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let silent = primary(dir.clone(), Duration::from_secs(3600), stop.clone(), sender);
        std::thread::sleep(Duration::from_millis(100));
        assert!(matches!(claim(&dir, &[Request::Focus], Duration::from_millis(300)), Claim::Unanswered));
        stop.store(true, Ordering::SeqCst);
        drop(silent);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn errors_never_carry_the_url() {
        let error = send_launch(&temp_path("missing-2"), URL).unwrap_err();
        assert!(!format!("{error:?}").contains("token"));
    }
}

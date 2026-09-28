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
//! On Windows and Linux the operating system passes the first launch's URL as
//! a process argument (Linux: the `.desktop` file's `Exec=… %u`); that
//! instance uses it once and never forwards it. On Linux other local users
//! can read process arguments (`/proc/<pid>/cmdline`), launch token included,
//! so the Linux package is for internal testing only and is not released.
//!
//! One window: the primary holds an exclusive per-user lock on
//! `primary.lock` for its lifetime (the operating system releases it when the
//! process ends, however it ends), and a process that cannot take the lock or
//! serve the hand-off never shows a window. A launch that finds the lock held
//! keeps handing its request over until the owner takes it, and gives up
//! without a window after [`CLAIM_WAIT`]. The owner answers only once its
//! application has taken the request, and refuses everything once it begins
//! to exit, so the other launch retries and starts afresh instead of losing
//! it.

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
/// How long the running instance waits for its application to take a
/// handed-over request before refusing it (the other launch then retries).
pub const TAKE_WAIT: Duration = Duration::from_secs(5);
/// How long the macOS launcher keeps handing a link over.
pub const LAUNCHER_WAIT: Duration = Duration::from_secs(30);

/// What a launch turned out to be.
#[derive(Debug)]
pub enum Claim {
    /// This process owns the window: the lock is held until it exits.
    Primary(std::fs::File),
    /// The running instance took every request, or, for the macOS launcher's
    /// awaiting start (no requests), is ready to take the launcher's link.
    Forwarded,
    /// Another instance owns the window but did not answer in time.
    Unanswered,
    /// The lock cannot be taken at all (no usable settings folder) and no
    /// running instance answered: nothing may start.
    Unavailable,
}

/// Become the primary, or hand `requests` to the instance that is.
pub fn claim_window(requests: &[Request]) -> Claim {
    match crate::config::config_dir() {
        Some(dir) => claim(&dir, requests, CLAIM_WAIT),
        None => Claim::Unavailable,
    }
}

/// [`claim_window`] in `dir` (tests use their own). Owning the lock is
/// ownership of the window; the descriptor is readiness to take requests.
pub fn claim(dir: &Path, requests: &[Request], wait: Duration) -> Claim {
    let descriptor = dir.join("handoff.json");
    let lock = open_lock(dir);
    let deadline = Instant::now() + wait;
    let mut pending: Vec<&Request> = requests.iter().collect();
    loop {
        let attempt = lock.as_ref().ok().map(std::fs::File::try_lock);
        let owned_elsewhere = match attempt {
            Some(Ok(())) => return Claim::Primary(lock.expect("opened")),
            Some(Err(std::fs::TryLockError::WouldBlock)) => true,
            // No lock can be taken here: only a running owner can help.
            Some(Err(std::fs::TryLockError::Error(_))) | None => false,
        };
        if requests.is_empty() {
            // The launcher hands its link to an owner that is ready.
            if ready(&descriptor) {
                return Claim::Forwarded;
            }
        } else {
            pending.retain(|request| send(&descriptor, request).is_err());
            if pending.is_empty() {
                return Claim::Forwarded;
            }
        }
        if !owned_elsewhere {
            return Claim::Unavailable;
        }
        if Instant::now() >= deadline {
            return Claim::Unanswered;
        }
        std::thread::sleep(CLAIM_RETRY);
    }
}

/// The macOS launcher's hand-over of `urls`: whenever the running instance
/// does not take them and no standby it started is still running, it starts
/// one (`start`, an awaiting GUI that becomes the primary once the window is
/// free). `true` once every URL was taken; `false` when `wait` ran out.
pub fn deliver_with_standby<S>(
    path: &Path,
    urls: Vec<String>,
    wait: Duration,
    mut start: impl FnMut() -> Option<S>,
    running: impl Fn(&S) -> bool,
) -> bool {
    let deadline = Instant::now() + wait;
    let mut pending = urls;
    let mut standby: Option<S> = None;
    loop {
        pending.retain(|url| send_launch(path, url).is_err());
        if pending.is_empty() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        if !standby.as_ref().is_some_and(&running) {
            standby = start();
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// What another launch asks of the running instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    Launch(String),
    /// Bring the window to the front.
    Focus,
}

/// The primary's lock, listener and descriptor, held until the process exits.
static PRIMARY: Mutex<Option<(std::fs::File, Serving, PathBuf)>> = Mutex::new(None);
/// Set once the primary is shutting down: its application takes no more
/// handed-over requests.
static STOPPING: AtomicBool = AtomicBool::new(false);

/// Whether this primary is shutting down.
pub fn stopping() -> bool {
    STOPPING.load(Ordering::SeqCst)
}

/// Called by the instance that shows the editor, holding `lock` from
/// [`claim_window`]: serve later hand-offs, then deliver this process's own
/// launch URLs once. An instance that cannot serve must not start: other
/// launches could never reach it.
pub fn start_primary(lock: std::fs::File, urls: Vec<String>) -> std::io::Result<()> {
    let path = descriptor_path().ok_or_else(|| std::io::Error::other("no settings folder"))?;
    let serving = serve(&path, super::hand_off)?;
    *PRIMARY.lock().unwrap_or_else(|e| e.into_inner()) = Some((lock, serving, path));
    for url in urls {
        super::deliver_launch(url);
    }
    Ok(())
}

/// The primary is exiting: refuse every hand-off from now on (the other
/// launch retries, and starts afresh once this process has ended) and
/// withdraw the descriptor.
pub fn stop_serving() {
    let primary = PRIMARY.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, serving, path)) = primary.as_ref() {
        STOPPING.store(true, Ordering::SeqCst);
        serving.stop();
        let _ = std::fs::remove_file(path);
    }
}

/// Tell the user why SecurePlan CAD did not start: on standard error, and in
/// a message box, since an app started from a menu or a link has nobody
/// reading standard error (on Linux the box needs `zenity`).
pub fn report_start_problem(message: &str) {
    eprintln!("{message}");
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    let _ = rfd::MessageDialog::new()
        .set_level(rfd::MessageLevel::Error)
        .set_title("SecurePlan CAD")
        .set_description(message)
        .set_buttons(rfd::MessageButtons::Ok)
        .show();
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

/// Open `dir/primary.lock`. On Unix the folder and the lock must belong to
/// the current user and are made private to them (0700 and 0600), so no
/// other user can open the lock and hold it to keep the window from opening.
/// A lock that an earlier build left readable by others is replaced rather
/// than tightened: a descriptor another user opened then keeps only the old
/// file.
#[cfg(unix)]
fn open_lock(dir: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
    unsafe extern "C" {
        fn geteuid() -> u32;
    }
    // SAFETY: geteuid takes no arguments and cannot fail.
    let uid = unsafe { geteuid() };
    let foreign = || std::io::Error::new(std::io::ErrorKind::PermissionDenied, "the SecurePlan CAD settings folder is not the user's own");
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    let folder = std::fs::metadata(dir)?;
    if !folder.is_dir() || folder.uid() != uid {
        return Err(foreign());
    }
    if folder.mode() & 0o077 != 0 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let path = dir.join("primary.lock");
    let open = || std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).mode(0o600).open(&path);
    let file = open()?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.uid() != uid {
        return Err(foreign());
    }
    if metadata.mode() & 0o077 == 0 {
        return Ok(file);
    }
    drop(file);
    let fresh = dir.join("primary.lock.new");
    let _ = std::fs::remove_file(&fresh);
    std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&fresh)?;
    std::fs::rename(&fresh, &path)?;
    let file = open()?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        return Err(foreign());
    }
    Ok(file)
}

#[cfg(not(unix))]
fn open_lock(dir: &Path) -> std::io::Result<std::fs::File> {
    std::fs::create_dir_all(dir)?;
    std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(dir.join("primary.lock"))
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

/// A running hand-off listener.
#[derive(Debug)]
pub struct Serving {
    stopped: std::sync::Arc<AtomicBool>,
}

impl Serving {
    /// Refuse every hand-off from now on, including one already being
    /// authenticated.
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
    }
}

/// Serve hand-offs for this instance: every authenticated request is passed
/// to `take`, and the other launch is told it was delivered only when `take`
/// returns `true` before the listener stops.
pub fn serve(path: &Path, take: impl Fn(Request) -> bool + Send + Sync + 'static) -> std::io::Result<Serving> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let secret = random32()?;
    write_descriptor(path, listener.local_addr()?.port(), &secret)?;
    let take = std::sync::Arc::new(take);
    let stopped = std::sync::Arc::new(AtomicBool::new(false));
    let serving = Serving { stopped: stopped.clone() };
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            if stopped.load(Ordering::SeqCst) {
                continue;
            }
            let (take, stopped) = (std::sync::Arc::clone(&take), std::sync::Arc::clone(&stopped));
            std::thread::spawn(move || accept_one(stream, &secret, |request| !stopped.load(Ordering::SeqCst) && take(request)));
        }
    });
    Ok(serving)
}

/// One hand-off: prove the secret, check the client's proof, and let `take`
/// have the URL or the focus request. The client hears `ok: true` only when
/// `take` took it.
fn accept_one(stream: TcpStream, secret: &[u8; 32], take: impl FnOnce(Request) -> bool) -> Option<()> {
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
    let taken = accepted && take(request);
    write_json(&mut writer, &json!({ "ok": taken })).ok()
}

/// Whether the instance described by `path` is ready: it proves the secret.
/// Nothing is sent, so it acts on nothing.
fn ready(path: &Path) -> bool {
    authenticated(path).is_ok()
}

/// A connection whose far end has proved the secret.
struct Authenticated {
    secret: [u8; 32],
    server_nonce: [u8; 32],
    writer: TcpStream,
    reader: BufReader<TcpStream>,
}

/// Connect to the instance described by `path` and check that it knows the
/// secret, before anything is sent.
fn authenticated(path: &Path) -> Result<Authenticated, HandoffError> {
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
    Ok(Authenticated { secret, server_nonce, writer, reader })
}

/// Pass `url` to the running instance described by `path`.
pub fn send_launch(path: &Path, url: &str) -> Result<(), HandoffError> {
    send(path, &Request::Launch(url.to_string()))
}

/// Pass `request` to the running instance described by `path`.
pub fn send(path: &Path, request: &Request) -> Result<(), HandoffError> {
    let Authenticated { secret, server_nonce, mut writer, mut reader } = authenticated(path)?;
    let client_proof = proof(&secret, CLIENT_LABEL, &server_nonce).finalize().into_bytes();
    let message = match request {
        Request::Launch(url) => json!({ "op": "launch", "proof": to_hex(&client_proof), "url": url }),
        Request::Focus => json!({ "op": "focus", "proof": to_hex(&client_proof) }),
    };
    write_json(&mut writer, &message).map_err(|_| HandoffError::Io)?;
    // The answer comes once the running application has taken the request.
    reader.get_ref().set_read_timeout(Some(TAKE_WAIT + IO_TIMEOUT)).map_err(|_| HandoffError::Io)?;
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
            sender.lock().unwrap().send(request).is_ok()
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
            sender.lock().unwrap().send(request).is_ok()
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
            sender.lock().unwrap().send(request).is_ok()
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
    /// `delay` (its start-up). When `stop` is set it exits: it refuses
    /// hand-offs, withdraws its descriptor, and ends `closing` later.
    fn primary(dir: PathBuf, delay: Duration, stop: std::sync::Arc<AtomicBool>, seen: mpsc::Sender<Request>) -> std::thread::JoinHandle<bool> {
        closing_primary(dir, delay, Duration::ZERO, stop, seen, true)
    }

    fn closing_primary(
        dir: PathBuf,
        delay: Duration,
        closing: Duration,
        stop: std::sync::Arc<AtomicBool>,
        seen: mpsc::Sender<Request>,
        takes: bool,
    ) -> std::thread::JoinHandle<bool> {
        std::thread::spawn(move || {
            let Claim::Primary(lock) = claim(&dir, &[], Duration::ZERO) else { return false };
            std::thread::sleep(delay);
            let seen = std::sync::Mutex::new(seen);
            let serving = serve(&dir.join("handoff.json"), move |request| takes && seen.lock().unwrap().send(request).is_ok()).unwrap();
            while !stop.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(10));
            }
            serving.stop();
            let _ = std::fs::remove_file(dir.join("handoff.json"));
            std::thread::sleep(closing);
            drop(lock);
            true
        })
    }

    /// The launcher's standby as `main` runs one with the awaiting flag: it
    /// waits for the window, serves once it owns it, and leaves otherwise.
    /// The flag is set while it runs.
    fn standby(dir: PathBuf, seen: mpsc::Sender<Request>, starts: std::sync::Arc<std::sync::atomic::AtomicUsize>) -> std::sync::Arc<AtomicBool> {
        starts.fetch_add(1, Ordering::SeqCst);
        let running = std::sync::Arc::new(AtomicBool::new(true));
        let flag = running.clone();
        std::thread::spawn(move || {
            if let Claim::Primary(lock) = claim(&dir, &[], Duration::from_secs(10)) {
                let seen = std::sync::Mutex::new(seen);
                let serving = serve(&dir.join("handoff.json"), move |request| seen.lock().unwrap().send(request).is_ok()).unwrap();
                std::thread::sleep(Duration::from_secs(5));
                serving.stop();
                drop(lock);
            }
            flag.store(false, Ordering::SeqCst);
        });
        running
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
                            sender.lock().unwrap().send(request).is_ok()
                        })
                        .unwrap();
                        Some(lock)
                    }
                    Claim::Forwarded => None,
                    other => panic!("the primary never answered: {other:?}"),
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
        // The macOS launcher's awaiting start has nothing to send: next to a
        // ready primary it leaves (the launcher hands its link there).
        assert!(matches!(claim(&dir, &[], Duration::from_secs(10)), Claim::Forwarded));
        stop.store(true, Ordering::SeqCst);
        assert!(first.join().unwrap());
        // The primary is gone, its listener with it (its descriptor stale):
        // the next launch is the primary.
        let closed = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        write_descriptor(&dir.join("handoff.json"), closed.local_addr().unwrap().port(), &[9u8; 32]).unwrap();
        drop(closed);
        assert!(matches!(claim(&dir, &[Request::Focus], Duration::from_secs(10)), Claim::Primary(_)));
        // A primary that holds the window but never answers.
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let silent = primary(dir.clone(), Duration::from_secs(3600), stop.clone(), sender);
        std::thread::sleep(Duration::from_millis(100));
        assert!(matches!(claim(&dir, &[Request::Focus], Duration::from_millis(300)), Claim::Unanswered));
        stop.store(true, Ordering::SeqCst);
        drop(silent);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// DSK-04: a hand-off still being authenticated when the primary begins
    /// to exit is refused, never acknowledged and then lost; the next one is
    /// not served at all.
    #[test]
    fn a_hand_off_completing_after_shutdown_began_is_refused() {
        let path = temp_path("stopping");
        let (sender, receiver) = mpsc::channel();
        let sender = std::sync::Mutex::new(sender);
        let serving = serve(&path, move |request| sender.lock().unwrap().send(request).is_ok()).unwrap();
        let descriptor: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let port = descriptor["port"].as_u64().unwrap() as u16;
        let secret: [u8; 32] = from_hex(descriptor["secret"].as_str().unwrap()).unwrap();
        // The client has authenticated the primary...
        let stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
        stream.set_read_timeout(Some(IO_TIMEOUT)).unwrap();
        let mut writer = stream.try_clone().unwrap();
        let mut reader = BufReader::new(stream);
        let client_nonce = [5u8; 32];
        write_json(&mut writer, &json!({ "op": "hello", "nonce": to_hex(&client_nonce) })).unwrap();
        let answer = read_json(&mut reader).unwrap();
        let server_proof: [u8; 32] = from_hex(answer["proof"].as_str().unwrap()).unwrap();
        assert!(proof(&secret, SERVER_LABEL, &client_nonce).verify_slice(&server_proof).is_ok());
        let server_nonce: [u8; 32] = from_hex(answer["nonce"].as_str().unwrap()).unwrap();
        // ...when the primary begins to exit; then it proves itself.
        serving.stop();
        let client_proof = proof(&secret, CLIENT_LABEL, &server_nonce).finalize().into_bytes();
        write_json(&mut writer, &json!({ "op": "launch", "proof": to_hex(&client_proof), "url": URL })).unwrap();
        assert_eq!(read_json(&mut reader).unwrap()["ok"], false, "acknowledged during shutdown");
        assert!(receiver.recv_timeout(Duration::from_millis(300)).is_err(), "taken during shutdown");
        assert!(send_launch(&path, URL).is_err());
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    /// DSK-04: the macOS launcher's awaiting start next to a primary that is
    /// exiting (the window's owner, no longer ready) waits and then owns
    /// the window, instead of leaving with the link undelivered.
    #[test]
    fn an_awaiting_start_waits_for_an_exiting_primary_and_takes_over() {
        let dir = temp_dir("exiting");
        let (sender, _receiver) = mpsc::channel();
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let first = closing_primary(dir.clone(), Duration::ZERO, Duration::from_millis(500), stop.clone(), sender, true);
        while !dir.join("handoff.json").exists() {
            std::thread::sleep(Duration::from_millis(10));
        }
        stop.store(true, Ordering::SeqCst);
        while dir.join("handoff.json").exists() {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(matches!(claim(&dir, &[], Duration::from_secs(10)), Claim::Primary(_)), "the awaiting start left");
        assert!(first.join().unwrap());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// DSK-04: the macOS launcher keeps a standby running until its link is
    /// taken. Next to an exiting primary, its standby becomes the primary
    /// and gets the link. Next to a primary that is ready but does not take
    /// the link and then exits, standbys that left are started again until
    /// one owns the window.
    #[test]
    fn the_launcher_hands_its_link_over_through_shutdown_and_concurrent_starts() {
        for (case, takes) in [("exiting", true), ("refusing", false)] {
            let dir = temp_dir(&format!("standby_{case}"));
            let (sender, receiver) = mpsc::channel();
            let stop = std::sync::Arc::new(AtomicBool::new(false));
            let first = closing_primary(dir.clone(), Duration::ZERO, Duration::from_millis(400), stop.clone(), sender.clone(), takes);
            while !dir.join("handoff.json").exists() {
                std::thread::sleep(Duration::from_millis(10));
            }
            if case == "exiting" {
                stop.store(true, Ordering::SeqCst);
                while dir.join("handoff.json").exists() {
                    std::thread::sleep(Duration::from_millis(10));
                }
            } else {
                let stop = stop.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(600));
                    stop.store(true, Ordering::SeqCst);
                });
            }
            let starts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let delivered = deliver_with_standby(
                &dir.join("handoff.json"),
                vec![URL.to_string()],
                Duration::from_secs(20),
                || Some(standby(dir.clone(), sender.clone(), starts.clone())),
                |running| running.load(Ordering::SeqCst),
            );
            assert!(delivered, "{case}: the link was not delivered");
            assert_eq!(receiver.recv_timeout(Duration::from_secs(5)).unwrap(), Request::Launch(URL.into()), "{case}");
            assert!(receiver.recv_timeout(Duration::from_millis(300)).is_err(), "{case}: delivered twice");
            let starts = starts.load(Ordering::SeqCst);
            if case == "exiting" {
                assert_eq!(starts, 1, "{case}");
            } else {
                assert!(starts >= 2, "{case}: a standby that left was not replaced ({starts})");
            }
            assert!(first.join().unwrap());
            std::fs::remove_dir_all(&dir).ok();
        }
    }

    /// DSK-04: without a lock no launch starts as the primary. With a running
    /// owner it hands over; without one it is refused. A listener that
    /// cannot write its descriptor fails, so `main` does not start.
    #[test]
    fn no_launch_owns_the_window_without_the_lock_or_the_descriptor() {
        let dir = temp_dir("unlockable");
        std::fs::create_dir_all(dir.join("primary.lock")).unwrap();
        assert!(matches!(claim(&dir, &[Request::Focus], Duration::from_secs(1)), Claim::Unavailable));
        assert!(matches!(claim(&dir, &[], Duration::from_secs(1)), Claim::Unavailable));
        let (sender, receiver) = mpsc::channel();
        let sender = std::sync::Mutex::new(sender);
        let _serving = serve(&dir.join("handoff.json"), move |request| sender.lock().unwrap().send(request).is_ok()).unwrap();
        assert!(matches!(claim(&dir, &[Request::Focus], Duration::from_secs(1)), Claim::Forwarded));
        assert_eq!(receiver.recv_timeout(Duration::from_secs(5)).unwrap(), Request::Focus);
        std::fs::remove_dir_all(&dir).ok();
        let dir = temp_dir("undescribable");
        std::fs::create_dir_all(dir.join("handoff.json")).unwrap();
        assert!(serve(&dir.join("handoff.json"), |_| true).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// DSK-04: the settings folder and `primary.lock` are private to the
    /// user. A lock an earlier build left readable, already held through a
    /// descriptor opened then (as another user could), is replaced, so the
    /// launch still owns the window; a private lock is kept as it is.
    #[cfg(unix)]
    #[test]
    fn the_lock_is_private_and_a_readable_one_held_elsewhere_is_replaced() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let mode = |path: &Path| std::fs::metadata(path).unwrap().mode() & 0o777;
        let dir = temp_dir("private-lock");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = dir.join("primary.lock");
        std::fs::write(&path, b"").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let held = std::fs::File::open(&path).unwrap();
        held.try_lock().unwrap();
        let Claim::Primary(lock) = claim(&dir, &[], Duration::from_millis(200)) else { panic!("a lock held through a readable file kept the window") };
        assert_eq!((mode(&dir), mode(&path)), (0o700, 0o600));
        assert_ne!(held.metadata().unwrap().ino(), std::fs::metadata(&path).unwrap().ino());
        assert!(!dir.join("primary.lock.new").exists());
        assert!(matches!(claim(&dir, &[], Duration::from_millis(200)), Claim::Unanswered), "the new lock is not exclusive");
        let inode = lock.metadata().unwrap().ino();
        drop(lock);
        let Claim::Primary(lock) = claim(&dir, &[], Duration::from_millis(200)) else { panic!("the private lock was not taken") };
        assert_eq!(lock.metadata().unwrap().ino(), inode, "a private lock was replaced");
        drop((lock, held));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn errors_never_carry_the_url() {
        let error = send_launch(&temp_path("missing-2"), URL).unwrap_err();
        assert!(!format!("{error:?}").contains("token"));
    }
}

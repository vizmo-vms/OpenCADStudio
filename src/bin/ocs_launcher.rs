//! Receives macOS document-open callbacks before the GUI runtime starts.
//!
//! The app bundle uses this helper as its `CFBundleExecutable`. It forwards
//! Finder URLs to a running editor or starts the sibling GUI binary, then
//! stays alive so later opens keep reaching the same AppKit delegate: until
//! every GUI it started has ended and no link is still being handed over.

#[cfg(target_os = "macos")]
use std::path::PathBuf;
#[cfg(target_os = "macos")]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

#[cfg(target_os = "macos")]
use objc2::rc::Retained;
#[cfg(target_os = "macos")]
use objc2::runtime::ProtocolObject;
#[cfg(target_os = "macos")]
use objc2::{declare_class, msg_send_id, mutability, ClassType, DeclaredClass};
#[cfg(target_os = "macos")]
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate};
#[cfg(target_os = "macos")]
use objc2_foundation::{
    MainThreadMarker, NSArray, NSNotification, NSObject, NSObjectProtocol, NSURL,
};

#[cfg(target_os = "macos")]
use OpenCADStudio::io::single_instance;

/// Name of the real GUI binary inside `Contents/MacOS/`, sibling to this
/// launcher. Must match the packaging script's bundle assembly step.
#[cfg(target_os = "macos")]
const REAL_BINARY_NAME: &str = "OpenCADStudio-App";

/// How long to wait, after `applicationDidFinishLaunching:`, for an
/// `application:openURLs:` callback before concluding this particular launch
/// came with no documents (Dock icon, `open OpenCADStudio.app` with no
/// file). `application:openURLs:` arrives as part of the same startup
/// sequence when it's coming at all — essentially immediately, not after a
/// meaningful delay — so this window is slack, not a user-visible wait.
#[cfg(target_os = "macos")]
const NO_DOCUMENTS_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(400);

/// Set once `application:openURLs:` has handled a launch's documents, so the
/// delayed "no documents" fallback in `applicationDidFinishLaunching:` knows
/// to stay out of the way instead of *also* launching a bare instance.
#[cfg(target_os = "macos")]
static DOCS_HANDLED: AtomicBool = AtomicBool::new(false);

/// The GUIs this launcher started and the links it is handing over.
#[cfg(target_os = "macos")]
static LIFETIME: Lifetime = Lifetime::new();

/// Counts what keeps the launcher alive: running GUIs and links still being
/// handed over. Once a GUI has run, the launcher leaves when none is left.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
struct Lifetime(Mutex<(usize, bool)>);

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
impl Lifetime {
    const fn new() -> Self {
        Self(Mutex::new((0, false)))
    }

    /// A GUI started (`gui`) or a hand-over began.
    fn begin(&self, gui: bool) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        state.0 += 1;
        state.1 |= gui;
    }

    /// One ended; `leave` runs, with nothing else able to begin meanwhile,
    /// when that was the last and a GUI has run.
    fn end(&self, leave: impl FnOnce()) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        state.0 -= 1;
        if state.0 == 0 && state.1 {
            leave();
        }
    }
}

#[cfg(target_os = "macos")]
fn real_binary_path() -> PathBuf {
    let exe = std::env::current_exe().expect("could not resolve the launcher's own path");
    exe.parent()
        .expect("launcher executable has no parent directory")
        .join(REAL_BINARY_NAME)
}

/// Forward to a running editor or start the bundled GUI.
#[cfg(target_os = "macos")]
fn deliver_or_launch(files: &[String]) {
    let paths: Vec<PathBuf> = files.iter().map(PathBuf::from).collect();
    if let Some(stream) = single_instance::try_connect_existing() {
        if single_instance::handoff(stream, &paths) {
            reassert_accessory_policy();
            return;
        }
    }
    start_gui(files);
    reassert_accessory_policy();
}

/// Start the bundled GUI; the flag stays set while it runs.
#[cfg(target_os = "macos")]
fn start_gui(args: &[String]) -> Option<std::sync::Arc<AtomicBool>> {
    match std::process::Command::new(real_binary_path()).args(args).spawn() {
        Ok(mut child) => {
            LIFETIME.begin(true);
            let running = std::sync::Arc::new(AtomicBool::new(true));
            let flag = running.clone();
            std::thread::spawn(move || {
                let _ = child.wait();
                flag.store(false, Ordering::SeqCst);
                LIFETIME.end(|| std::process::exit(0));
            });
            Some(running)
        }
        Err(err) => {
            eprintln!("OpenCADStudio launcher: failed to launch the GUI: {err}");
            None
        }
    }
}

/// SecurePlan CAD: hand `secureplan-cad:` launch URLs to the running editor
/// over the per-user channel, never as a process argument (DSK-04). While no
/// editor takes them (none running, one starting or one exiting), a standby
/// GUI that waits windowless for them (BRG-02) is kept running, and the
/// launcher stays alive until they are delivered or the wait runs out.
#[cfg(all(target_os = "macos", feature = "secureplan"))]
fn deliver_launches(urls: Vec<String>) {
    use OpenCADStudio::app::secureplan::handoff;
    LIFETIME.begin(false);
    std::thread::spawn(move || {
        let delivered = handoff::descriptor_path().is_some_and(|path| {
            handoff::deliver_with_standby(
                &path,
                urls,
                handoff::LAUNCHER_WAIT,
                || start_gui(&[handoff::AWAITING_LAUNCH_ARG.to_string()]),
                |running| running.load(Ordering::SeqCst),
            )
        });
        if !delivered {
            eprintln!("SecurePlan CAD launcher: a SecurePlan CAD link could not be handed to the editor.");
        }
        LIFETIME.end(|| std::process::exit(0));
    });
}

/// Keep the windowless relay out of the Dock after an open event activates it.
#[cfg(target_os = "macos")]
fn reassert_accessory_policy() {
    if let Some(mtm) = MainThreadMarker::new() {
        NSApplication::sharedApplication(mtm)
            .setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    }
}

#[cfg(target_os = "macos")]
declare_class!(
    struct Delegate;

    unsafe impl ClassType for Delegate {
        type Super = NSObject;
        type Mutability = mutability::MainThreadOnly;
        const NAME: &'static str = "OCSLauncherDelegate";
    }

    impl DeclaredClass for Delegate {}

    unsafe impl NSObjectProtocol for Delegate {}

    unsafe impl NSApplicationDelegate for Delegate {
        #[method(applicationDidFinishLaunching:)]
        fn did_finish_launching(&self, _notification: &NSNotification) {
            std::thread::spawn(|| {
                std::thread::sleep(NO_DOCUMENTS_TIMEOUT);
                if !DOCS_HANDLED.swap(true, Ordering::SeqCst) {
                    deliver_or_launch(&[]);
                }
            });
        }

        #[method(application:openURLs:)]
        fn open_urls(&self, _app: &NSApplication, urls: &NSArray<NSURL>) {
            DOCS_HANDLED.store(true, Ordering::SeqCst);
            #[cfg(feature = "secureplan")]
            let (launches, urls): (Vec<String>, Vec<_>) = {
                use OpenCADStudio::app::secureplan::handoff::is_launch_url;
                let mut launches = Vec::new();
                let mut others = Vec::new();
                for url in urls.iter() {
                    match unsafe { url.absoluteString() }.map(|text| text.to_string()) {
                        Some(text) if is_launch_url(&text) => launches.push(text),
                        _ => others.push(url),
                    }
                }
                (launches, others)
            };
            let files: Vec<String> = urls
                .iter()
                .filter_map(|url| unsafe { url.path() })
                .map(|path| path.to_string())
                .collect();
            #[cfg(feature = "secureplan")]
            if !launches.is_empty() {
                deliver_launches(launches);
                if files.is_empty() {
                    return;
                }
            }
            deliver_or_launch(&files);
        }

        /// Start or activate the GUI when the bundle is reopened without files.
        #[method(applicationShouldHandleReopen:hasVisibleWindows:)]
        fn should_handle_reopen(&self, _sender: &NSApplication, has_visible_windows: bool) -> bool {
            if !has_visible_windows {
                deliver_or_launch(&[]);
            }
            true
        }
    }
);

#[cfg(target_os = "macos")]
impl Delegate {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = mtm.alloc();
        unsafe { msg_send_id![this, init] }
    }
}

#[cfg(target_os = "macos")]
fn main() {
    let mtm = MainThreadMarker::new().expect("the launcher must run on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    // No window, no Dock icon, no menu bar — this process only relays.
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    let delegate = Delegate::new(mtm);
    let proto: &ProtocolObject<dyn NSApplicationDelegate> = ProtocolObject::from_ref(&*delegate);
    app.setDelegate(Some(proto));

    // SAFETY: called once, on the main thread, immediately after `setDelegate`.
    unsafe { app.run() };
}

#[cfg(not(target_os = "macos"))]
fn main() {}

#[cfg(test)]
mod tests {
    use super::Lifetime;
    use std::cell::Cell;

    /// The launcher outlives its first GUI while a link is still being
    /// handed over, and while a standby GUI it started runs; it leaves when
    /// the last of them ends.
    #[test]
    fn the_launcher_leaves_only_when_nothing_it_started_is_left() {
        let lifetime = Lifetime::new();
        let left = Cell::new(0);
        let leave = || left.set(left.get() + 1);
        // Forwarding alone (no GUI started) never ends the relay.
        lifetime.begin(false);
        lifetime.end(leave);
        assert_eq!(left.get(), 0);
        lifetime.begin(true); // the first GUI
        lifetime.begin(false); // a link arrives while it is exiting
        lifetime.end(leave); // the first GUI ends
        assert_eq!(left.get(), 0, "left with a link still on its way");
        lifetime.begin(true); // the standby GUI
        lifetime.end(leave); // the link is delivered
        assert_eq!(left.get(), 0, "left while the standby GUI runs");
        lifetime.end(leave); // the standby GUI ends
        assert_eq!(left.get(), 1);
    }
}

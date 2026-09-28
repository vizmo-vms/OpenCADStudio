//! DSK-02 hardening: the SecurePlan build makes no network request except the
//! DSK-07 updater's requests to the GitHub Releases of
//! `vizmo-vms/OpenCADStudio` and their asset downloads.
//!
//! Every native HTTP request goes through `crate::network::agent`, which in a
//! SecurePlan build installs [`enforce_allowlist`] and follows no redirects,
//! so a request outside [`request_allowed`] fails before any connection is
//! made. Each upstream fetch is also switched off at its source (update check,
//! SHX fonts, plugins and the marketplace, feeds, remote images), and the
//! automation listeners are refused; the tests below prove each one inert.

use ureq::http::{Request, Response, Uri};
use ureq::middleware::MiddlewareNext;
use ureq::{Body, SendBody};

/// The message shown for anything the SecurePlan build refuses.
pub const DISABLED: &str = "This feature is not available in SecurePlan CAD.";

/// `(host, path prefix)` pairs the updater may request, over HTTPS only.
/// Release assets redirect from `github.com` to the `githubusercontent.com`
/// asset hosts; the updater must follow that redirect itself (the agent
/// follows none) and each hop is checked here.
const ALLOWED: &[(&str, &str)] = &[
    ("api.github.com", "/repos/vizmo-vms/OpenCADStudio/releases"),
    ("github.com", "/vizmo-vms/OpenCADStudio/releases/"),
    ("objects.githubusercontent.com", "/"),
    ("release-assets.githubusercontent.com", "/"),
];

/// Whether the SecurePlan build may request `uri`.
pub fn request_allowed(uri: &Uri) -> bool {
    if uri.scheme_str() != Some("https") || uri.port_u16().is_some_and(|port| port != 443) {
        return false;
    }
    // Credentials in the authority are refused outright.
    if uri.authority().is_some_and(|authority| authority.as_str().contains('@')) {
        return false;
    }
    let (Some(host), path) = (uri.host(), uri.path()) else {
        return false;
    };
    let path_is_plain = !path.split('/').any(|segment| segment == ".." || segment == ".");
    path_is_plain
        && ALLOWED.iter().any(|(allowed_host, prefix)| {
            host.eq_ignore_ascii_case(allowed_host) && path_within(path, prefix)
        })
}

/// `prefix` ending in `/` admits everything below it; otherwise it admits
/// itself and everything below it.
fn path_within(path: &str, prefix: &str) -> bool {
    match path.strip_prefix(prefix) {
        Some(rest) => prefix.ends_with('/') || rest.is_empty() || rest.starts_with('/'),
        None => false,
    }
}

/// ureq middleware refusing every request outside [`request_allowed`].
pub fn enforce_allowlist(
    request: Request<SendBody>,
    next: MiddlewareNext,
) -> Result<Response<Body>, ureq::Error> {
    if request_allowed(request.uri()) {
        next.handle(request)
    } else {
        Err(ureq::Error::BadUri(
            "Network access outside the SecurePlan CAD updater is not allowed.".to_string(),
        ))
    }
}

/// Refusal for `--mcp` and `--serve`, checked by `main` before anything else
/// starts.
pub fn headless_automation_refusal(mcp: bool, serve: bool) -> Option<&'static str> {
    (mcp || serve).then_some(
        "--mcp and --serve are not available in SecurePlan CAD: it runs no automation listener.",
    )
}

/// Whether the headless modes that read or write drawings (`--export`,
/// `--dwg-thumbnail`, `--script`) run: only in test and development builds,
/// for measurement. Release builds refuse them before any file access
/// (DSK-08). `secureplan-test` builds apply the release rule when
/// `SECUREPLAN_TEST_RELEASE_RULES` is set, so the refusal can be tested.
pub fn headless_files_allowed() -> bool {
    #[cfg(test)]
    if TEST_RELEASE_RULES.with(std::cell::Cell::get) {
        return false;
    }
    #[cfg(feature = "secureplan-test")]
    if std::env::var_os("SECUREPLAN_TEST_RELEASE_RULES").is_some() {
        return false;
    }
    cfg!(any(debug_assertions, feature = "secureplan-test"))
}

#[cfg(test)]
thread_local! {
    /// Unit tests apply the release rule on their own thread only.
    pub(crate) static TEST_RELEASE_RULES: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Refusal for the headless file modes, checked by `main` before anything
/// reads a file.
pub fn headless_file_refusal(export: bool, thumbnail: bool, script: bool) -> Option<&'static str> {
    ((export || thumbnail || script) && !headless_files_allowed()).then_some(
        "--export, --dwg-thumbnail and --script are not available in SecurePlan CAD: it opens drawings only for SecurePlan.",
    )
}

/// Linux (DSK-02): no core file. A crash must not write the process memory
/// (drawings, pairing secrets, session keys) to a core file in the working
/// directory or to systemd-coredump's store, which honour `RLIMIT_CORE`, so
/// the soft and hard limits are set to 0 before anything else runs. macOS
/// starts apps with this limit already at 0.
///
/// A crash handler that the system pipes core dumps to (Ubuntu's apport) is
/// not bound by the limit; see the SecurePlan CAD documentation.
/// `PR_SET_DUMPABLE` is deliberately not cleared: that also denies
/// `/proc/<pid>/root` to the user's own xdg-desktop-portal, which then
/// refuses every request from SecurePlan CAD, the file chooser included.
#[cfg(target_os = "linux")]
pub fn disable_core_dumps() -> std::io::Result<()> {
    #[repr(C)]
    struct Limit {
        current: std::ffi::c_ulong,
        maximum: std::ffi::c_ulong,
    }
    unsafe extern "C" {
        fn setrlimit(resource: std::ffi::c_int, limit: *const Limit) -> std::ffi::c_int;
    }
    /// `RLIMIT_CORE` in <sys/resource.h> on every Linux architecture.
    const RLIMIT_CORE: std::ffi::c_int = 4;
    let none = Limit { current: 0, maximum: 0 };
    // SAFETY: plain libc call with a valid pointer to a correctly laid out
    // `struct rlimit` (two `rlim_t`, which is `unsigned long` on Linux).
    if unsafe { setrlimit(RLIMIT_CORE, &none) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Process hardening for `main`, before anything else runs.
pub fn harden_process() {
    #[cfg(target_os = "linux")]
    if disable_core_dumps().is_err() {
        eprintln!("SecurePlan CAD could not turn off core dumps.");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::time::Duration;

    fn uri(value: &str) -> Uri {
        value.parse().expect("uri")
    }

    /// A local listener that records whether anything connected to it.
    struct Trap {
        listener: TcpListener,
    }

    impl Trap {
        fn new() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
            listener.set_nonblocking(true).expect("nonblocking");
            Self { listener }
        }

        fn url(&self, scheme: &str, path: &str) -> String {
            format!("{scheme}://127.0.0.1:{}{path}", self.listener.local_addr().unwrap().port())
        }

        fn was_contacted(&self) -> bool {
            std::thread::sleep(Duration::from_millis(50));
            self.listener.accept().is_ok()
        }
    }

    #[test]
    fn allowlist_admits_only_the_secureplan_release_endpoints() {
        for allowed in [
            "https://api.github.com/repos/vizmo-vms/OpenCADStudio/releases/latest",
            "https://api.github.com/repos/vizmo-vms/OpenCADStudio/releases",
            "https://github.com/vizmo-vms/OpenCADStudio/releases/download/secureplan-cad-v1.0.0/SHA256SUMS",
            "https://objects.githubusercontent.com/github-production-release-asset/1/2?x=y",
            "https://release-assets.githubusercontent.com/github-production-release-asset/1/2",
        ] {
            assert!(request_allowed(&uri(allowed)), "{allowed}");
        }
        for refused in [
            "http://api.github.com/repos/vizmo-vms/OpenCADStudio/releases/latest",
            "https://api.github.com/repos/HakanSeven12/OpenCADStudio/releases/latest",
            "https://api.github.com/repos/vizmo-vms/OpenCADStudioX/releases",
            "https://api.github.com/repos/vizmo-vms/OpenCADStudio/releases/../../other/repo",
            "https://api.github.com/graphql",
            "https://github.com/vizmo-vms/OpenCADStudio/archive/main.zip",
            "https://github.com/vizmo-vms/OpenCADStudio/releases",
            "https://raw.githubusercontent.com/HakanSeven12/OpenCADStudio/main/plugins/registry.json",
            "https://githubusercontent.com/x",
            "https://evil.objects.githubusercontent.com/x",
            "https://objects.githubusercontent.com.evil.example/x",
            "https://github.com:8443/vizmo-vms/OpenCADStudio/releases/latest",
            "https://user@github.com/vizmo-vms/OpenCADStudio/releases/latest",
            "https://www.youtube.com/feeds/videos.xml",
            "https://www.patreon.com/api/x",
            "https://127.0.0.1/x",
            "https://example.com/image.png",
        ] {
            assert!(!request_allowed(&uri(refused)), "{refused}");
        }
    }

    #[test]
    fn the_shared_agent_refuses_other_hosts_before_connecting() {
        let trap = Trap::new();
        let agent = crate::network::agent(Duration::from_secs(2));
        assert!(agent.get(&trap.url("http", "/x")).call().is_err());
        assert!(agent.get(&trap.url("https", "/x")).call().is_err());
        assert!(!trap.was_contacted(), "the agent connected to a refused host");
        assert_eq!(agent.config().max_redirects(), 0);
    }

    #[test]
    fn upstream_update_check_is_inert() {
        assert!(pollster::block_on(crate::io::update_check::check_for_update()).is_none());
    }

    #[test]
    fn shx_font_download_is_inert() {
        let fonts = vec!["romans.shx".to_string()];
        for source in [
            crate::io::font_repo::FontSource::Community,
            crate::io::font_repo::FontSource::from_url("http://127.0.0.1:9/fonts"),
        ] {
            assert_eq!(crate::io::font_repo::download_fonts(&fonts, &source), Err(DISABLED.to_string()));
        }
    }

    #[test]
    fn plugins_are_never_discovered_or_loaded() {
        assert!(crate::plugin::external::plugins_dir().is_none());
        assert!(crate::plugin::external::discover().is_empty());
    }

    #[test]
    fn plugin_marketplace_fetches_are_refused() {
        for result in [
            crate::plugin::marketplace::fetch_registry().map(|_| ()),
            crate::plugin::marketplace::fetch_releases("HakanSeven12/example").map(|_| ()),
            crate::plugin::marketplace::fetch_readme("HakanSeven12/example").map(|_| ()),
        ] {
            let error = result.expect_err("marketplace request succeeded");
            assert!(error.contains("not allowed"), "{error}");
        }
    }

    #[test]
    fn start_page_feeds_are_inert() {
        assert_eq!(crate::discussions::fetch_discussions().err().as_deref(), Some(DISABLED));
        assert_eq!(crate::videos::fetch_playlist().err().as_deref(), Some(DISABLED));
        assert_eq!(crate::patreon::fetch_patrons().err().as_deref(), Some(DISABLED));
    }

    #[test]
    fn a_drawing_with_an_http_image_makes_no_request() {
        let trap = Trap::new();
        let reference = trap.url("http", "/logo.png");
        assert!(crate::scene::model::image_model::resolve_image(&reference).is_none());
        assert!(!trap.was_contacted(), "the image reference was fetched");
    }

    /// DSK-02: on Linux a crash writes no core file, and the process stays
    /// dumpable, so the user's own xdg-desktop-portal still answers it.
    /// (Lowering the limit in the test process harms no other test.)
    #[cfg(target_os = "linux")]
    #[test]
    fn linux_turns_off_core_dumps_but_keeps_the_portal_working() {
        harden_process();
        let limits = std::fs::read_to_string("/proc/self/limits").unwrap();
        let core = limits.lines().find(|line| line.starts_with("Max core file size")).unwrap();
        let values: Vec<&str> = core.split_whitespace().skip(4).take(2).collect();
        assert_eq!(values, ["0", "0"], "{core}");
        unsafe extern "C" {
            fn prctl(option: std::ffi::c_int, ...) -> std::ffi::c_int;
        }
        const PR_GET_DUMPABLE: std::ffi::c_int = 3;
        // SAFETY: PR_GET_DUMPABLE takes no further arguments.
        assert_eq!(unsafe { prctl(PR_GET_DUMPABLE) }, 1);
    }

    #[test]
    fn headless_automation_is_refused() {
        assert!(headless_automation_refusal(true, false).is_some());
        assert!(headless_automation_refusal(false, true).is_some());
        assert!(headless_automation_refusal(false, false).is_none());
    }

    /// Release builds refuse the headless drawing modes, and `export_headless`
    /// refuses on its own, before reading the input (DSK-08).
    #[test]
    fn release_rules_refuse_the_headless_drawing_modes() {
        assert_eq!(headless_file_refusal(false, false, false), None);
        assert_eq!(headless_file_refusal(true, true, true), None, "a development build refused");
        TEST_RELEASE_RULES.with(|rule| rule.set(true));
        for (export, thumbnail, script) in [(true, false, false), (false, true, false), (false, false, true)] {
            assert!(headless_file_refusal(export, thumbnail, script).is_some_and(|r| r.contains("not available in SecurePlan CAD")));
        }
        assert_eq!(headless_file_refusal(false, false, false), None);
        let dir = std::env::temp_dir().join(format!("secureplan_export_refused_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (input, output) = (dir.join("in.dxf"), dir.join("out.dxf"));
        std::fs::write(&input, crate::app::secureplan::testutil::synthetic_dxf()).unwrap();
        // 2 is the refusal; a failed read or write would be 1.
        assert_eq!(crate::app::export_headless(&input, &output), 2);
        assert!(!output.exists());
        TEST_RELEASE_RULES.with(|rule| rule.set(false));
        assert_eq!(crate::app::export_headless(&input, &output), 0, "the development export stopped working");
        std::fs::remove_dir_all(&dir).ok();
    }
}

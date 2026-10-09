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

/// Refusal for `--mcp`, `--serve`, `--http` and `--sync-mcp-schemas`, checked
/// by `main` before anything else starts.
pub fn headless_automation_refusal(mcp: bool, serve: bool, http: bool, sync_mcp_schemas: bool) -> Option<&'static str> {
    (mcp || serve || http || sync_mcp_schemas).then_some(
        "--mcp, --serve, --http and --sync-mcp-schemas are not available in SecurePlan CAD: it runs no automation listener.",
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

    #[test]
    fn headless_automation_is_refused() {
        assert!(headless_automation_refusal(true, false, false, false).is_some());
        assert!(headless_automation_refusal(false, true, false, false).is_some());
        assert!(headless_automation_refusal(false, false, true, false).is_some());
        assert!(headless_automation_refusal(false, false, false, true).is_some());
        assert!(headless_automation_refusal(false, false, false, false).is_none());
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
        assert_eq!(crate::app::export_headless(&input, &output, None), 2);
        assert!(!output.exists());
        TEST_RELEASE_RULES.with(|rule| rule.set(false));
        assert_eq!(crate::app::export_headless(&input, &output, None), 0, "the development export stopped working");
        std::fs::remove_dir_all(&dir).ok();
    }
}

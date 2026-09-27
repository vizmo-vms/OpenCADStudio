//! SecurePlan CAD updates from the fork's GitHub Releases (DSK-07).
//!
//! - **Check**, at startup and then daily unless turned off, and on demand:
//!   `GET https://api.github.com/repos/vizmo-vms/OpenCADStudio/releases/latest`
//!   through the shared HTTP client (`crate::network::agent`: HTTPS only,
//!   the platform certificate verifier, the release allowlist, no automatic
//!   redirects). The request carries only a `User-Agent` naming the
//!   SecurePlan CAD version and an `Accept` header: no survey data,
//!   identifiers or telemetry. 403 and 429 (rate limits) mean "no update".
//! - **Offer** a release only when it is published (not a draft or a
//!   prerelease), tagged `secureplan-cad-vX.Y.Z` with a version above this
//!   build's, at least an hour old (upstream's rule, so every asset is
//!   uploaded), and lists this platform's asset and `SHA256SUMS`.
//! - **Download** `SHA256SUMS` and this platform's asset from that release,
//!   following only the release-asset redirect to the GitHub asset hosts
//!   (HTTPS, port 443, the existing allowlist); then check the size the
//!   release lists and the SHA-256 checksum. Anything else refuses the update
//!   and leaves the installed copy untouched.
//! - **Install** with the detached helper in [`super::update_helper`].
//!
//! The updater does not depend on pairing, so a build below the web's minimum
//! version (BRG-04) can still update itself.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use iced::Task;
use serde_json::Value;
use sha2_011::{Digest, Sha256};

use super::ui::{Action, Dialog};
use super::update_helper::{self, Target};
use super::{parse_version, release_tag, Msg, PROTOCOL, RELEASE_TAG_PREFIX, VERSION};
use crate::app::{Message, OpenCADStudio};

/// Where release assets are downloaded from: `<base>/<tag>/<asset name>`.
pub const DOWNLOAD_BASE: &str = "https://github.com/vizmo-vms/OpenCADStudio/releases/download";
/// The fixed, versionless release asset names (DSK-05).
pub const ASSETS: [&str; 3] = ["SecurePlanCAD-macos-arm64.dmg", "SecurePlanCAD-macos-x64.dmg", "SecurePlanCAD-windows-x64.msi"];
pub const SUMS_NAME: &str = "SHA256SUMS";
/// How often the automatic check runs after the one at startup.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
/// The asset hosts GitHub redirects release downloads to.
const ASSET_HOSTS: [&str; 2] = ["objects.githubusercontent.com", "release-assets.githubusercontent.com"];
const API_TIMEOUT: Duration = Duration::from_secs(20);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const MAX_API_BYTES: u64 = 1 << 20;
const MAX_SUMS_BYTES: u64 = 64 * 1024;
const MAX_ASSET_BYTES: u64 = 2 << 30;
const MAX_REDIRECTS: usize = 5;
/// Prefix of the per-download staging folders in the temporary directory.
const STAGING_PREFIX: &str = "secureplan-cad-update-";
const CANCELLED: &str = "The download was cancelled.";

/// This platform's release asset, if SecurePlan CAD is released for it.
pub fn platform_asset() -> Option<&'static str> {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some(ASSETS[0])
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Some(ASSETS[1])
    } else if cfg!(all(windows, target_arch = "x86_64")) {
        Some(ASSETS[2])
    } else {
        None
    }
}

/// The only identifying header: the product and its version.
fn user_agent() -> String {
    format!("SecurePlanCAD/{VERSION}")
}

// ── Endpoints ───────────────────────────────────────────────────────────────

/// The release endpoints: GitHub, or (test builds only) a local mock server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoints {
    latest: String,
    download: String,
    /// `http://127.0.0.1:<port>` of the mock server standing in for GitHub.
    #[cfg(any(test, feature = "secureplan-test"))]
    mock_origin: Option<String>,
}

impl Endpoints {
    pub fn github() -> Self {
        Self {
            latest: crate::io::update_check::RELEASES_API.to_string(),
            download: DOWNLOAD_BASE.to_string(),
            #[cfg(any(test, feature = "secureplan-test"))]
            mock_origin: None,
        }
    }

    /// Test builds only: a local mock server at `http://127.0.0.1:<port>`
    /// serving `/latest` (the release JSON) and `/download/<tag>/<name>`.
    /// Any other base is refused.
    #[cfg(any(test, feature = "secureplan-test"))]
    pub fn mock(base: &str) -> Option<Self> {
        let url = url::Url::parse(base).ok()?;
        let port = url.port()?;
        let loopback = url.scheme() == "http"
            && url.host_str() == Some("127.0.0.1")
            && url.path() == "/"
            && url.query().is_none()
            && url.fragment().is_none()
            && url.username().is_empty()
            && url.password().is_none();
        loopback.then(|| {
            let origin = format!("http://127.0.0.1:{port}");
            Self { latest: format!("{origin}/latest"), download: format!("{origin}/download"), mock_origin: Some(origin) }
        })
    }

    /// GitHub, or in a `secureplan-test` build the mock server named by
    /// `SECUREPLAN_TEST_RELEASES_BASE` (DSK-07 test override).
    pub fn current() -> Self {
        #[cfg(feature = "secureplan-test")]
        if let Some(mock) = std::env::var("SECUREPLAN_TEST_RELEASES_BASE").ok().and_then(|base| Self::mock(&base)) {
            return mock;
        }
        Self::github()
    }

    #[cfg(any(test, feature = "secureplan-test"))]
    fn mock_origin(&self) -> Option<&str> {
        self.mock_origin.as_deref()
    }

    #[cfg(not(any(test, feature = "secureplan-test")))]
    fn mock_origin(&self) -> Option<&str> {
        None
    }

    /// Whether the updater may request `url` directly.
    fn allowed(&self, url: &url::Url) -> bool {
        match self.mock_origin() {
            Some(origin) => mock_allows(origin, url.as_str()),
            None => url.as_str().parse::<ureq::http::Uri>().is_ok_and(|uri| super::hardening::request_allowed(&uri)),
        }
    }

    /// Whether a release download may be redirected to `url`: only to the
    /// GitHub asset hosts, over HTTPS on port 443, within the allowlist.
    fn redirect_allowed(&self, url: &url::Url) -> bool {
        match self.mock_origin() {
            Some(origin) => mock_allows(origin, url.as_str()),
            None => github_redirect_allowed(url),
        }
    }

    fn agent(&self, timeout: Duration) -> ureq::Agent {
        #[cfg(any(test, feature = "secureplan-test"))]
        if let Some(origin) = self.mock_origin.clone() {
            // The mock is plain HTTP on loopback; nothing else is reachable.
            let config = ureq::Agent::config_builder()
                .timeout_global(Some(timeout))
                .max_redirects(0)
                .middleware(move |request: ureq::http::Request<ureq::SendBody>, next: ureq::middleware::MiddlewareNext| {
                    if mock_allows(&origin, &request.uri().to_string()) {
                        next.handle(request)
                    } else {
                        Err(ureq::Error::BadUri("outside the mock release server".to_string()))
                    }
                });
            return config.build().into();
        }
        crate::network::agent(timeout)
    }

    /// GET `url`. Only a release download (`follow`) follows redirects, and
    /// only to the asset hosts; every other request refuses them.
    fn get(&self, agent: &ureq::Agent, url: &str, accept: &str, follow: bool) -> Result<ureq::http::Response<ureq::Body>, Fetch> {
        let mut url = url::Url::parse(url).map_err(|_| Fetch::Failed("The release address is not valid.".into()))?;
        for hop in 0..=MAX_REDIRECTS {
            let allowed = if hop == 0 { self.allowed(&url) } else { self.redirect_allowed(&url) };
            if !allowed {
                return Err(Fetch::Failed("GitHub redirected the download outside SecurePlan CAD's release hosts, so it was refused.".into()));
            }
            let response = agent
                .get(url.as_str())
                .header("User-Agent", user_agent())
                .header("Accept", accept)
                .config()
                .http_status_as_error(false)
                .build()
                .call()
                .map_err(|error| Fetch::Failed(format!("SecurePlan CAD could not reach GitHub Releases ({error}).")))?;
            match response.status().as_u16() {
                200 => return Ok(response),
                301 | 302 | 303 | 307 | 308 if follow => {
                    let location = response
                        .headers()
                        .get("location")
                        .and_then(|value| value.to_str().ok())
                        .ok_or_else(|| Fetch::Failed("GitHub sent a redirect without a location.".into()))?;
                    url = url.join(location).map_err(|_| Fetch::Failed("GitHub sent an invalid redirect.".into()))?;
                }
                301 | 302 | 303 | 307 | 308 => {
                    return Err(Fetch::Failed("GitHub redirected a request that must not be redirected, so it was refused.".into()))
                }
                403 | 429 => return Err(Fetch::Limited),
                404 => return Err(Fetch::NotFound),
                status => return Err(Fetch::Failed(format!("GitHub answered with HTTP status {status}."))),
            }
        }
        Err(Fetch::Failed("GitHub redirected the download too many times.".into()))
    }
}

/// A mock-server request: same origin, no credentials.
fn mock_allows(origin: &str, url: &str) -> bool {
    url::Url::parse(url).is_ok_and(|url| {
        url.origin().ascii_serialization() == origin && url.username().is_empty() && url.password().is_none()
    })
}

/// A GitHub release-asset redirect target: HTTPS on port 443 to an asset host,
/// with no credentials, and inside the updater allowlist.
pub fn github_redirect_allowed(url: &url::Url) -> bool {
    url.scheme() == "https"
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && url.host_str().is_some_and(|host| ASSET_HOSTS.iter().any(|allowed| host.eq_ignore_ascii_case(allowed)))
        && url.as_str().parse::<ureq::http::Uri>().is_ok_and(|uri| super::hardening::request_allowed(&uri))
}

enum Fetch {
    /// 403 or 429: GitHub's rate limit.
    Limited,
    NotFound,
    Failed(String),
}

impl Fetch {
    fn message(self) -> String {
        match self {
            Fetch::Limited => "GitHub is limiting requests right now; try again later.".into(),
            Fetch::NotFound => "The release file is missing on GitHub.".into(),
            Fetch::Failed(message) => message,
        }
    }
}

// ── Check ───────────────────────────────────────────────────────────────────

/// A release asset as the release lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    pub name: String,
    pub size: u64,
}

/// A newer release that can be installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: String,
    pub tag: String,
    /// The release notes (the GitHub release body).
    pub notes: String,
    pub published: u64,
    pub asset: Asset,
}

/// What a check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Checked {
    UpToDate,
    Available(Release),
    /// A newer release exists but is not ready yet: under an hour old, or
    /// without this platform's asset or `SHA256SUMS`.
    NotReady,
    /// GitHub limited the request (403/429): no update, silently.
    Limited,
    Failed(String),
}

/// Decide whether `release` (the releases API JSON) is an update for
/// `installed` on the platform whose asset is `asset`, at Unix time `now`.
pub fn evaluate(release: &Value, installed: &str, asset: &str, now: u64) -> Checked {
    if release["draft"].as_bool() != Some(false) || release["prerelease"].as_bool() != Some(false) {
        return Checked::UpToDate;
    }
    let Some(tag) = release["tag_name"].as_str() else { return Checked::UpToDate };
    let Some(version) = tag.strip_prefix(RELEASE_TAG_PREFIX) else { return Checked::UpToDate };
    if release_tag(version).as_deref() != Some(tag) {
        return Checked::UpToDate;
    }
    match (parse_version(version), parse_version(installed)) {
        (Some(latest), Some(current)) if latest > current => {}
        _ => return Checked::UpToDate,
    }
    let Some(published) = release["published_at"].as_str().and_then(crate::io::update_check::parse_iso8601_utc) else {
        return Checked::NotReady;
    };
    if now.saturating_sub(published) < crate::io::update_check::MIN_RELEASE_AGE_SECS {
        return Checked::NotReady;
    }
    let listed = |name: &str| {
        release["assets"].as_array().and_then(|assets| {
            assets.iter().find(|entry| entry["name"].as_str() == Some(name)).and_then(|entry| {
                let uploaded = entry.get("state").is_none_or(|state| state.as_str() == Some("uploaded"));
                entry["size"].as_u64().filter(|&size| uploaded && size > 0)
            })
        })
    };
    let (Some(size), Some(_)) = (listed(asset), listed(SUMS_NAME)) else { return Checked::NotReady };
    if size > MAX_ASSET_BYTES {
        return Checked::Failed("The release's installer is larger than SecurePlan CAD accepts.".into());
    }
    Checked::Available(Release {
        version: version.to_string(),
        tag: tag.to_string(),
        notes: release["body"].as_str().unwrap_or_default().to_string(),
        published,
        asset: Asset { name: asset.to_string(), size },
    })
}

/// Check the latest release. Blocking; run it on a worker.
pub fn check(endpoints: &Endpoints, asset: &str, now: u64) -> Checked {
    let agent = endpoints.agent(API_TIMEOUT);
    let mut response = match endpoints.get(&agent, &endpoints.latest, "application/vnd.github+json", false) {
        Ok(response) => response,
        Err(Fetch::Limited) => return Checked::Limited,
        // No release yet.
        Err(Fetch::NotFound) => return Checked::UpToDate,
        Err(Fetch::Failed(message)) => return Checked::Failed(message),
    };
    let Ok(bytes) = response.body_mut().with_config().limit(MAX_API_BYTES).read_to_vec() else {
        return Checked::Failed("GitHub's answer could not be read.".into());
    };
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(json) => evaluate(&json, VERSION, asset, now),
        Err(_) => Checked::Failed("GitHub's answer could not be read.".into()),
    }
}

pub fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or_default()
}

// ── Download and verification ───────────────────────────────────────────────

/// A downloaded and verified update, waiting to be installed.
#[derive(Clone)]
pub struct Staged {
    pub release: Release,
    /// The staging folder (owner-only), removed when the update is dropped.
    pub dir: PathBuf,
    pub file: PathBuf,
}

impl std::fmt::Debug for Staged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Staged({})", self.release.version)
    }
}

impl Staged {
    pub fn discard(&self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The SHA-256 that `SHA256SUMS` lists for `name` (lower-case hex), if it
/// lists it exactly once. Lines are `<hex>  <name>` or `<hex> *<name>`.
pub fn checksum_for(sums: &[u8], name: &str) -> Option<String> {
    let text = std::str::from_utf8(sums).ok()?;
    let mut found = None;
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        let Some((hash, rest)) = line.split_once(' ') else { continue };
        let listed = rest.strip_prefix(' ').or_else(|| rest.strip_prefix('*'));
        if listed != Some(name) {
            continue;
        }
        if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) || found.is_some() {
            return None;
        }
        found = Some(hash.to_ascii_lowercase());
    }
    found
}

fn make_staging_dir(root: &Path) -> Result<PathBuf, String> {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or_default();
    let dir = root.join(format!("{STAGING_PREFIX}{}-{nanos}", std::process::id()));
    // Owner-only on Unix; the per-user temporary folder already is on Windows.
    #[cfg(unix)]
    let builder = {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = std::fs::DirBuilder::new();
        builder.mode(0o700);
        builder
    };
    #[cfg(not(unix))]
    let builder = std::fs::DirBuilder::new();
    builder.create(&dir).map_err(|_| "SecurePlan CAD could not create a folder for the download.".to_string())?;
    Ok(dir)
}

/// Download `release`'s asset and `SHA256SUMS` into a new staging folder
/// under `root`, and verify the asset's size and SHA-256 checksum. Nothing is
/// kept on failure. Blocking; run it on a worker.
pub fn download(endpoints: &Endpoints, release: &Release, root: &Path, cancel: &AtomicBool) -> Result<Staged, String> {
    let dir = make_staging_dir(root)?;
    match download_into(endpoints, release, &dir, cancel) {
        Ok(file) => Ok(Staged { release: release.clone(), dir, file }),
        Err(message) => {
            let _ = std::fs::remove_dir_all(&dir);
            Err(message)
        }
    }
}

fn download_into(endpoints: &Endpoints, release: &Release, dir: &Path, cancel: &AtomicBool) -> Result<PathBuf, String> {
    let agent = endpoints.agent(DOWNLOAD_TIMEOUT);
    let url = |name: &str| format!("{}/{}/{}", endpoints.download, release.tag, name);
    let mut sums = endpoints.get(&agent, &url(SUMS_NAME), "application/octet-stream", true).map_err(Fetch::message)?;
    let sums = sums
        .body_mut()
        .with_config()
        .limit(MAX_SUMS_BYTES)
        .read_to_vec()
        .map_err(|_| "SHA256SUMS could not be downloaded.".to_string())?;
    let expected = checksum_for(&sums, &release.asset.name)
        .ok_or_else(|| format!("The release's SHA256SUMS does not list {} once, so the update was refused.", release.asset.name))?;
    if cancel.load(Ordering::SeqCst) {
        return Err(CANCELLED.into());
    }
    let mut response = endpoints.get(&agent, &url(&release.asset.name), "application/octet-stream", true).map_err(Fetch::message)?;
    let path = dir.join(&release.asset.name);
    let mut file = std::fs::File::create(&path).map_err(|_| "SecurePlan CAD could not save the download.".to_string())?;
    let mut hasher = Sha256::new();
    let mut total: u64 = 0;
    let mut buffer = vec![0u8; 64 * 1024];
    let mut reader = response.body_mut().as_reader();
    loop {
        if cancel.load(Ordering::SeqCst) {
            return Err(CANCELLED.into());
        }
        let read = reader.read(&mut buffer).map_err(|_| "The download stopped before it finished.".to_string())?;
        if read == 0 {
            break;
        }
        total += read as u64;
        if total > release.asset.size {
            return Err("The download is larger than the release lists, so the update was refused.".into());
        }
        hasher.update(&buffer[..read]);
        file.write_all(&buffer[..read]).map_err(|_| "SecurePlan CAD could not save the download.".to_string())?;
    }
    file.sync_all().map_err(|_| "SecurePlan CAD could not save the download.".to_string())?;
    if total != release.asset.size {
        return Err(format!("The download has {total} bytes but the release lists {}, so the update was refused.", release.asset.size));
    }
    let actual: String = hasher.finalize().iter().map(|byte| format!("{byte:02x}")).collect();
    if actual != expected {
        return Err("The download's SHA-256 checksum does not match the release's SHA256SUMS, so the update was refused.".into());
    }
    Ok(path)
}

/// Remove staging folders older than a day that an earlier run left behind.
pub fn sweep_stale(root: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else { return };
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age > CHECK_INTERVAL);
        let ours = entry.file_name().to_str().is_some_and(|name| name.starts_with(STAGING_PREFIX));
        if ours && old && entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

// ── Application state ───────────────────────────────────────────────────────

/// The updater's state in the application.
#[derive(Debug)]
pub struct Updater {
    pub endpoints: Endpoints,
    /// This platform's asset; `None` where SecurePlan CAD is not released.
    pub asset: Option<&'static str>,
    /// Where downloads are staged (the temporary directory).
    pub staging_root: PathBuf,
    pub checking: bool,
    /// A newer release found by the last check.
    pub available: Option<Release>,
    /// The version last announced on the command line.
    announced: Option<String>,
    /// While a download runs: its cancel flag.
    pub downloading: Option<Arc<AtomicBool>>,
    /// A verified download waiting for **Install and restart**.
    pub staged: Option<Staged>,
    /// The helper to start when the application exits.
    pending: Mutex<Option<update_helper::Launch>>,
    /// Unit tests stand in for the running copy's install location.
    #[cfg(test)]
    pub test_target: Option<Target>,
    /// Unit tests record the helper launches instead of starting them.
    #[cfg(test)]
    pub launched: Mutex<Vec<update_helper::Plan>>,
    /// Unit tests can hold the updater's worker jobs to interleave them.
    #[cfg(test)]
    pub held_jobs: Option<super::hooks::HeldJobs>,
}

impl Default for Updater {
    fn default() -> Self {
        Self {
            // Unit tests never reach GitHub: the discard port refuses at once.
            #[cfg(test)]
            endpoints: Endpoints::mock("http://127.0.0.1:9").expect("mock endpoints"),
            #[cfg(not(test))]
            endpoints: Endpoints::current(),
            asset: platform_asset(),
            staging_root: std::env::temp_dir(),
            checking: false,
            available: None,
            announced: None,
            downloading: None,
            staged: None,
            pending: Mutex::new(None),
            #[cfg(test)]
            test_target: None,
            #[cfg(test)]
            launched: Mutex::new(Vec::new()),
            #[cfg(test)]
            held_jobs: None,
        }
    }
}

impl Updater {
    /// Whether the automatic check (startup, then daily) should run.
    pub fn automatic(&self, settings: &super::settings::Settings) -> bool {
        self.asset.is_some() && !settings.update_checks_off
    }

    /// Whether the helper will install an update when the application exits.
    pub fn install_pending(&self) -> bool {
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).is_some()
    }
}

/// The installed build, as the dialogs show it (DSK-05).
pub fn installed_line() -> String {
    format!(
        "Installed: SecurePlan CAD {VERSION}, bridge protocol {PROTOCOL}, fork commit {}.",
        &super::FORK_COMMIT[..12.min(super::FORK_COMMIT.len())]
    )
}

/// Emits a check now and then every [`CHECK_INTERVAL`] while subscribed.
pub fn schedule() -> impl iced::futures::Stream<Item = Message> {
    iced::stream::channel(1, |output: iced::futures::channel::mpsc::Sender<Message>| async move {
        std::thread::spawn(move || {
            use iced::futures::SinkExt;
            let mut output = output;
            loop {
                if pollster::block_on(output.send(Message::SecurePlan(Msg::UpdateDue))).is_err() {
                    break;
                }
                std::thread::sleep(CHECK_INTERVAL);
            }
        });
        std::future::pending::<()>().await;
    })
}

impl OpenCADStudio {
    /// `SECUREPLANUPDATE`: check now and show what was found, or the state
    /// of an update already under way.
    pub(crate) fn secureplan_update_command(&mut self) -> Task<Message> {
        let update = &self.secureplan.update;
        if let Some(staged) = &update.staged {
            self.secureplan.dialog = Some(ready_dialog(&staged.release));
            return Task::none();
        }
        if let (Some(_), Some(release)) = (&update.downloading, &update.available) {
            self.secureplan.dialog = Some(Dialog::choice(
                "Downloading the update",
                vec![format!("SecurePlan CAD {} is downloading. You can keep working; you will be asked before it installs.", release.version)],
                vec![("Cancel download".into(), Action::UpdateCancelDownload), ("Close".into(), Action::Dismiss)],
            ));
            return Task::none();
        }
        if update.install_pending() {
            self.command_line.push_info("SecurePlan: the update installs when SecurePlan CAD closes.");
            return Task::none();
        }
        if update.asset.is_none() {
            self.secureplan.dialog = Some(Dialog::notice(
                "Updates",
                vec!["SecurePlan CAD updates itself on macOS and Windows only.".into(), installed_line()],
            ));
            return Task::none();
        }
        self.command_line.push_info("SecurePlan: checking for updates…");
        self.secureplan_check_updates(true)
    }

    /// Run a check on a worker (automatic when `manual` is false).
    pub(crate) fn secureplan_check_updates(&mut self, manual: bool) -> Task<Message> {
        let update = &mut self.secureplan.update;
        let Some(asset) = update.asset else { return Task::none() };
        if update.checking || update.downloading.is_some() || update.staged.is_some() || update.install_pending() {
            return Task::none();
        }
        update.checking = true;
        let endpoints = update.endpoints.clone();
        self.secureplan_run_update_job(
            move || Msg::UpdateChecked(manual, check(&endpoints, asset, unix_now())),
            Msg::UpdateChecked(manual, Checked::Failed("The update check stopped unexpectedly.".into())),
        )
    }

    pub(crate) fn secureplan_update_checked(&mut self, manual: bool, checked: Checked) -> Task<Message> {
        self.secureplan.update.checking = false;
        match checked {
            Checked::Available(release) => {
                let announce = self.secureplan.update.announced.as_deref() != Some(release.version.as_str());
                self.secureplan.update.available = Some(release.clone());
                if manual {
                    let quiet = format!("SecurePlan CAD {} is available. Choose Check for updates again to see what's new.", release.version);
                    let dialog = Dialog::Update(Box::new(super::ui::update_dialog::UpdateDialog::new(release)));
                    self.secureplan_update_show(dialog, &quiet);
                } else if announce {
                    // Never interrupts work: a line on the command line and a
                    // marker in the window title; the notes open on request.
                    self.command_line.push_info(&format!(
                        "SecurePlan CAD {} is available. Choose SecurePlan › Check for updates (or type SECUREPLANUPDATE) to see what's new and update.",
                        release.version
                    ));
                    self.secureplan.update.announced = Some(release.version);
                }
            }
            other if manual => {
                let line = match other {
                    Checked::UpToDate => "You have the latest SecurePlan CAD.".to_string(),
                    Checked::NotReady => "A newer SecurePlan CAD is still being published. Try again in an hour.".to_string(),
                    Checked::Limited => "GitHub is limiting update checks right now. Try again later.".to_string(),
                    Checked::Failed(message) => format!("The update check failed: {message}"),
                    Checked::Available(_) => unreachable!("handled above"),
                };
                let quiet = format!("SecurePlan: {line}");
                self.secureplan_update_show(Dialog::notice("Check for updates", vec![line, installed_line()]), &quiet);
            }
            // Automatic checks stay silent unless there is an update.
            _ => {}
        }
        Task::none()
    }

    /// **Update**: first ask about unapplied SecurePlan work, then download.
    pub(crate) fn secureplan_update_start(&mut self) -> Task<Message> {
        if self.secureplan.update.available.is_none() {
            return Task::none();
        }
        let unapplied: Vec<usize> = (0..self.tabs.len()).filter(|&index| self.secureplan_has_unapplied(index)).collect();
        let Some(&first) = unapplied.first() else { return self.secureplan_update_download() };
        let tab_id = self.tabs[first].id;
        let can_apply = self
            .secureplan
            .sessions
            .by_tab(tab_id)
            .is_some_and(|bound| bound.connected() && bound.mode == super::session::Mode::Edit);
        let mut buttons = Vec::new();
        if can_apply {
            buttons.push(("Apply first".to_string(), Action::UpdateApplyFirst(tab_id)));
        }
        buttons.push(("Keep a recovery copy and update".to_string(), Action::UpdateKeepAndStart));
        buttons.push(("Cancel".to_string(), Action::Dismiss));
        let count = unapplied.len();
        self.secureplan.dialog = Some(Dialog::choice(
            "Unapplied edits",
            vec![
                format!(
                    "{} SecurePlan drawing{} edits SecurePlan does not have yet.",
                    if count == 1 { "A".to_string() } else { count.to_string() },
                    if count == 1 { " has" } else { "s have" }
                ),
                "Apply them first, or keep a recovery copy: it is offered again when you open the survey from SecurePlan after the update.".into(),
            ],
            buttons,
        ));
        Task::none()
    }

    /// Keep every unapplied SecurePlan drawing as a recovery copy, then
    /// download.
    pub(crate) fn secureplan_update_keep_and_start(&mut self) -> Task<Message> {
        if !self.secureplan_update_keep_unapplied() {
            return Task::none();
        }
        self.secureplan_update_download()
    }

    /// Keep a recovery copy of every unapplied SecurePlan drawing; `false`
    /// (with a notice) when one could not be kept.
    fn secureplan_update_keep_unapplied(&mut self) -> bool {
        for index in 0..self.tabs.len() {
            if self.secureplan_keep_recovery(index) == super::session::Preserve::Failed {
                self.secureplan.dialog = Some(Dialog::notice(
                    "Update not started",
                    vec!["A recovery copy could not be saved, so the update did not start. Your edits are still open.".into()],
                ));
                return false;
            }
        }
        true
    }

    pub(crate) fn secureplan_update_apply_first(&mut self, tab_id: u64) -> Task<Message> {
        if let Some(index) = self.secureplan_tab_index(tab_id) {
            self.active_tab = index;
        }
        self.secureplan_begin_apply()
    }

    fn secureplan_update_download(&mut self) -> Task<Message> {
        // Refuse before downloading when this copy cannot replace itself.
        if let Err(reason) = self.secureplan_update_target() {
            self.secureplan.dialog = Some(Dialog::notice("Update not installed", vec![reason, "Nothing was changed.".into()]));
            return Task::none();
        }
        let update = &mut self.secureplan.update;
        let Some(release) = update.available.clone() else { return Task::none() };
        if update.downloading.is_some() || update.staged.is_some() {
            return Task::none();
        }
        let cancel = Arc::new(AtomicBool::new(false));
        update.downloading = Some(Arc::clone(&cancel));
        let endpoints = update.endpoints.clone();
        let root = update.staging_root.clone();
        self.command_line.push_info(&format!(
            "SecurePlan: downloading SecurePlan CAD {}. You can keep working; you will be asked before it installs.",
            release.version
        ));
        self.secureplan_run_update_job(
            move || Msg::UpdateDownloaded(download(&endpoints, &release, &root, &cancel)),
            Msg::UpdateDownloaded(Err("The download stopped unexpectedly.".into())),
        )
    }

    /// Where this copy is installed, if it can update itself (DSK-07).
    fn secureplan_update_target(&self) -> Result<Target, String> {
        #[cfg(test)]
        if let Some(target) = &self.secureplan.update.test_target {
            return Ok(target.clone());
        }
        update_helper::target()
    }

    pub(crate) fn secureplan_update_cancel_download(&mut self) {
        if let Some(cancel) = &self.secureplan.update.downloading {
            cancel.store(true, Ordering::SeqCst);
        }
    }

    pub(crate) fn secureplan_update_downloaded(&mut self, result: Result<Staged, String>) -> Task<Message> {
        let cancelled = self.secureplan.update.downloading.take().is_some_and(|cancel| cancel.load(Ordering::SeqCst));
        match result {
            Ok(staged) if cancelled => staged.discard(),
            Ok(staged) => {
                let quiet = format!(
                    "SecurePlan CAD {} is downloaded and checked. Choose SecurePlan › Check for updates to install it.",
                    staged.release.version
                );
                let dialog = ready_dialog(&staged.release);
                self.secureplan.update.staged = Some(staged);
                self.secureplan_update_show(dialog, &quiet);
            }
            Err(_) if cancelled => self.command_line.push_info("SecurePlan: the update download was cancelled."),
            Err(reason) => {
                let quiet = format!("SecurePlan: the update was not installed. {reason}");
                let dialog = Dialog::notice("Update not installed", vec![reason, "Nothing was installed; SecurePlan CAD is unchanged.".into()]);
                self.secureplan_update_show(dialog, &quiet);
            }
        }
        Task::none()
    }

    /// Show an updater result that arrived from a worker, unless the user is
    /// in another dialog: then it is only announced (the window title shows
    /// the state too), so a key meant for that dialog never acts on it.
    fn secureplan_update_show(&mut self, dialog: Dialog, quiet: &str) {
        if self.secureplan_dialog_open() || self.active_modal.is_some() {
            self.command_line.push_info(quiet);
        } else {
            self.secureplan.dialog = Some(dialog);
        }
    }

    /// **Install and restart**: keep unapplied SecurePlan work, close the
    /// SecurePlan drawings, then quit; the helper starts as the application
    /// exits, installs and opens the new version (DSK-07).
    pub(crate) fn secureplan_update_install(&mut self) -> Task<Message> {
        let Some(staged) = self.secureplan.update.staged.take() else { return Task::none() };
        let prepared = self
            .secureplan_update_target()
            .and_then(|target| update_helper::prepare(&staged, &target, update_helper::result_path()));
        let launch = match prepared {
            Ok(launch) => launch,
            Err(reason) => {
                staged.discard();
                self.secureplan.dialog = Some(Dialog::notice("Update not installed", vec![reason, "Nothing was changed.".into()]));
                return Task::none();
            }
        };
        if !self.secureplan_update_keep_unapplied() {
            self.secureplan.update.staged = Some(staged);
            return Task::none();
        }
        *self.secureplan.update.pending.lock().unwrap_or_else(|e| e.into_inner()) = Some(launch);
        self.command_line.push_info("SecurePlan: closing to install the update. If you cancel closing, it installs when SecurePlan CAD next closes.");
        // The kept drawings close as Keep would, so the quit does not ask again.
        let bound: Vec<u64> = self.secureplan.sessions.bound.iter().map(|bound| bound.tab_id).collect();
        let mut tasks: Vec<Task<Message>> = bound.into_iter().map(|tab_id| self.secureplan_close_tab(tab_id)).collect();
        tasks.push(match self.main_window {
            Some(id) => Task::done(Message::WindowCloseRequested(id)),
            None => self.exit_app(),
        });
        Task::batch(tasks)
    }

    /// Called as the application exits: start the helper for a pending
    /// update. It waits for this process to end before installing.
    pub(crate) fn secureplan_update_on_exit(&self) {
        let Some(launch) = self.secureplan.update.pending.lock().unwrap_or_else(|e| e.into_inner()).take() else {
            // A download nobody chose to install goes with this session.
            if let Some(staged) = &self.secureplan.update.staged {
                staged.discard();
            }
            return;
        };
        #[cfg(test)]
        self.secureplan.update.launched.lock().unwrap_or_else(|e| e.into_inner()).push(launch.plan.clone());
        #[cfg(not(test))]
        update_helper::launch(launch);
    }

    /// Shown in the window title: the update's state, without a dialog.
    pub(crate) fn secureplan_update_title_suffix(&self) -> &'static str {
        let update = &self.secureplan.update;
        if update.install_pending() {
            " (update installs on close)"
        } else if update.staged.is_some() {
            " (update ready to install)"
        } else if update.available.is_some() {
            " (update available)"
        } else {
            ""
        }
    }

    /// Worker jobs for the updater: tests run them at once, like other jobs;
    /// they never touch drawings, so references need not be refused.
    fn secureplan_run_update_job<F>(&mut self, work: F, failed: Msg) -> Task<Message>
    where
        F: FnOnce() -> Msg + Send + 'static,
    {
        let guarded = move || std::panic::catch_unwind(std::panic::AssertUnwindSafe(work)).unwrap_or(failed);
        #[cfg(test)]
        {
            if let Some(held) = self.secureplan.update.held_jobs.as_mut() {
                held.0.push(Box::new(guarded));
                return Task::none();
            }
            let message = guarded();
            self.secureplan_update(message)
        }
        #[cfg(not(test))]
        {
            let (sender, receiver) = iced::futures::channel::oneshot::channel();
            std::thread::spawn(move || {
                let _ = sender.send(guarded());
            });
            Task::perform(async move { receiver.await.ok() }, |message| match message {
                Some(message) => Message::SecurePlan(message),
                None => Message::Noop,
            })
        }
    }
}

/// **Install and restart** or **Later**. Focus, Enter and Escape all rest
/// on **Later**, which keeps the download; installing takes a deliberate
/// choice of the other button.
fn ready_dialog(release: &Release) -> Dialog {
    let buttons = vec![("Install and restart".to_string(), Action::UpdateInstall), ("Later".to_string(), Action::Dismiss)];
    let mut form = super::ui::Form::new(Vec::new(), buttons, Action::Dismiss);
    form.focus = 1;
    Dialog::Choice {
        title: "Update ready to install".into(),
        form,
        lines: vec![
            format!("SecurePlan CAD {} was downloaded, and its size and SHA-256 checksum match the release.", release.version),
            "Install and restart keeps a recovery copy of any unapplied SecurePlan work, closes SecurePlan CAD, installs the update and opens it again. Later keeps the download until SecurePlan CAD closes.".into(),
        ],
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::net::TcpListener;

    /// A request the mock server received.
    #[derive(Debug, Clone)]
    pub(crate) struct Recorded {
        pub target: String,
        pub headers: Vec<(String, String)>,
    }

    #[derive(Clone)]
    pub(crate) struct Reply {
        pub status: u16,
        pub headers: Vec<(String, String)>,
        pub body: Vec<u8>,
    }

    impl Reply {
        pub fn ok(body: impl Into<Vec<u8>>) -> Self {
            Self { status: 200, headers: Vec::new(), body: body.into() }
        }

        pub fn status(status: u16) -> Self {
            Self { status, headers: Vec::new(), body: Vec::new() }
        }

        pub fn redirect(location: &str) -> Self {
            Self { status: 302, headers: vec![("Location".into(), location.into())], body: Vec::new() }
        }
    }

    /// A local HTTP server standing in for GitHub Releases.
    pub(crate) struct Mock {
        pub base: String,
        pub routes: Arc<Mutex<HashMap<String, Reply>>>,
        pub requests: Arc<Mutex<Vec<Recorded>>>,
    }

    impl Mock {
        pub fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
            let routes: Arc<Mutex<HashMap<String, Reply>>> = Default::default();
            let requests: Arc<Mutex<Vec<Recorded>>> = Default::default();
            let (served_routes, served_requests) = (Arc::clone(&routes), Arc::clone(&requests));
            std::thread::spawn(move || {
                for stream in listener.incoming().flatten() {
                    let routes = Arc::clone(&served_routes);
                    let requests = Arc::clone(&served_requests);
                    std::thread::spawn(move || serve(stream, &routes, &requests));
                }
            });
            Self { base, routes, requests }
        }

        pub fn endpoints(&self) -> Endpoints {
            Endpoints::mock(&self.base).unwrap()
        }

        pub fn route(&self, path: &str, reply: Reply) {
            self.routes.lock().unwrap().insert(path.to_string(), reply);
        }

        pub fn requests(&self) -> Vec<Recorded> {
            self.requests.lock().unwrap().clone()
        }

        /// Serve a release of `version` with `asset` holding `bytes`.
        pub fn release(&self, version: &str, asset: &str, bytes: &[u8], age_secs: u64) {
            let tag = release_tag(version).unwrap();
            let sums = format!("{}  {asset}\n{}  SecurePlanCAD-source.tar.gz\n", sha256_hex(bytes), "0".repeat(64));
            self.route("/latest", Reply::ok(release_json(version, asset, bytes.len() as u64, age_secs).to_string()));
            self.route(&format!("/download/{tag}/{SUMS_NAME}"), Reply::ok(sums));
            // Assets redirect to the "asset host", as GitHub's do.
            self.route(&format!("/download/{tag}/{asset}"), Reply::redirect(&format!("/assets/{asset}?signature=x")));
            self.route(&format!("/assets/{asset}?signature=x"), Reply::ok(bytes.to_vec()));
        }
    }

    fn serve(mut stream: std::net::TcpStream, routes: &Mutex<HashMap<String, Reply>>, requests: &Mutex<Vec<Recorded>>) {
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            match stream.read(&mut byte) {
                Ok(1) => head.push(byte[0]),
                _ => return,
            }
        }
        let head = String::from_utf8_lossy(&head).to_string();
        let mut lines = head.split("\r\n");
        let target = lines.next().and_then(|line| line.split(' ').nth(1)).unwrap_or_default().to_string();
        let headers = lines
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
            .collect();
        requests.lock().unwrap().push(Recorded { target: target.clone(), headers });
        let reply = routes.lock().unwrap().get(&target).cloned().unwrap_or(Reply::status(404));
        let mut response = format!("HTTP/1.1 {} X\r\nContent-Length: {}\r\nConnection: close\r\n", reply.status, reply.body.len());
        for (name, value) in &reply.headers {
            response.push_str(&format!("{name}: {value}\r\n"));
        }
        response.push_str("\r\n");
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.write_all(&reply.body);
    }

    pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
        Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
    }

    fn iso8601(unix: u64) -> String {
        let text = crate::app::secureplan::ui::format_unix_time(unix);
        // "YYYY-MM-DD HH:MM UTC" → "YYYY-MM-DDTHH:MM:SSZ"
        format!("{}T{}:{:02}Z", &text[..10], &text[11..16], unix % 60)
    }

    pub(crate) fn release_json(version: &str, asset: &str, size: u64, age_secs: u64) -> Value {
        serde_json::json!({
            "tag_name": release_tag(version).unwrap(),
            "draft": false,
            "prerelease": false,
            "published_at": iso8601(unix_now() - age_secs),
            "body": "## What's new\n- Faster Apply",
            "assets": [
                { "name": asset, "size": size, "state": "uploaded" },
                { "name": SUMS_NAME, "size": 200, "state": "uploaded" },
            ],
        })
    }

    const ASSET: &str = "SecurePlanCAD-macos-arm64.dmg";
    const HOUR: u64 = 3600;

    #[test]
    fn only_a_published_newer_release_old_enough_with_both_assets_is_offered() {
        let now = unix_now();
        let json = |version: &str| release_json(version, ASSET, 10, 2 * HOUR);
        let Checked::Available(release) = evaluate(&json("0.2.0"), "0.1.0", ASSET, now) else { panic!("0.2.0 is newer") };
        assert_eq!((release.version.as_str(), release.tag.as_str(), release.asset.size), ("0.2.0", "secureplan-cad-v0.2.0", 10));
        assert!(release.notes.contains("Faster Apply"));
        // Same or older versions, numeric comparison (0.10.0 > 0.9.0).
        assert_eq!(evaluate(&json("0.1.0"), "0.1.0", ASSET, now), Checked::UpToDate);
        assert_eq!(evaluate(&json("0.0.9"), "0.1.0", ASSET, now), Checked::UpToDate);
        assert!(matches!(evaluate(&json("0.10.0"), "0.9.0", ASSET, now), Checked::Available(_)));
        // Drafts and prereleases are never offered.
        for flag in ["draft", "prerelease"] {
            let mut release = json("0.2.0");
            release[flag] = Value::Bool(true);
            assert_eq!(evaluate(&release, "0.1.0", ASSET, now), Checked::UpToDate, "{flag}");
        }
        // Tags outside the scheme (upstream's, a malformed version) are ignored.
        for tag in ["v2026.39", "secureplan-cad-v0.2", "secureplan-cad-v256.0.0", "secureplan-cad-v0.02.0"] {
            let mut release = json("0.2.0");
            release["tag_name"] = Value::from(tag);
            assert_eq!(evaluate(&release, "0.1.0", ASSET, now), Checked::UpToDate, "{tag}");
        }
        // Too new (under an hour), or not dated: not yet.
        assert_eq!(evaluate(&release_json("0.2.0", ASSET, 10, HOUR - 60), "0.1.0", ASSET, now), Checked::NotReady);
        let mut undated = json("0.2.0");
        undated["published_at"] = Value::Null;
        assert_eq!(evaluate(&undated, "0.1.0", ASSET, now), Checked::NotReady);
        // Without this platform's asset or SHA256SUMS, or with an empty one: not yet.
        assert_eq!(evaluate(&json("0.2.0"), "0.1.0", "SecurePlanCAD-windows-x64.msi", now), Checked::NotReady);
        let mut no_sums = json("0.2.0");
        no_sums["assets"].as_array_mut().unwrap().remove(1);
        assert_eq!(evaluate(&no_sums, "0.1.0", ASSET, now), Checked::NotReady);
        let mut uploading = json("0.2.0");
        uploading["assets"][0]["state"] = Value::from("starter");
        assert_eq!(evaluate(&uploading, "0.1.0", ASSET, now), Checked::NotReady);
    }

    #[test]
    fn checksums_are_read_per_asset_and_ambiguity_is_refused() {
        let hash = "A".repeat(64);
        let sums = format!("{hash}  {ASSET}\n{}  other.msi\r\n", "b".repeat(64));
        assert_eq!(checksum_for(sums.as_bytes(), ASSET), Some("a".repeat(64)));
        assert_eq!(checksum_for(format!("{hash} *{ASSET}\n").as_bytes(), ASSET), Some("a".repeat(64)));
        assert_eq!(checksum_for(sums.as_bytes(), "missing.dmg"), None);
        assert_eq!(checksum_for(format!("{hash}  {ASSET}\n{hash}  {ASSET}\n").as_bytes(), ASSET), None, "listed twice");
        assert_eq!(checksum_for(format!("{}  {ASSET}\n", "a".repeat(63)).as_bytes(), ASSET), None, "short hash");
        assert_eq!(checksum_for(format!("{}  {ASSET}\n", "g".repeat(64)).as_bytes(), ASSET), None, "not hex");
        assert_eq!(checksum_for(format!("{hash}  {ASSET}.bak\n").as_bytes(), ASSET), None);
    }

    #[test]
    fn a_rate_limited_or_missing_check_is_no_update() {
        let mock = Mock::start();
        for status in [403, 429] {
            mock.route("/latest", Reply::status(status));
            assert_eq!(check(&mock.endpoints(), ASSET, unix_now()), Checked::Limited, "{status}");
        }
        mock.route("/latest", Reply::status(404));
        assert_eq!(check(&mock.endpoints(), ASSET, unix_now()), Checked::UpToDate);
        mock.route("/latest", Reply::status(500));
        assert!(matches!(check(&mock.endpoints(), ASSET, unix_now()), Checked::Failed(_)));
        // The API request itself never follows a redirect.
        mock.route("/latest", Reply::redirect("/elsewhere"));
        mock.route("/elsewhere", Reply::ok(release_json("0.2.0", ASSET, 10, 2 * HOUR).to_string()));
        assert!(matches!(check(&mock.endpoints(), ASSET, unix_now()), Checked::Failed(_)));
        assert!(!mock.requests().iter().any(|r| r.target == "/elsewhere"));
    }

    #[test]
    fn requests_carry_no_survey_data_or_identifiers() {
        let mock = Mock::start();
        let bytes = b"installer bytes".to_vec();
        mock.release("0.2.0", ASSET, &bytes, 2 * HOUR);
        let Checked::Available(release) = check(&mock.endpoints(), ASSET, unix_now()) else { panic!("available") };
        let root = test_root("requests");
        download(&mock.endpoints(), &release, &root, &AtomicBool::new(false)).unwrap().discard();
        let tag = "secureplan-cad-v0.2.0";
        let expected = ["/latest".to_string(), format!("/download/{tag}/{SUMS_NAME}"), format!("/download/{tag}/{ASSET}"), format!("/assets/{ASSET}?signature=x")];
        let requests = mock.requests();
        assert_eq!(requests.iter().map(|r| r.target.clone()).collect::<Vec<_>>(), expected);
        for request in &requests {
            for (name, value) in &request.headers {
                assert!(["host", "user-agent", "accept", "accept-encoding", "connection"].contains(&name.as_str()), "unexpected header {name}: {value}");
            }
            let agent = request.headers.iter().find(|(name, _)| name == "user-agent").map(|(_, value)| value.as_str());
            assert_eq!(agent, Some(user_agent().as_str()));
        }
        std::fs::remove_dir_all(root).ok();
    }

    pub(crate) fn test_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("secureplan_update_{tag}_{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn release_for(mock: &Mock) -> Release {
        let Checked::Available(release) = check(&mock.endpoints(), ASSET, unix_now()) else { panic!("available") };
        release
    }

    fn staging_dirs(root: &Path) -> usize {
        std::fs::read_dir(root).map(|entries| entries.count()).unwrap_or(0)
    }

    #[test]
    fn a_verified_download_follows_the_asset_redirect() {
        let mock = Mock::start();
        let bytes = vec![7u8; 200_000];
        mock.release("0.2.0", ASSET, &bytes, 2 * HOUR);
        let root = test_root("verified");
        let staged = download(&mock.endpoints(), &release_for(&mock), &root, &AtomicBool::new(false)).expect("verified");
        assert_eq!(std::fs::read(&staged.file).unwrap(), bytes);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&staged.dir).unwrap().permissions().mode() & 0o777, 0o700);
        }
        staged.discard();
        assert_eq!(staging_dirs(&root), 0);
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn checksum_size_and_listing_failures_refuse_and_keep_nothing() {
        let root = test_root("refusals");
        let bytes = b"the real installer".to_vec();
        type Break = fn(&Mock, &[u8]);
        let cases: [(&str, Break); 6] = [
            ("checksum", |mock, _| mock.route(&format!("/assets/{ASSET}?signature=x"), Reply::ok(b"the fake installer".to_vec()))),
            ("longer", |mock, bytes| mock.route(&format!("/assets/{ASSET}?signature=x"), Reply::ok([bytes, b"!"].concat()))),
            ("shorter", |mock, bytes| mock.route(&format!("/assets/{ASSET}?signature=x"), Reply::ok(bytes[1..].to_vec()))),
            ("unlisted", |mock, _| mock.route(&format!("/download/secureplan-cad-v0.2.0/{SUMS_NAME}"), Reply::ok(format!("{}  other.msi\n", "a".repeat(64))))),
            ("sums missing", |mock, _| mock.route(&format!("/download/secureplan-cad-v0.2.0/{SUMS_NAME}"), Reply::status(404))),
            ("asset missing", |mock, _| mock.route(&format!("/assets/{ASSET}?signature=x"), Reply::status(404))),
        ];
        for (name, break_it) in cases {
            let mock = Mock::start();
            mock.release("0.2.0", ASSET, &bytes, 2 * HOUR);
            let release = release_for(&mock);
            break_it(&mock, &bytes);
            let error = download(&mock.endpoints(), &release, &root, &AtomicBool::new(false)).expect_err(name);
            assert!(!error.is_empty(), "{name}");
            assert_eq!(staging_dirs(&root), 0, "{name} left files behind");
        }
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn a_redirect_to_another_host_scheme_or_port_is_refused() {
        let mock = Mock::start();
        let bytes = b"installer".to_vec();
        mock.release("0.2.0", ASSET, &bytes, 2 * HOUR);
        let release = release_for(&mock);
        let port = mock.base.rsplit(':').next().unwrap().parse::<u16>().unwrap();
        let root = test_root("redirects");
        let trap = TcpListener::bind("127.0.0.1:0").unwrap();
        trap.set_nonblocking(true).unwrap();
        let other_port = trap.local_addr().unwrap().port();
        for location in [
            format!("http://127.0.0.1:{other_port}/assets/{ASSET}"),
            format!("https://127.0.0.1:{port}/assets/{ASSET}"),
            format!("http://localhost:{port}/assets/{ASSET}"),
            format!("http://user@127.0.0.1:{port}/assets/{ASSET}"),
            "https://example.com/x".to_string(),
        ] {
            mock.route(&format!("/download/{}/{ASSET}", release.tag), Reply::redirect(&location));
            let error = download(&mock.endpoints(), &release, &root, &AtomicBool::new(false)).expect_err(&location);
            assert!(error.contains("redirected"), "{location}: {error}");
        }
        std::thread::sleep(Duration::from_millis(50));
        assert!(trap.accept().is_err(), "a refused redirect was requested");
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn github_redirects_go_only_to_the_asset_hosts_over_https() {
        let parse = |value: &str| url::Url::parse(value).unwrap();
        for allowed in [
            "https://release-assets.githubusercontent.com/github-production-release-asset/1/2?sp=r&sig=x",
            "https://objects.githubusercontent.com/github-production-release-asset-2e65be/1/2",
        ] {
            assert!(github_redirect_allowed(&parse(allowed)), "{allowed}");
        }
        for refused in [
            "http://release-assets.githubusercontent.com/x",
            "https://release-assets.githubusercontent.com:8443/x",
            "https://user@objects.githubusercontent.com/x",
            "https://evil.example/x",
            "https://raw.githubusercontent.com/x",
            "https://github.com/vizmo-vms/OpenCADStudio/releases/download/secureplan-cad-v0.2.0/SHA256SUMS",
            "https://api.github.com/repos/vizmo-vms/OpenCADStudio/releases/latest",
        ] {
            assert!(!github_redirect_allowed(&parse(refused)), "{refused}");
        }
        // The first request goes only to the fork's release paths.
        let github = Endpoints::github();
        assert!(github.allowed(&parse(&format!("{DOWNLOAD_BASE}/secureplan-cad-v0.2.0/{ASSET}"))));
        assert!(github.allowed(&parse(&github.latest)));
        assert!(!github.allowed(&parse("https://github.com/other/repo/releases/download/x/y")));
        // A GitHub download's redirect hops use the asset-host rule.
        assert!(github.redirect_allowed(&parse("https://release-assets.githubusercontent.com/x")));
        assert!(!github.redirect_allowed(&parse("https://evil.example/x")));
        assert!(!github.redirect_allowed(&parse(&github.latest)));
    }

    #[test]
    fn the_test_override_accepts_only_a_loopback_origin() {
        assert!(Endpoints::mock("http://127.0.0.1:8080").is_some());
        for refused in ["https://127.0.0.1:8080", "http://localhost:8080", "http://127.0.0.1", "http://127.0.0.1:8080/x", "http://10.0.0.1:80", "https://api.github.com"] {
            assert!(Endpoints::mock(refused).is_none(), "{refused}");
        }
        assert_eq!(Endpoints::github().latest, "https://api.github.com/repos/vizmo-vms/OpenCADStudio/releases/latest");
    }

    #[test]
    fn a_cancelled_download_keeps_nothing() {
        let mock = Mock::start();
        mock.release("0.2.0", ASSET, b"bytes", 2 * HOUR);
        let root = test_root("cancel");
        let error = download(&mock.endpoints(), &release_for(&mock), &root, &AtomicBool::new(true)).unwrap_err();
        assert_eq!(error, CANCELLED);
        assert_eq!(staging_dirs(&root), 0);
        std::fs::remove_dir_all(root).ok();
    }

    // ── The flow in the application ──

    fn history(app: &OpenCADStudio) -> String {
        app.command_line.history.iter().map(|entry| entry.text.clone()).collect::<Vec<_>>().join("\n")
    }

    fn dialog(app: &OpenCADStudio) -> Option<String> {
        match &app.secureplan.dialog {
            Some(Dialog::Choice { title, .. }) => Some(title.clone()),
            Some(Dialog::Update(_)) => Some("Update available".into()),
            _ => None,
        }
    }

    fn action(app: &mut OpenCADStudio, action: Action) {
        let _ = app.update(Message::SecurePlan(Msg::Action(action)));
    }

    /// Point `app`'s updater at `mock`, staging under `root`, as if it were
    /// the installed macOS app at `root/Applications/SecurePlan CAD.app`.
    fn use_mock(app: &mut OpenCADStudio, mock: &Mock, root: &Path) {
        let update = &mut app.secureplan.update;
        update.endpoints = mock.endpoints();
        update.asset = Some(ASSET);
        update.staging_root = root.to_path_buf();
        update.test_target = Some(Target::Mac { app: root.join("Applications/SecurePlan CAD.app") });
    }

    fn launched(app: &OpenCADStudio) -> Vec<update_helper::Plan> {
        app.secureplan.update.launched.lock().unwrap().clone()
    }

    /// An unpaired copy (as one below the web's minimum version is, since
    /// the web refuses to pair with it) finds, downloads, verifies and
    /// installs an update: nothing in the updater depends on a session.
    #[test]
    fn an_unpaired_copy_updates_through_the_whole_flow() {
        let mock = Mock::start();
        let bytes = vec![3u8; 50_000];
        mock.release("0.2.0", ASSET, &bytes, 2 * HOUR);
        let root = test_root("app-flow");
        let mut app = OpenCADStudio::new_for_test();
        use_mock(&mut app, &mock, &root);
        assert!(app.secureplan.sessions.bound.is_empty());

        let _ = app.dispatch_command("SECUREPLANUPDATE");
        let Some(Dialog::Update(update)) = &app.secureplan.dialog else { panic!("the notes open on a manual check") };
        assert_eq!(update.release.version, "0.2.0");
        assert!(update.notes.iter().any(|line| line.contains("Faster Apply")));
        assert!(app.secureplan_window_title().ends_with("(update available)"));

        // Enter on the focused Update button.
        let _ = app.update(Message::SecurePlan(Msg::DialogKey(crate::app::secureplan::ui::DialogKey::Activate)));
        assert_eq!(dialog(&app).as_deref(), Some("Update ready to install"));
        let staged = app.secureplan.update.staged.clone().expect("a verified download");
        assert_eq!(std::fs::read(&staged.file).unwrap(), bytes);
        assert!(launched(&app).is_empty(), "nothing starts before Install and restart");

        action(&mut app, Action::UpdateInstall);
        // Unit tests have no window: the app exits at once, starting the helper.
        let plans = launched(&app);
        assert_eq!(plans.len(), 1, "the helper starts as the application exits");
        let plan = &plans[0];
        assert_eq!(plan.version, "0.2.0");
        assert_eq!(plan.parent_pid, std::process::id());
        assert_eq!(plan.install, update_helper::Install::Dmg { dmg: staged.file.clone(), app: root.join("Applications/SecurePlan CAD.app") });
        let written: update_helper::Plan = serde_json::from_slice(&std::fs::read(staged.dir.join("update-plan.json")).unwrap()).unwrap();
        assert_eq!(&written, plan);
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn a_refused_download_leaves_nothing_to_install() {
        let mock = Mock::start();
        mock.release("0.2.0", ASSET, b"the real installer", 2 * HOUR);
        mock.route(&format!("/assets/{ASSET}?signature=x"), Reply::ok(b"the fake installer".to_vec()));
        let root = test_root("app-refused");
        let mut app = OpenCADStudio::new_for_test();
        use_mock(&mut app, &mock, &root);
        let _ = app.dispatch_command("SECUREPLANUPDATE");
        action(&mut app, Action::UpdateStart);
        assert_eq!(dialog(&app).as_deref(), Some("Update not installed"));
        let Some(Dialog::Choice { lines, .. }) = &app.secureplan.dialog else { unreachable!() };
        assert!(lines[0].contains("SHA-256"), "{lines:?}");
        assert!(app.secureplan.update.staged.is_none() && !app.secureplan.update.install_pending());
        assert_eq!(staging_dirs(&root), 0, "the refused download was kept");
        // Quitting now starts no installer.
        let _ = app.exit_app();
        assert!(launched(&app).is_empty());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn a_copy_that_cannot_replace_itself_refuses_before_downloading() {
        let mock = Mock::start();
        mock.release("0.2.0", ASSET, b"bytes", 2 * HOUR);
        let root = test_root("app-target");
        let mut app = OpenCADStudio::new_for_test();
        use_mock(&mut app, &mock, &root);
        // Unit tests run on Linux, where SecurePlan CAD does not update itself.
        app.secureplan.update.test_target = None;
        let _ = app.dispatch_command("SECUREPLANUPDATE");
        action(&mut app, Action::UpdateStart);
        assert_eq!(dialog(&app).as_deref(), Some("Update not installed"));
        assert!(!mock.requests().iter().any(|request| request.target.starts_with("/download")), "it downloaded anyway");
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn automatic_checks_announce_once_without_a_dialog_and_stay_silent_otherwise() {
        let mock = Mock::start();
        let root = test_root("app-auto");
        let mut app = OpenCADStudio::new_for_test();
        use_mock(&mut app, &mock, &root);
        for status in [403, 429, 500] {
            mock.route("/latest", Reply::status(status));
            let before = app.command_line.history.len();
            let _ = app.update(Message::SecurePlan(Msg::UpdateDue));
            assert_eq!(app.command_line.history.len(), before, "{status} was not silent");
            assert!(app.secureplan.dialog.is_none());
        }
        mock.release("0.2.0", ASSET, b"bytes", 2 * HOUR);
        let _ = app.update(Message::SecurePlan(Msg::UpdateDue));
        let _ = app.update(Message::SecurePlan(Msg::UpdateDue));
        assert!(app.secureplan.dialog.is_none(), "an automatic check interrupted work");
        assert_eq!(history(&app).matches("SecurePlan CAD 0.2.0 is available").count(), 1);
        assert!(app.secureplan_window_title().ends_with("(update available)"));
        // The off switch stops the schedule; a manual check still works.
        let _ = app.dispatch_command("SECUREPLANAUTOUPDATE OFF");
        assert!(!app.secureplan.update.automatic(&app.secureplan.settings));
        let _ = app.dispatch_command("SECUREPLANAUTOUPDATE");
        assert!(app.secureplan.update.automatic(&app.secureplan.settings), "the ribbon button toggles");
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn update_asks_about_unapplied_work_keeps_it_and_sends_no_survey_data() {
        use crate::app::secureplan::session::tests::Harness;
        let mut h = Harness::new("update_unapplied");
        h.open_dxf();
        h.edit((0.0, 0.0), (10.0, 0.0));
        let mock = Mock::start();
        mock.release("0.2.0", ASSET, b"installer", 2 * HOUR);
        let root = test_root("app-unapplied");
        use_mock(&mut h.app, &mock, &root);

        let _ = h.app.dispatch_command("SECUREPLANUPDATE");
        action(&mut h.app, Action::UpdateStart);
        assert_eq!(dialog(&h.app).as_deref(), Some("Unapplied edits"));
        let Some(Dialog::Choice { form, .. }) = &h.app.secureplan.dialog else { unreachable!() };
        let labels: Vec<&str> = form.buttons.iter().map(|(label, _)| label.as_str()).collect();
        assert_eq!(labels, ["Apply first", "Keep a recovery copy and update", "Cancel"]);
        assert!(h.app.secureplan.update.staged.is_none(), "it downloaded before the answer");

        // Apply first opens Apply and does not download.
        let tab_id = h.tab_id();
        action(&mut h.app, Action::UpdateApplyFirst(tab_id));
        assert!(matches!(h.app.secureplan.dialog, Some(Dialog::Apply(_) | Dialog::Align(_))), "Apply did not open");
        assert!(h.app.secureplan.update.staged.is_none());
        h.app.secureplan.dialog = None;

        // Keep: a recovery copy of the edits, then the download.
        action(&mut h.app, Action::UpdateStart);
        action(&mut h.app, Action::UpdateKeepAndStart);
        let bound = h.bound().clone();
        assert!(h.app.secureplan.recovery.load(&bound.origin, bound.survey.expose()).is_some(), "no recovery copy");
        assert!(h.app.secureplan.update.staged.is_some(), "the download did not start");

        // Install and restart closes the SecurePlan drawing without asking again.
        action(&mut h.app, Action::UpdateInstall);
        assert!(h.app.secureplan.sessions.bound.is_empty(), "the SecurePlan drawing stayed open");
        assert_eq!(launched(&h.app).len(), 1);

        // Not one request named the survey, the website or the drawing.
        let survey = bound.survey.expose().to_string();
        for request in mock.requests() {
            let text = format!("{} {:?}", request.target, request.headers);
            for secret in [survey.as_str(), bound.origin.as_str(), "Synthetic survey", "secureplan.example"] {
                assert!(!text.contains(secret), "a request carried {secret}: {text}");
            }
        }
        std::fs::remove_dir_all(root).ok();
    }

    /// A download that finishes while the user is in another dialog must not
    /// replace it or take its keys: Enter meant for that form never installs.
    /// When the ready dialog does show, Enter and Escape mean Later.
    #[test]
    fn a_finished_download_never_takes_over_a_foreground_dialog() {
        use crate::app::secureplan::session::tests::Harness;
        use crate::app::secureplan::ui::DialogKey;
        let mut h = Harness::new("update_foreground");
        h.open_dxf();
        let mock = Mock::start();
        mock.release("0.2.0", ASSET, b"installer", 2 * HOUR);
        let root = test_root("app-foreground");
        use_mock(&mut h.app, &mock, &root);
        let _ = h.app.dispatch_command("SECUREPLANUPDATE");
        h.app.secureplan.update.held_jobs = Some(Default::default());
        h.key(DialogKey::Activate); // Update: the download waits on its worker
        assert!(h.app.secureplan.dialog.is_none());

        // The user opens the alignment form, then the download finishes.
        let _ = h.app.dispatch_command("SECUREPLANALIGN");
        assert!(matches!(h.app.secureplan.dialog, Some(Dialog::Align(_))), "no alignment form");
        let job = h.app.secureplan.update.held_jobs.as_mut().unwrap().0.remove(0);
        let _ = h.app.secureplan_update(job());
        assert!(matches!(h.app.secureplan.dialog, Some(Dialog::Align(_))), "the ready dialog replaced the form");
        assert!(h.app.secureplan.update.staged.is_some());
        assert!(history(&h.app).contains("0.2.0 is downloaded and checked"));
        assert!(h.app.secureplan_window_title().ends_with("(update ready to install)"));
        h.key(DialogKey::Activate); // Enter, meant for the form
        assert!(!h.app.secureplan.update.install_pending() && launched(&h.app).is_empty(), "Enter installed the update");

        // Asked for, the ready dialog rests on Later: Enter and Escape keep the download.
        h.app.secureplan.dialog = None;
        for key in [DialogKey::Activate, DialogKey::Cancel, DialogKey::Space] {
            let _ = h.app.dispatch_command("SECUREPLANUPDATE");
            assert_eq!(dialog(&h.app).as_deref(), Some("Update ready to install"));
            h.key(key);
            assert!(h.app.secureplan.dialog.is_none(), "{key:?}");
            assert!(h.app.secureplan.update.staged.is_some() && !h.app.secureplan.update.install_pending(), "{key:?} installed or dropped it");
        }
        // Installing takes choosing the other button.
        let _ = h.app.dispatch_command("SECUREPLANUPDATE");
        h.key(DialogKey::Previous);
        h.key(DialogKey::Activate);
        assert_eq!(launched(&h.app).len(), 1);
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn a_download_nobody_installs_goes_with_the_session() {
        let mock = Mock::start();
        mock.release("0.2.0", ASSET, b"installer", 2 * HOUR);
        let root = test_root("app-unused");
        let mut app = OpenCADStudio::new_for_test();
        use_mock(&mut app, &mock, &root);
        let _ = app.dispatch_command("SECUREPLANUPDATE");
        action(&mut app, Action::UpdateStart);
        let staged = app.secureplan.update.staged.clone().expect("downloaded");
        action(&mut app, Action::Dismiss); // Later
        assert!(staged.file.is_file(), "Later dropped the download");
        let _ = app.exit_app();
        assert!(!staged.dir.exists(), "the unused download stayed");
        assert!(launched(&app).is_empty());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn asset_names_match_the_release_workflow() {
        let workflow = include_str!("../../.github/workflows/secureplan-release.yml");
        for name in ASSETS.iter().chain([&SUMS_NAME]) {
            assert!(workflow.contains(name), "{name} is not built by secureplan-release.yml");
        }
    }
}

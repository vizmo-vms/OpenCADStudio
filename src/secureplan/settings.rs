//! SecurePlan settings (BRG-02): the allowed websites (trusted origins) and
//! the developer setting that makes `http://localhost` and `http://127.0.0.1`
//! origins eligible. Stored as `secureplan.json` in SecurePlan CAD's own
//! config directory, readable by the current user only.
//!
//! [`BUILT_IN_ORIGIN`] is allowed in every build without being stored: it is
//! never prompted for and cannot be removed.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::pairing::is_serialized_origin;

/// The SecurePlan website every build allows (product-owner decision,
/// 27 September 2026). Only this exact origin: another scheme, port or host
/// is an ordinary origin.
pub const BUILT_IN_ORIGIN: &str = "https://secureplan.vizmo.dev";

pub fn is_built_in(origin: &str) -> bool {
    origin == BUILT_IN_ORIGIN
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    #[serde(default)]
    pub trusted_origins: Vec<String>,
    #[serde(default)]
    pub developer_loopback_origins: bool,
    /// The automatic update check at startup and daily is off (DSK-07). A
    /// manual check still works. Absent in older files, which means on.
    #[serde(default)]
    pub update_checks_off: bool,
}

pub fn path() -> Option<PathBuf> {
    crate::config::config_dir().map(|dir| dir.join("secureplan.json"))
}

impl Settings {
    /// Load the settings; a missing or unreadable file gives the defaults.
    /// Entries that are not serialized origins are dropped.
    pub fn load() -> Self {
        path().map(|path| Self::load_from(&path)).unwrap_or_default()
    }

    pub fn load_from(path: &Path) -> Self {
        let mut settings: Settings = std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        settings.trusted_origins.retain(|origin| is_serialized_origin(origin) && !is_built_in(origin));
        settings.trusted_origins.sort();
        settings.trusted_origins.dedup();
        settings
    }

    pub fn save(&self) -> std::io::Result<()> {
        let path = path().ok_or_else(|| std::io::Error::other("no user configuration directory"))?;
        self.save_to(&path)
    }

    /// Write atomically, with owner-only permissions on Unix.
    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let temporary = path.with_extension("json.tmp");
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        {
            use std::io::Write;
            let mut file = options.open(&temporary)?;
            file.write_all(&serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?)?;
            file.sync_all()?;
        }
        std::fs::rename(&temporary, path)
    }

    /// Whether `origin` is allowed: built in, or added by the user.
    pub fn is_trusted(&self, origin: &str) -> bool {
        is_built_in(origin) || self.trusted_origins.iter().any(|trusted| trusted == origin)
    }

    /// Every allowed origin: the built-in one, then the user's.
    pub fn allowed_origins(&self) -> Vec<String> {
        std::iter::once(BUILT_IN_ORIGIN.to_string()).chain(self.trusted_origins.iter().cloned()).collect()
    }

    pub fn trust(&mut self, origin: &str) {
        if !self.is_trusted(origin) {
            self.trusted_origins.push(origin.to_string());
            self.trusted_origins.sort();
        }
    }

    /// Forget an origin the user allowed; `false` when it was not one (the
    /// built-in origin never is).
    pub fn revoke(&mut self, origin: &str) -> bool {
        let before = self.trusted_origins.len();
        self.trusted_origins.retain(|trusted| trusted != origin);
        before != self.trusted_origins.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(tag: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!("secureplan_settings_{tag}_{}", std::process::id()))
            .join("secureplan.json")
    }

    #[test]
    fn trusted_origins_persist_and_can_be_revoked() {
        let path = temp_file("persist");
        let mut settings = Settings::default();
        settings.trust("https://secureplan.example");
        settings.trust("https://secureplan.example");
        settings.developer_loopback_origins = true;
        settings.update_checks_off = true;
        settings.save_to(&path).unwrap();
        let loaded = Settings::load_from(&path);
        assert_eq!(loaded, settings);
        assert_eq!(loaded.trusted_origins, vec!["https://secureplan.example"]);

        let mut loaded = loaded;
        assert!(loaded.revoke("https://secureplan.example"));
        assert!(!loaded.revoke("https://secureplan.example"));
        loaded.save_to(&path).unwrap();
        assert!(!Settings::load_from(&path).is_trusted("https://secureplan.example"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn the_built_in_origin_is_allowed_without_being_stored_and_cannot_be_removed() {
        let path = temp_file("built_in");
        let mut settings = Settings::default();
        assert!(settings.is_trusted(BUILT_IN_ORIGIN));
        settings.trust(BUILT_IN_ORIGIN);
        assert!(settings.trusted_origins.is_empty(), "the built-in origin was stored");
        assert!(!settings.revoke(BUILT_IN_ORIGIN));
        assert!(settings.is_trusted(BUILT_IN_ORIGIN), "the built-in origin was removed");
        assert_eq!(settings.allowed_origins(), vec![BUILT_IN_ORIGIN.to_string()]);
        // Only the exact origin: other schemes, ports and hosts are ordinary.
        for look_alike in [
            "http://secureplan.vizmo.dev",
            "https://secureplan.vizmo.dev:8443",
            "https://evil-secureplan.vizmo.dev",
            "https://secureplan.vizmo.dev.example",
            "https://vizmo.dev",
        ] {
            assert!(!settings.is_trusted(look_alike), "{look_alike}");
        }
        // A file that lists it anyway keeps only the user's own origins.
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, br#"{"trustedOrigins":["https://secureplan.vizmo.dev","https://ok.example"]}"#).unwrap();
        let loaded = Settings::load_from(&path);
        assert_eq!(loaded.trusted_origins, vec!["https://ok.example"]);
        assert_eq!(loaded.allowed_origins(), vec![BUILT_IN_ORIGIN.to_string(), "https://ok.example".to_string()]);
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn damaged_files_load_as_defaults_and_bad_origins_are_dropped() {
        let path = temp_file("damaged");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{not json").unwrap();
        assert_eq!(Settings::load_from(&path), Settings::default());
        std::fs::write(&path, br#"{"trustedOrigins":["https://ok.example","https://Bad.example/","null"]}"#).unwrap();
        let loaded = Settings::load_from(&path);
        assert_eq!(loaded.trusted_origins, vec!["https://ok.example"]);
        // Files from before the update setting keep automatic checks on.
        assert!(!loaded.update_checks_off);
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }
}

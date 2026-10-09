// SecurePlan CAD's Windows version resource (DSK-08): its own name and
// version, never upstream's. `build.rs` includes this file with `include!`,
// so it holds one self-contained function.

/// The version strings for `src/secureplan/mod.rs` (its `VERSION`), and the
/// numeric `MAJOR << 48 | MINOR << 32 | PATCH << 16` version.
pub fn secureplan_win_resources(mod_rs: &str) -> Option<(Vec<(&'static str, String)>, u64)> {
    let version = mod_rs
        .lines()
        .find_map(|line| line.strip_prefix("pub const VERSION: &str = \"")?.strip_suffix("\";"))?;
    let parts: Vec<u64> = version.split('.').map(|part| part.parse().ok()).collect::<Option<_>>()?;
    let [major, minor, patch] = parts[..] else { return None };
    let strings = vec![
        ("ProductName", "SecurePlan CAD".to_string()),
        ("FileDescription", "SecurePlan CAD".to_string()),
        ("ProductVersion", version.to_string()),
        ("FileVersion", version.to_string()),
    ];
    Some((strings, major << 48 | minor << 32 | patch << 16))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_windows_resource_names_secureplan_cad_and_its_version() {
        let (strings, numeric) = secureplan_win_resources(include_str!("mod.rs")).expect("VERSION is readable");
        let value = |key: &str| strings.iter().find(|(k, _)| *k == key).map(|(_, v)| v.as_str());
        assert_eq!(value("ProductName"), Some(super::super::APP_NAME));
        assert_eq!(value("FileDescription"), Some(super::super::APP_NAME));
        assert_eq!(value("ProductVersion"), Some(super::super::VERSION));
        assert_eq!(value("FileVersion"), Some(super::super::VERSION));
        assert_eq!(numeric, 2 << 32 | 5 << 16, "0.2.5");
        assert!(strings.iter().all(|(_, v)| !v.contains("Open") && !v.contains("2026")));
    }
}

//! Redacting wrappers for SecurePlan values (DSK-02).
//!
//! Survey names, file paths, pairing data and drawing bytes are held in
//! [`Redacted`], whose `Debug` and `Display` print `[redacted]`. Logging a
//! value, deriving `Debug` on a struct that holds one, or `eprintln!("{x:?}")`
//! therefore never writes the value itself. Code that needs the value reads it
//! explicitly through [`Redacted::expose`].

use std::fmt;

const REDACTED: &str = "[redacted]";

/// A value that must never appear in logs.
#[derive(Clone, Default, PartialEq, Eq, Hash)]
pub struct Redacted<T>(T);

impl<T> Redacted<T> {
    pub fn new(value: T) -> Self {
        Self(value)
    }

    /// The wrapped value, for code that must use it (never for logging).
    pub fn expose(&self) -> &T {
        &self.0
    }

    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> From<T> for Redacted<T> {
    fn from(value: T) -> Self {
        Self(value)
    }
}

impl<T> fmt::Debug for Redacted<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED)
    }
}

impl<T> fmt::Display for Redacted<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED)
    }
}

/// A survey name or label.
pub type SurveyLabel = Redacted<String>;
/// A local file path.
pub type LocalPath = Redacted<std::path::PathBuf>;
/// Pairing data: launch URLs, pairing ids, tokens, nonces, keys.
pub type PairingSecret = Redacted<Vec<u8>>;
/// Drawing, PDF or snap-file bytes.
pub type Bytes = Redacted<Vec<u8>>;

#[cfg(test)]
mod tests {
    use super::*;

    const SURVEY: &str = "Synthetic Survey 7";
    const PATH: &str = "/tmp/synthetic-survey/plan.dwg";
    const TOKEN: &str = "c2VjdXJlcGxhbi1jYWQgc3ludGhldGljIHRva2Vu";

    #[derive(Debug)]
    #[allow(dead_code)] // fields exist to be formatted
    struct Session {
        survey: SurveyLabel,
        path: LocalPath,
        pairing: PairingSecret,
        drawing: Bytes,
        port: u16,
    }

    fn session() -> Session {
        Session {
            survey: Redacted::new(SURVEY.to_string()),
            path: Redacted::new(PATH.into()),
            pairing: Redacted::new(TOKEN.as_bytes().to_vec()),
            drawing: Redacted::new(b"AC1032 synthetic drawing bytes".to_vec()),
            port: 47815,
        }
    }

    #[test]
    fn debug_and_display_print_redacted() {
        let survey: SurveyLabel = SURVEY.to_string().into();
        assert_eq!(format!("{survey:?}"), "[redacted]");
        assert_eq!(format!("{survey}"), "[redacted]");
        assert_eq!(format!("{survey:#?}"), "[redacted]");
        assert_eq!(survey.expose(), SURVEY);
    }

    #[test]
    fn log_lines_contain_no_forbidden_values() {
        // What `log::info!("{:?}", …)`, `eprintln!("{:#?}", …)` and error
        // messages would write for a session holding every kind of value.
        let session = session();
        let lines = [
            format!("{session:?}"),
            format!("{session:#?}"),
            format!("opened {} from {}", session.survey, session.path),
            format!("pairing {:?} drawing {:?}", session.pairing, session.drawing),
        ];
        for line in &lines {
            for forbidden in [SURVEY, PATH, TOKEN, "AC1032", "synthetic drawing"] {
                assert!(!line.contains(forbidden), "{forbidden:?} leaked into {line:?}");
            }
        }
        assert!(lines[0].contains("port: 47815"), "non-sensitive fields still format");
    }
}

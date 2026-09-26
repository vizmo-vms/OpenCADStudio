//! Launch URLs and pending pairings (BRG-01, BRG-02).
//!
//! `secureplan-cad://pair?v=1&origin=…&pairing=…&token=…&survey=…&intent=…`
//! creates a pending pairing. Pending pairings are silent, capped at
//! [`MAX_PENDING`] (the oldest is dropped) and expire with their token
//! [`TOKEN_TTL`] after the launch arrived. A handshake consumes one.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use super::channel::b64url_array;
use super::redact::Redacted;

/// A pairing token is single-use and expires 120 s after the click.
pub const TOKEN_TTL: Duration = Duration::from_secs(120);
/// At most this many pairings wait for a handshake at once.
pub const MAX_PENDING: usize = 4;

const PREFIX: &str = "secureplan-cad://pair?";
const KEYS: [&str; 6] = ["v", "origin", "pairing", "token", "survey", "intent"];

/// What the web asked the desktop to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intent {
    Edit,
    View,
    Import,
    Export,
}

impl Intent {
    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "edit" => Intent::Edit,
            "view" => Intent::View,
            "import" => Intent::Import,
            "export" => Intent::Export,
            _ => return None,
        })
    }
}

/// A parsed launch URL. Only the intent is ever formatted.
#[derive(Clone, PartialEq, Eq)]
pub struct LaunchRequest {
    pub origin: String,
    pub pairing: Redacted<[u8; 16]>,
    pub token: Redacted<[u8; 32]>,
    pub survey: Redacted<String>,
    pub intent: Intent,
}

impl std::fmt::Debug for LaunchRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LaunchRequest").field("intent", &self.intent).finish_non_exhaustive()
    }
}

/// Why a launch URL was refused. Carries no URL content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchError {
    NotAPairingUrl,
    Parameters,
    Version,
    Origin,
    Pairing,
    Token,
    Survey,
    Intent,
}

/// Whether `origin` is an ASCII serialized origin exactly as a browser sends
/// it: `http` or `https`, lowercase host, no default port, path, userinfo or
/// trailing slash.
pub fn is_serialized_origin(origin: &str) -> bool {
    let Ok(url) = url::Url::parse(origin) else {
        return false;
    };
    matches!(url.scheme(), "http" | "https") && url.origin().ascii_serialization() == origin
}

fn is_uuid_v4(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => *b == b'-',
            14 => *b == b'4',
            19 => matches!(b, b'8' | b'9' | b'a' | b'b'),
            _ => b.is_ascii_digit() || (b'a'..=b'f').contains(b),
        })
}

/// Parse and validate a launch URL (BRG-01).
pub fn parse_launch_url(url: &str) -> Result<LaunchRequest, LaunchError> {
    let query = url.strip_prefix(PREFIX).ok_or(LaunchError::NotAPairingUrl)?;
    if query.contains('#') {
        return Err(LaunchError::NotAPairingUrl);
    }
    let mut values: [Option<String>; 6] = Default::default();
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        let index = KEYS.iter().position(|k| *k == key).ok_or(LaunchError::Parameters)?;
        if values[index].replace(value.into_owned()).is_some() {
            return Err(LaunchError::Parameters);
        }
    }
    let [Some(v), Some(origin), Some(pairing), Some(token), Some(survey), Some(intent)] = values
    else {
        return Err(LaunchError::Parameters);
    };
    if v != "1" {
        return Err(LaunchError::Version);
    }
    if !is_serialized_origin(&origin) {
        return Err(LaunchError::Origin);
    }
    let pairing = b64url_array::<16>(&pairing).ok_or(LaunchError::Pairing)?;
    let token = b64url_array::<32>(&token).ok_or(LaunchError::Token)?;
    if !is_uuid_v4(&survey) {
        return Err(LaunchError::Survey);
    }
    let intent = Intent::parse(&intent).ok_or(LaunchError::Intent)?;
    Ok(LaunchRequest {
        origin,
        pairing: pairing.into(),
        token: token.into(),
        survey: survey.into(),
        intent,
    })
}

#[derive(Debug)]
struct Pending {
    request: LaunchRequest,
    received: Instant,
}

/// Pairings waiting for their handshake.
#[derive(Debug, Default)]
pub struct PendingPairings {
    entries: VecDeque<Pending>,
}

impl PendingPairings {
    fn prune(&mut self, now: Instant) {
        self.entries
            .retain(|entry| now.saturating_duration_since(entry.received) < TOKEN_TTL);
    }

    /// Add a pairing silently. A repeated pairing id replaces the earlier
    /// one; beyond [`MAX_PENDING`] the oldest is dropped.
    pub fn insert(&mut self, request: LaunchRequest, now: Instant) {
        self.prune(now);
        self.entries.retain(|entry| entry.request.pairing != request.pairing);
        while self.entries.len() >= MAX_PENDING {
            self.entries.pop_front();
        }
        self.entries.push_back(Pending { request, received: now });
    }

    /// Whether any live pairing names `origin` (the WebSocket upgrade check).
    pub fn has_origin(&mut self, origin: &str, now: Instant) -> bool {
        self.prune(now);
        self.entries.iter().any(|entry| entry.request.origin == origin)
    }

    /// The live pairing with this id and origin, if any.
    pub fn find(&mut self, pairing: &[u8; 16], origin: &str, now: Instant) -> Option<LaunchRequest> {
        self.prune(now);
        self.entries
            .iter()
            .find(|entry| entry.request.pairing.expose() == pairing && entry.request.origin == origin)
            .map(|entry| entry.request.clone())
    }

    /// Remove a pairing once its handshake has succeeded, so it can never be
    /// used again.
    pub fn consume(&mut self, pairing: &[u8; 16]) {
        self.entries.retain(|entry| entry.request.pairing.expose() != pairing);
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::app::secureplan::vectors;

    pub(crate) fn launch(origin: &str, pairing: u8) -> LaunchRequest {
        LaunchRequest {
            origin: origin.to_string(),
            pairing: [pairing; 16].into(),
            token: [pairing; 32].into(),
            survey: "7d3c1f6e-2b4a-4c8d-9e0f-1a2b3c4d5e6f".to_string().into(),
            intent: Intent::Edit,
        }
    }

    #[test]
    fn launch_url_vectors() {
        let cases = vectors::json("launch-urls.json");
        for case in cases["valid"].as_array().unwrap() {
            let parsed = parse_launch_url(case["url"].as_str().unwrap())
                .unwrap_or_else(|e| panic!("{}: {e:?}", case["name"]));
            let expect = &case["expect"];
            assert_eq!(parsed.origin, expect["origin"].as_str().unwrap());
            assert_eq!(
                super::super::channel::b64url_encode(parsed.pairing.expose()),
                expect["pairing"].as_str().unwrap()
            );
            assert_eq!(
                super::super::channel::b64url_encode(parsed.token.expose()),
                expect["token"].as_str().unwrap()
            );
            assert_eq!(parsed.survey.expose(), expect["survey"].as_str().unwrap());
            assert_eq!(format!("{:?}", parsed.intent).to_lowercase(), expect["intent"].as_str().unwrap());
        }
        for case in cases["invalid"].as_array().unwrap() {
            assert!(parse_launch_url(case["url"].as_str().unwrap()).is_err(), "{}", case["name"]);
        }
    }

    #[test]
    fn origin_serialization_vectors() {
        let cases = vectors::json("launch-urls.json");
        for case in cases["origins"].as_array().unwrap() {
            let origin = case["origin"].as_str().unwrap();
            assert_eq!(is_serialized_origin(origin), case["serialized"].as_bool().unwrap(), "{origin}");
        }
    }

    #[test]
    fn launch_requests_never_format_secrets() {
        let cases = vectors::json("launch-urls.json");
        let url = cases["valid"][0]["url"].as_str().unwrap();
        let parsed = parse_launch_url(url).unwrap();
        let text = format!("{parsed:?}");
        assert!(!text.contains(cases["valid"][0]["expect"]["token"].as_str().unwrap()));
        assert!(!text.contains("secureplan.example"));
        assert!(!text.contains("7d3c1f6e"));
    }

    #[test]
    fn pending_pairings_are_capped_expire_and_are_single_use() {
        let start = Instant::now();
        let mut pending = PendingPairings::default();
        for id in 1..=5 {
            pending.insert(launch("https://secureplan.example", id), start);
        }
        assert_eq!(pending.len(), MAX_PENDING);
        assert!(pending.find(&[1; 16], "https://secureplan.example", start).is_none(), "oldest dropped");
        assert!(pending.find(&[5; 16], "https://secureplan.example", start).is_some());
        assert!(pending.find(&[5; 16], "https://other.example", start).is_none(), "origin must match");
        pending.consume(&[5; 16]);
        assert!(pending.find(&[5; 16], "https://secureplan.example", start).is_none(), "consumed");
        let later = start + TOKEN_TTL - Duration::from_millis(1);
        assert!(pending.has_origin("https://secureplan.example", later));
        assert!(!pending.has_origin("https://secureplan.example", start + TOKEN_TTL));
        assert!(pending.is_empty());
    }
}

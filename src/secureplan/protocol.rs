//! Strict BRG-07 control messages.
//!
//! Every message is checked against the pinned JSON Schemas in
//! `secureplan-vectors/schemas` (embedded at build time), then against the
//! semantic rules the schemas state in prose (JSON payload size, page area,
//! page height). Unknown types or fields are rejected. Outgoing messages are
//! checked the same way before they are sent.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};

use super::channel::b64url_array;

/// Control-message plaintext limit (BRG-06).
pub const MAX_CONTROL_BYTES: usize = 1024 * 1024;
/// Transfer size limits (BRG-06).
pub const MAX_TRANSFER_BYTES: u64 = 50 * 1024 * 1024;
pub const MAX_JSON_PAYLOAD_BYTES: u64 = 8 * 1024 * 1024;
pub const JSON_PAYLOAD_MEDIA_TYPES: [&str; 2] = [
    "application/vnd.secureplan.overlay+json",
    "application/vnd.secureplan.export+json",
];
const MAX_PAGE_AREA_PT2: f64 = 64_000_000.0;

const SCHEMA_FILES: [(&str, &str); 5] = [
    ("common.schema.json", include_str!("../../secureplan-vectors/schemas/common.schema.json")),
    ("messages.schema.json", include_str!("../../secureplan-vectors/schemas/messages.schema.json")),
    ("overlay.schema.json", include_str!("../../secureplan-vectors/schemas/overlay.schema.json")),
    ("convert-candidates.schema.json", include_str!("../../secureplan-vectors/schemas/convert-candidates.schema.json")),
    ("export-payload.schema.json", include_str!("../../secureplan-vectors/schemas/export-payload.schema.json")),
];

/// Which side sent a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    WebToDesktop,
    DesktopToWeb,
}

impl Direction {
    fn key(self) -> &'static str {
        match self {
            Direction::WebToDesktop => "webToDesktop",
            Direction::DesktopToWeb => "desktopToWeb",
        }
    }
}

/// A message that broke the protocol. Carries no message content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolError(pub String);

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "protocol error: {}", self.0)
    }
}

fn schemas() -> &'static HashMap<&'static str, Value> {
    static SCHEMAS: OnceLock<HashMap<&'static str, Value>> = OnceLock::new();
    SCHEMAS.get_or_init(|| {
        SCHEMA_FILES
            .iter()
            .map(|(name, text)| (*name, serde_json::from_str(text).expect("embedded schema is JSON")))
            .collect()
    })
}

fn pattern_matches(pattern: &str, value: &str) -> bool {
    static CACHE: OnceLock<Mutex<HashMap<String, regex::Regex>>> = OnceLock::new();
    let mut cache = CACHE.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
    let regex = cache
        .entry(pattern.to_string())
        .or_insert_with(|| regex::Regex::new(pattern).expect("schema pattern compiles"));
    regex.is_match(value)
}

fn resolve(reference: &str, file: &str) -> (&'static Value, &'static str) {
    let (target, pointer) = reference.split_once('#').unwrap_or((reference, ""));
    let wanted = if target.is_empty() { file } else { target };
    let (name, schema) = schemas().get_key_value(wanted).expect("known schema file");
    let node = if pointer.is_empty() { schema } else { schema.pointer(pointer).expect("known schema pointer") };
    (node, name)
}

/// Validate `value` against `schema` (the subset of JSON Schema the protocol
/// schemas use). Returns the first violation's path.
fn validate(value: &Value, schema: &Value, file: &str, path: &str) -> Result<(), String> {
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        let (target, target_file) = resolve(reference, file);
        return validate(value, target, target_file, path);
    }
    if let Some(options) = schema.get("oneOf").and_then(Value::as_array) {
        let matches = options.iter().filter(|option| validate(value, option, file, path).is_ok()).count();
        return if matches == 1 { Ok(()) } else { Err(format!("{path}: matched {matches} alternatives")) };
    }
    let fail = |what: &str| Err(format!("{path}: {what}"));
    if let Some(expected) = schema.get("const") {
        if value != expected {
            return fail("unexpected constant");
        }
    }
    if let Some(options) = schema.get("enum").and_then(Value::as_array) {
        if !options.contains(value) {
            return fail("not an allowed value");
        }
    }
    match schema.get("type").and_then(Value::as_str) {
        Some("object") => {
            let Some(object) = value.as_object() else { return fail("expected an object") };
            let properties = schema.get("properties").and_then(Value::as_object);
            for required in schema.get("required").and_then(Value::as_array).into_iter().flatten() {
                if !object.contains_key(required.as_str().unwrap_or_default()) {
                    return fail("missing a required field");
                }
            }
            for (key, child) in object {
                match properties.and_then(|p| p.get(key)) {
                    Some(property) => validate(child, property, file, &format!("{path}.{key}"))?,
                    None if schema.get("additionalProperties") == Some(&Value::Bool(false)) => {
                        return fail("unknown field");
                    }
                    None => {}
                }
            }
        }
        Some("array") => {
            let Some(items) = value.as_array() else { return fail("expected an array") };
            let len = items.len() as u64;
            if schema.get("minItems").and_then(Value::as_u64).is_some_and(|min| len < min) {
                return fail("too few items");
            }
            if schema.get("maxItems").and_then(Value::as_u64).is_some_and(|max| len > max) {
                return fail("too many items");
            }
            if schema.get("uniqueItems") == Some(&Value::Bool(true))
                && items.iter().enumerate().any(|(i, item)| items[..i].contains(item))
            {
                return fail("duplicate items");
            }
            if let Some(item_schema) = schema.get("items") {
                for (i, item) in items.iter().enumerate() {
                    validate(item, item_schema, file, &format!("{path}[{i}]"))?;
                }
            }
        }
        Some("string") => {
            let Some(text) = value.as_str() else { return fail("expected a string") };
            let len = text.chars().count() as u64;
            if schema.get("minLength").and_then(Value::as_u64).is_some_and(|min| len < min) {
                return fail("too short");
            }
            if schema.get("maxLength").and_then(Value::as_u64).is_some_and(|max| len > max) {
                return fail("too long");
            }
            if let Some(pattern) = schema.get("pattern").and_then(Value::as_str) {
                if !pattern_matches(pattern, text) {
                    return fail("does not match its pattern");
                }
            }
        }
        Some(kind @ ("integer" | "number")) => {
            let Some(number) = value.as_f64().filter(|n| n.is_finite()) else {
                return fail("expected a number");
            };
            if kind == "integer" && number.fract() != 0.0 {
                return fail("expected an integer");
            }
            let bound = |key: &str| schema.get(key).and_then(Value::as_f64);
            if let Some(min) = bound("minimum").filter(|min| number < *min) {
                return fail(&format!("out of range (minimum {min})"));
            }
            if let Some(max) = bound("maximum").filter(|max| number > *max) {
                return fail(&format!("out of range (maximum {max})"));
            }
            if let Some(min) = bound("exclusiveMinimum").filter(|min| number <= *min) {
                return fail(&format!("out of range (more than {min})"));
            }
        }
        Some("boolean") if !value.is_boolean() => return fail("expected a boolean"),
        Some("null") if !value.is_null() => return fail("expected null"),
        _ => {}
    }
    Ok(())
}

/// Validate a payload (`overlay.schema.json`, `export-payload.schema.json`,
/// `convert-candidates.schema.json`).
pub fn validate_payload(schema_file: &str, value: &Value) -> Result<(), ProtocolError> {
    let schema = schemas().get(schema_file).ok_or_else(|| ProtocolError("unknown payload schema".into()))?;
    validate(value, schema, schema_file, "$").map_err(ProtocolError)
}

/// Validate `value` against one node of a protocol schema (for example
/// `("common.schema.json", "/$defs/placement")`), reporting it as `path`.
pub fn validate_node(schema_file: &str, pointer: &str, value: &Value, path: &str) -> Result<(), ProtocolError> {
    let schema = schemas().get(schema_file).and_then(|schema| schema.pointer(pointer)).ok_or_else(|| ProtocolError("unknown schema node".into()))?;
    validate(value, schema, schema_file, path).map_err(ProtocolError)?;
    semantic_checks(&serde_json::json!({ path: value })).map_err(ProtocolError)
}

fn semantic_checks(message: &Value) -> Result<(), String> {
    if message["type"] == "transferStart" {
        let media = message["mediaType"].as_str().unwrap_or_default();
        let length = message["byteLength"].as_u64().unwrap_or(u64::MAX);
        if JSON_PAYLOAD_MEDIA_TYPES.contains(&media) && length > MAX_JSON_PAYLOAD_BYTES {
            return Err("JSON payload over 8 MiB".into());
        }
    }
    if let Some(placement) = message.get("placement").filter(|p| p.is_object()) {
        let number = |key: &str| placement[key].as_f64().unwrap_or(f64::NAN);
        let (w, h, width_mm, height_mm) = (number("widthPt"), number("heightPt"), number("widthMm"), number("heightMm"));
        if w * h > MAX_PAGE_AREA_PT2 {
            return Err("page area over 64,000,000 pt²".into());
        }
        let expected = h * width_mm / w;
        // Written so that NaN counts as inconsistent.
        let consistent = (height_mm - expected).abs() <= 1e-6 * expected.abs();
        if !consistent {
            return Err("page height inconsistent with its width".into());
        }
    }
    Ok(())
}

/// Validate a control message travelling in `direction`: its type must be
/// allowed in that direction and the message must satisfy its schema and the
/// semantic rules.
pub fn validate_message(direction: Direction, message: &Value) -> Result<(), ProtocolError> {
    let messages = &schemas()["messages.schema.json"];
    let kind = message.get("type").and_then(Value::as_str).unwrap_or_default();
    let allowed = messages["x-directions"][direction.key()]
        .as_array()
        .is_some_and(|types| types.iter().any(|t| t == kind));
    if !message.is_object() || !allowed {
        return Err(ProtocolError(format!("unknown {} message type", direction.key())));
    }
    validate(message, &messages["$defs"][kind], "messages.schema.json", kind).map_err(ProtocolError)?;
    semantic_checks(message).map_err(ProtocolError)
}

/// `hello`, the web's first frame.
#[derive(Clone, PartialEq, Eq)]
pub struct Hello {
    pub pairing: [u8; 16],
    pub nonce_w: [u8; 32],
    pub protocols: Vec<u64>,
    pub min_desktop_version: String,
}

impl std::fmt::Debug for Hello {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hello").field("protocols", &self.protocols).finish_non_exhaustive()
    }
}

/// An announced transfer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferStart {
    pub transfer_id: u32,
    pub name: super::redact::Redacted<String>,
    pub media_type: String,
    pub byte_length: u64,
    pub sha256: [u8; 32],
}

/// A validated web-to-desktop message after the handshake.
#[derive(Clone)]
pub enum Inbound {
    Close { reason: String },
    TransferStart(TransferStart),
    /// Any other session message (`openSession`, `planUpdate`, …), already
    /// validated; later tasks give each its own type.
    Session { kind: String, request_id: String, body: Value },
}

impl std::fmt::Debug for Inbound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Inbound::Close { reason } => f.debug_struct("Close").field("reason", reason).finish(),
            Inbound::TransferStart(start) => f.debug_tuple("TransferStart").field(&start.transfer_id).finish(),
            Inbound::Session { kind, .. } => f.debug_struct("Session").field("kind", kind).finish_non_exhaustive(),
        }
    }
}

fn parse_json(text: &str) -> Result<Value, ProtocolError> {
    if text.len() > MAX_CONTROL_BYTES {
        return Err(ProtocolError("control message over 1 MiB".into()));
    }
    serde_json::from_str(text).map_err(|_| ProtocolError("control message is not JSON".into()))
}

fn hex32(text: &str) -> Option<[u8; 32]> {
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(text.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

/// Parse the plaintext `hello`.
pub fn parse_hello(text: &str) -> Result<Hello, ProtocolError> {
    let value = parse_json(text)?;
    if value["type"] != "hello" {
        return Err(ProtocolError("expected hello".into()));
    }
    validate_message(Direction::WebToDesktop, &value)?;
    Ok(Hello {
        pairing: b64url_array(value["pairing"].as_str().unwrap_or_default())
            .ok_or_else(|| ProtocolError("bad pairing id".into()))?,
        nonce_w: b64url_array(value["nonceW"].as_str().unwrap_or_default())
            .ok_or_else(|| ProtocolError("bad nonce".into()))?,
        protocols: value["protocols"].as_array().into_iter().flatten().filter_map(Value::as_u64).collect(),
        min_desktop_version: value["minDesktopVersion"].as_str().unwrap_or_default().to_string(),
    })
}

/// Parse the plaintext `prove`; returns `proofW`.
pub fn parse_prove(text: &str) -> Result<[u8; 32], ProtocolError> {
    let value = parse_json(text)?;
    if value["type"] != "prove" {
        return Err(ProtocolError("expected prove".into()));
    }
    validate_message(Direction::WebToDesktop, &value)?;
    b64url_array(value["proofW"].as_str().unwrap_or_default()).ok_or_else(|| ProtocolError("bad proof".into()))
}

/// Parse a sealed web-to-desktop control message.
pub fn parse_inbound(text: &str) -> Result<Inbound, ProtocolError> {
    let value = parse_json(text)?;
    let kind = value["type"].as_str().unwrap_or_default().to_string();
    if matches!(kind.as_str(), "hello" | "prove") {
        return Err(ProtocolError("handshake message after the handshake".into()));
    }
    validate_message(Direction::WebToDesktop, &value)?;
    Ok(match kind.as_str() {
        "close" => Inbound::Close { reason: value["reason"].as_str().unwrap_or_default().to_string() },
        "transferStart" => Inbound::TransferStart(TransferStart {
            transfer_id: value["transferId"].as_u64().and_then(|id| u32::try_from(id).ok()).unwrap_or_default(),
            name: value["name"].as_str().unwrap_or_default().to_string().into(),
            media_type: value["mediaType"].as_str().unwrap_or_default().to_string(),
            byte_length: value["byteLength"].as_u64().unwrap_or_default(),
            sha256: hex32(value["sha256"].as_str().unwrap_or_default())
                .ok_or_else(|| ProtocolError("bad sha256".into()))?,
        }),
        _ => Inbound::Session {
            request_id: value["requestId"].as_str().unwrap_or_default().to_string(),
            kind,
            body: value,
        },
    })
}

/// Serialise an outgoing desktop-to-web message after checking it.
pub fn encode_outbound(message: &Value) -> Result<String, ProtocolError> {
    validate_message(Direction::DesktopToWeb, message)?;
    let text = message.to_string();
    if text.len() > MAX_CONTROL_BYTES {
        return Err(ProtocolError("control message over 1 MiB".into()));
    }
    Ok(text)
}

pub fn challenge(request_id: &str, nonce_d: &[u8; 32], proof_d: &[u8; 32]) -> Value {
    use super::channel::b64url_encode;
    json!({ "type": "challenge", "requestId": request_id, "nonceD": b64url_encode(nonce_d), "proofD": b64url_encode(proof_d) })
}

pub fn welcome(request_id: &str) -> Value {
    json!({
        "type": "welcome",
        "requestId": request_id,
        "desktopVersion": super::VERSION,
        "protocol": super::PROTOCOL,
        "capabilities": ["overlay", "import", "apply", "layoutView", "convert", "export"],
    })
}

pub fn ping(request_id: &str) -> Value {
    json!({ "type": "ping", "requestId": request_id })
}

pub fn close(request_id: &str, reason: &str) -> Value {
    json!({ "type": "close", "requestId": request_id, "reason": reason })
}

pub fn transfer_start(request_id: &str, transfer_id: u32, name: &str, media_type: &str, bytes: &[u8]) -> Value {
    use sha2_011::{Digest, Sha256};
    let sha256: String = Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect();
    json!({
        "type": "transferStart",
        "requestId": request_id,
        "transferId": transfer_id,
        "name": name,
        "mediaType": media_type,
        "byteLength": bytes.len(),
        "sha256": sha256,
    })
}

/// `sessionState`: dirty flag, active view, busy state and last error.
pub fn session_state(request_id: &str, dirty: bool, active_view: Value, busy: Value, error: Value) -> Value {
    json!({
        "type": "sessionState",
        "requestId": request_id,
        "dirty": dirty,
        "activeView": active_view,
        "busy": busy,
        "error": error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::secureplan::vectors;

    fn direction(name: &str) -> Direction {
        if name == "webToDesktop" { Direction::WebToDesktop } else { Direction::DesktopToWeb }
    }

    #[test]
    fn welcome_offers_conversion_and_export() {
        let welcome = welcome("w1");
        validate_message(Direction::DesktopToWeb, &welcome).unwrap();
        let capabilities: Vec<&str> = welcome["capabilities"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
        assert_eq!(capabilities, ["overlay", "import", "apply", "layoutView", "convert", "export"]);
    }

    #[test]
    fn valid_message_samples_pass() {
        for sample in vectors::json("messages/valid.json")["samples"].as_array().unwrap() {
            let dir = direction(sample["direction"].as_str().unwrap());
            validate_message(dir, &sample["message"]).unwrap_or_else(|e| panic!("{}: {e}", sample["name"]));
        }
    }

    #[test]
    fn invalid_message_samples_fail() {
        for sample in vectors::json("messages/invalid.json")["samples"].as_array().unwrap() {
            let dir = direction(sample["direction"].as_str().unwrap());
            assert!(validate_message(dir, &sample["message"]).is_err(), "{}", sample["name"]);
        }
    }

    #[test]
    fn messages_are_refused_in_the_wrong_direction() {
        let ping = ping("g1");
        assert!(validate_message(Direction::DesktopToWeb, &ping).is_ok());
        assert!(validate_message(Direction::WebToDesktop, &ping).is_err());
    }

    #[test]
    fn payload_samples_follow_their_schemas() {
        for sample in vectors::json("payloads/valid.json")["samples"].as_array().unwrap() {
            validate_payload(sample["schema"].as_str().unwrap(), &sample["value"])
                .unwrap_or_else(|e| panic!("{}: {e}", sample["name"]));
        }
        for sample in vectors::json("payloads/invalid.json")["samples"].as_array().unwrap() {
            assert!(validate_payload(sample["schema"].as_str().unwrap(), &sample["value"]).is_err(), "{}", sample["name"]);
        }
    }

    #[test]
    fn built_messages_follow_the_schemas() {
        let nonce = [9u8; 32];
        for message in [
            challenge("c1", &nonce, &nonce),
            welcome("w1"),
            ping("g1"),
            close("x1", "superseded"),
            transfer_start("t1", 7, "synthetic.pdf", "application/pdf", b"%PDF-1.7"),
            session_state("s1", true, json!({ "kind": "model" }), json!(null), json!(null)),
        ] {
            encode_outbound(&message).unwrap_or_else(|e| panic!("{message}: {e}"));
        }
        assert!(encode_outbound(&close("x1", "because")).is_err());
    }

    #[test]
    fn handshake_messages_parse() {
        let handshake = vectors::json("handshake.json");
        let hello = parse_hello(&handshake["steps"][0]["message"].to_string()).unwrap();
        assert_eq!(hello.protocols, vec![1]);
        assert!(!format!("{hello:?}").contains(handshake["inputs"]["pairing"].as_str().unwrap()));
        parse_prove(&handshake["steps"][2]["message"].to_string()).unwrap();
        assert!(parse_hello(&handshake["steps"][2]["message"].to_string()).is_err());
        assert!(parse_inbound(&handshake["steps"][0]["message"].to_string()).is_err());
    }

    #[test]
    fn inbound_messages_parse_and_never_format_content() {
        let samples = vectors::json("messages/valid.json");
        let open = samples["samples"].as_array().unwrap().iter().find(|s| s["name"] == "openSession-edit").unwrap();
        let inbound = parse_inbound(&open["message"].to_string()).unwrap();
        assert!(matches!(&inbound, Inbound::Session { kind, .. } if kind == "openSession"));
        assert!(!format!("{inbound:?}").contains("Synthetic"));
        let start = samples["samples"].as_array().unwrap().iter().find(|s| s["name"] == "transferStart-drawing").unwrap();
        let Inbound::TransferStart(start) = parse_inbound(&start["message"].to_string()).unwrap() else {
            panic!("expected transferStart");
        };
        assert_eq!(start.transfer_id, 7);
        assert!(parse_inbound("{").is_err());
        assert!(parse_inbound(&"x".repeat(MAX_CONTROL_BYTES + 1)).is_err());
    }
}

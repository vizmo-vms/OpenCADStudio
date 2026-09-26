//! Bridge cryptography (BRG-04): the mutual HMAC handshake proofs, the HKDF
//! directional keys, and AES-256-GCM sealed frames with per-direction counter
//! nonces. The byte-level conventions are pinned by `secureplan-vectors/`
//! (`README.md`, `handshake.json`, `sealed-frames.json`).
//!
//! Crates (pinned in Cargo.toml): RustCrypto `hmac`, `hkdf`, `aes-gcm` and
//! `sha2`, the current stable release line of each.

use aes_gcm::aead::{Aead, Payload};
use aes_gcm::{Aes256Gcm, KeyInit};
use hmac::{Hmac, Mac};
use sha2_011::Sha256;

/// Every secret and nonce in the handshake is 32 bytes.
pub const SECRET_LEN: usize = 32;
/// AES-GCM tag length.
pub const TAG_LEN: usize = 16;

const LABEL_WEB_TO_DESKTOP: &str = "secureplan-cad web\u{2192}desktop";
const LABEL_DESKTOP_TO_WEB: &str = "secureplan-cad desktop\u{2192}web";

type HmacSha256 = Hmac<Sha256>;

/// What a sealed frame carries; bound into the AEAD as one byte of AAD.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    /// A control message, sent as a WebSocket text frame (base64url).
    Control,
    /// A transfer chunk, sent as a WebSocket binary frame.
    Chunk,
}

impl FrameKind {
    fn aad(self) -> [u8; 1] {
        match self {
            FrameKind::Control => [0x01],
            FrameKind::Chunk => [0x02],
        }
    }
}

/// The channel refused a frame; the session must close.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelError {
    /// The frame failed authentication: tampered, replayed, reordered,
    /// reflected, of the wrong kind, or sealed under another key.
    Authentication,
    /// The frame is shorter than a tag, or is not base64url.
    Malformed,
    /// An earlier failure closed this direction.
    Closed,
    /// The frame counter is exhausted.
    CounterExhausted,
}

impl std::fmt::Display for ChannelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ChannelError::Authentication => "sealed frame failed authentication",
            ChannelError::Malformed => "malformed sealed frame",
            ChannelError::Closed => "sealed channel closed",
            ChannelError::CounterExhausted => "sealed frame counter exhausted",
        })
    }
}

fn mac(token: &[u8; SECRET_LEN], parts: &[&[u8]]) -> HmacSha256 {
    let mut mac = <HmacSha256 as KeyInit>::new_from_slice(token).expect("HMAC accepts any key length");
    for part in parts {
        mac.update(part);
    }
    mac
}

/// `proofD = HMAC-SHA256(token, "desktop" ‖ nonceW ‖ nonceD)`.
pub fn desktop_proof(token: &[u8; SECRET_LEN], nonce_w: &[u8; SECRET_LEN], nonce_d: &[u8; SECRET_LEN]) -> [u8; SECRET_LEN] {
    mac(token, &[b"desktop", nonce_w, nonce_d]).finalize().into_bytes().into()
}

/// Constant-time check of the web's `proofW = HMAC-SHA256(token, "web" ‖ nonceD ‖ nonceW)`.
pub fn verify_web_proof(
    token: &[u8; SECRET_LEN],
    nonce_w: &[u8; SECRET_LEN],
    nonce_d: &[u8; SECRET_LEN],
    proof_w: &[u8],
) -> bool {
    mac(token, &[b"web", nonce_d, nonce_w]).verify_slice(proof_w).is_ok()
}

/// `proofW`, as the web computes it (used by the tests' web side).
pub fn web_proof(token: &[u8; SECRET_LEN], nonce_w: &[u8; SECRET_LEN], nonce_d: &[u8; SECRET_LEN]) -> [u8; SECRET_LEN] {
    mac(token, &[b"web", nonce_d, nonce_w]).finalize().into_bytes().into()
}

/// Constant-time check of `proofD`, as the web performs it.
pub fn verify_desktop_proof(
    token: &[u8; SECRET_LEN],
    nonce_w: &[u8; SECRET_LEN],
    nonce_d: &[u8; SECRET_LEN],
    proof_d: &[u8],
) -> bool {
    mac(token, &[b"desktop", nonce_w, nonce_d]).verify_slice(proof_d).is_ok()
}

/// A 32-byte AES-256-GCM key that never prints.
#[derive(Clone)]
pub struct Key([u8; SECRET_LEN]);

impl std::fmt::Debug for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[redacted]")
    }
}

/// The two directional keys derived after both proofs pass.
#[derive(Debug, Clone)]
pub struct DirectionalKeys {
    pub web_to_desktop: Key,
    pub desktop_to_web: Key,
}

/// HKDF-SHA256 with the token as key material and `nonceW ‖ nonceD` as salt.
pub fn derive_keys(token: &[u8; SECRET_LEN], nonce_w: &[u8; SECRET_LEN], nonce_d: &[u8; SECRET_LEN]) -> DirectionalKeys {
    let salt = [nonce_w.as_slice(), nonce_d.as_slice()].concat();
    let hkdf = hkdf::Hkdf::<Sha256>::new(Some(&salt), token);
    let expand = |label: &str| {
        let mut key = [0u8; SECRET_LEN];
        hkdf.expand(label.as_bytes(), &mut key).expect("32 bytes is a valid HKDF-SHA256 length");
        Key(key)
    };
    DirectionalKeys {
        web_to_desktop: expand(LABEL_WEB_TO_DESKTOP),
        desktop_to_web: expand(LABEL_DESKTOP_TO_WEB),
    }
}

fn nonce(counter: u64) -> [u8; 12] {
    let mut nonce = [0u8; 12];
    nonce[4..].copy_from_slice(&counter.to_be_bytes());
    nonce
}

fn cipher(key: &Key) -> Aes256Gcm {
    <Aes256Gcm as KeyInit>::new_from_slice(&key.0).expect("AES-256 key is 32 bytes")
}

/// Seals frames in one direction, numbering them from 0.
pub struct Sealer {
    cipher: Aes256Gcm,
    counter: u64,
}

impl std::fmt::Debug for Sealer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sealer").field("counter", &self.counter).finish_non_exhaustive()
    }
}

impl Sealer {
    pub fn new(key: &Key) -> Self {
        Self { cipher: cipher(key), counter: 0 }
    }

    /// `ciphertext ‖ tag` for the next frame.
    pub fn seal(&mut self, kind: FrameKind, plaintext: &[u8]) -> Result<Vec<u8>, ChannelError> {
        let counter = self.counter;
        self.counter = counter.checked_add(1).ok_or(ChannelError::CounterExhausted)?;
        self.cipher
            .encrypt(&nonce(counter).into(), Payload { msg: plaintext, aad: &kind.aad() })
            .map_err(|_| ChannelError::Authentication)
    }
}

/// Opens frames in one direction, expecting counters 0, 1, 2, … and closing
/// for good at the first failure.
pub struct Opener {
    cipher: Aes256Gcm,
    expected: u64,
    closed: bool,
}

impl std::fmt::Debug for Opener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Opener")
            .field("expected", &self.expected)
            .field("closed", &self.closed)
            .finish_non_exhaustive()
    }
}

impl Opener {
    pub fn new(key: &Key) -> Self {
        Self { cipher: cipher(key), expected: 0, closed: false }
    }

    pub fn open(&mut self, kind: FrameKind, sealed: &[u8]) -> Result<Vec<u8>, ChannelError> {
        if self.closed {
            return Err(ChannelError::Closed);
        }
        let result = if sealed.len() < TAG_LEN {
            Err(ChannelError::Malformed)
        } else {
            self.cipher
                .decrypt(&nonce(self.expected).into(), Payload { msg: sealed, aad: &kind.aad() })
                .map_err(|_| ChannelError::Authentication)
        };
        match result {
            Ok(plaintext) => {
                self.expected = self.expected.checked_add(1).ok_or(ChannelError::CounterExhausted)?;
                Ok(plaintext)
            }
            Err(error) => {
                self.closed = true;
                Err(error)
            }
        }
    }
}

/// Base64url without padding, as used for control frames and binary values
/// in JSON.
pub fn b64url_encode(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Strict base64url decoding: no padding, canonical trailing bits.
pub fn b64url_decode(text: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(text).ok()
}

/// Decode exactly `N` bytes of base64url.
pub fn b64url_array<const N: usize>(text: &str) -> Option<[u8; N]> {
    b64url_decode(text)?.try_into().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::secureplan::vectors;
    use serde_json::Value;

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).expect("hex"))
            .collect()
    }

    fn arr(text: &str) -> [u8; 32] {
        hex(text).try_into().expect("32 bytes")
    }

    struct Inputs {
        token: [u8; 32],
        nonce_w: [u8; 32],
        nonce_d: [u8; 32],
    }

    fn inputs() -> (Inputs, Value) {
        let handshake = vectors::json("handshake.json");
        let i = &handshake["inputs"];
        let inputs = Inputs {
            token: arr(i["tokenHex"].as_str().unwrap()),
            nonce_w: arr(i["nonceWHex"].as_str().unwrap()),
            nonce_d: arr(i["nonceDHex"].as_str().unwrap()),
        };
        (inputs, handshake)
    }

    fn key_hex(key: &Key) -> Vec<u8> {
        key.0.to_vec()
    }

    #[test]
    fn proofs_match_the_handshake_vectors() {
        let (i, handshake) = inputs();
        let steps = handshake["steps"].as_array().unwrap();
        let proof_d = desktop_proof(&i.token, &i.nonce_w, &i.nonce_d);
        assert_eq!(proof_d.to_vec(), hex(steps[1]["proofDHex"].as_str().unwrap()));
        assert_eq!(b64url_encode(&proof_d), steps[1]["message"]["proofD"].as_str().unwrap());
        let proof_w = b64url_decode(steps[2]["message"]["proofW"].as_str().unwrap()).unwrap();
        assert_eq!(proof_w, hex(steps[2]["proofWHex"].as_str().unwrap()));
        assert!(verify_web_proof(&i.token, &i.nonce_w, &i.nonce_d, &proof_w));
        assert_eq!(web_proof(&i.token, &i.nonce_w, &i.nonce_d).to_vec(), proof_w);
        assert!(verify_desktop_proof(&i.token, &i.nonce_w, &i.nonce_d, &proof_d));
        for case in handshake["rejections"].as_array().unwrap().iter().filter(|c| c["side"] == "web") {
            if let Some(bad) = case["message"]["proofD"].as_str() {
                let bad = b64url_decode(bad).unwrap();
                assert!(!verify_desktop_proof(&i.token, &i.nonce_w, &i.nonce_d, &bad), "{}", case["name"]);
            }
        }
        assert_eq!(b64url_encode(&i.token), handshake["inputs"]["token"].as_str().unwrap());
    }

    #[test]
    fn bad_web_proofs_are_rejected() {
        let (i, handshake) = inputs();
        let mut rejected = 0;
        for case in handshake["rejections"].as_array().unwrap() {
            if case["side"] != "desktop" || !case["name"].as_str().unwrap().starts_with("proofW") {
                continue;
            }
            let proof = b64url_decode(case["message"]["proofW"].as_str().unwrap()).unwrap();
            assert!(!verify_web_proof(&i.token, &i.nonce_w, &i.nonce_d, &proof), "{}", case["name"]);
            rejected += 1;
        }
        assert!(rejected >= 2);
        assert!(!verify_web_proof(&i.token, &i.nonce_w, &i.nonce_d, &[]));
        // The desktop's own proof never passes as the web's (no reflection).
        let reflected = desktop_proof(&i.token, &i.nonce_w, &i.nonce_d);
        assert!(!verify_web_proof(&i.token, &i.nonce_w, &i.nonce_d, &reflected));
    }

    #[test]
    fn keys_match_the_handshake_vectors() {
        let (i, handshake) = inputs();
        let keys = derive_keys(&i.token, &i.nonce_w, &i.nonce_d);
        let step = &handshake["steps"][3];
        assert_eq!(key_hex(&keys.web_to_desktop), hex(step["webToDesktop"]["keyHex"].as_str().unwrap()));
        assert_eq!(key_hex(&keys.desktop_to_web), hex(step["desktopToWeb"]["keyHex"].as_str().unwrap()));
        assert_eq!(format!("{keys:?}").matches("[redacted]").count(), 2);
    }

    fn wire_bytes(wire: &Value) -> (FrameKind, Vec<u8>) {
        match wire["opcode"].as_str().unwrap() {
            "text" => (FrameKind::Control, b64url_decode(wire["data"].as_str().unwrap()).unwrap_or_default()),
            _ => (FrameKind::Chunk, hex(wire["dataHex"].as_str().unwrap())),
        }
    }

    fn frame_key(sealed: &Value, direction: &str) -> Key {
        let field = if direction == "webToDesktop" { "webToDesktopHex" } else { "desktopToWebHex" };
        Key(arr(sealed["keys"][field].as_str().unwrap()))
    }

    #[test]
    fn frames_seal_and_open_exactly_as_the_vectors() {
        let sealed = vectors::json("sealed-frames.json");
        for direction in ["desktopToWeb", "webToDesktop"] {
            let key = frame_key(&sealed, direction);
            let mut sealer = Sealer::new(&key);
            let mut opener = Opener::new(&key);
            for frame in sealed["frames"].as_array().unwrap().iter().filter(|f| f["direction"] == direction) {
                let kind = if frame["kind"] == "control" { FrameKind::Control } else { FrameKind::Chunk };
                let plaintext = match kind {
                    FrameKind::Control => frame["plaintext"].as_str().unwrap().as_bytes().to_vec(),
                    FrameKind::Chunk => hex(frame["plaintextHex"].as_str().unwrap()),
                };
                assert_eq!(frame["nonceHex"].as_str().unwrap(), nonce(frame["counter"].as_u64().unwrap()).iter().map(|b| format!("{b:02x}")).collect::<String>());
                let out = sealer.seal(kind, &plaintext).unwrap();
                assert_eq!(out, hex(frame["sealedHex"].as_str().unwrap()), "{direction} {}", frame["counter"]);
                let (wire_kind, wire) = wire_bytes(&frame["wire"]);
                assert_eq!(wire_kind, kind);
                assert_eq!(opener.open(wire_kind, &wire).unwrap(), plaintext);
            }
        }
    }

    #[test]
    fn tampered_replayed_reordered_and_reflected_frames_are_rejected() {
        let sealed = vectors::json("sealed-frames.json");
        let frames = sealed["frames"].as_array().unwrap();
        for case in sealed["rejections"].as_array().unwrap() {
            let direction = if case["receiver"] == "web" { "desktopToWeb" } else { "webToDesktop" };
            let key = frame_key(&sealed, direction);
            let mut opener = Opener::new(&key);
            // Bring the opener to the expected counter with the genuine frames.
            let expected = case["expectedCounter"].as_u64().unwrap();
            for frame in frames.iter().filter(|f| f["direction"] == direction).take(expected as usize) {
                let (kind, bytes) = wire_bytes(&frame["wire"]);
                opener.open(kind, &bytes).unwrap();
            }
            let (kind, bytes) = wire_bytes(&case["wire"]);
            assert!(opener.open(kind, &bytes).is_err(), "{}", case["name"]);
            // A failure closes the direction for good.
            let next = frames.iter().filter(|f| f["direction"] == direction).nth(expected as usize);
            if let Some(frame) = next {
                let (kind, bytes) = wire_bytes(&frame["wire"]);
                assert_eq!(opener.open(kind, &bytes), Err(ChannelError::Closed), "{}", case["name"]);
            }
        }
    }

    #[test]
    fn a_relay_without_the_token_cannot_read_or_forge_frames() {
        let (i, _) = inputs();
        let keys = derive_keys(&i.token, &i.nonce_w, &i.nonce_d);
        let mut web = Sealer::new(&keys.web_to_desktop);
        let secret = br#"{"type":"openSession","surveyLabel":"Synthetic survey"}"#;
        let frame = web.seal(FrameKind::Control, secret).unwrap();
        assert!(!frame.windows(b"Synthetic".len()).any(|w| w == b"Synthetic"));
        // A relay that saw both nonces but not the token derives other keys.
        let relay_keys = derive_keys(&[7u8; 32], &i.nonce_w, &i.nonce_d);
        assert!(Opener::new(&relay_keys.web_to_desktop).open(FrameKind::Control, &frame).is_err());
        let mut forged = Sealer::new(&relay_keys.desktop_to_web);
        let forged = forged.seal(FrameKind::Control, br#"{"type":"ping","requestId":"x"}"#).unwrap();
        assert!(Opener::new(&keys.desktop_to_web).open(FrameKind::Control, &forged).is_err());
    }

    #[test]
    fn base64url_is_strict() {
        assert_eq!(b64url_array::<16>("AAAAAAAAAAAAAAAAAAAAAA"), Some([0u8; 16]));
        assert_eq!(b64url_array::<16>("AAAAAAAAAAAAAAAAAAAAAA=="), None);
        assert_eq!(b64url_array::<16>("AAAAAAAAAAAAAAAAAAAAAB"), None, "non-canonical trailing bits");
        assert_eq!(b64url_array::<16>("AAAAAAAAAAAAAAAAAAAA+A"), None, "standard alphabet");
    }
}

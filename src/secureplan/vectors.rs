//! The SecurePlan protocol vectors (CON-07), copied byte-for-byte from the
//! canonical `desktop/protocol-vectors` in vizmo-vms/secure-plan into
//! `secureplan-vectors/`. The copy must match [`MANIFEST_SHA256`], the value
//! recorded in that repository's `desktop/README.md`; see its pin-bump
//! procedure before changing either.

use sha2::{Digest, Sha256};
use std::path::PathBuf;

/// SHA-256 of `secureplan-vectors/manifest.json`.
pub(crate) const MANIFEST_SHA256: &str =
    "91d1e60be39dbd0aa197a3290638a0aaca44ecdd9b31dcb97b3627e3101a2864";

pub(crate) fn path(relative: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("secureplan-vectors")
        .join(relative)
}

pub(crate) fn bytes(relative: &str) -> Vec<u8> {
    std::fs::read(path(relative)).unwrap_or_else(|e| panic!("read vector {relative}: {e}"))
}

pub(crate) fn json(relative: &str) -> serde_json::Value {
    serde_json::from_slice(&bytes(relative)).unwrap_or_else(|e| panic!("parse vector {relative}: {e}"))
}

fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data).iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn vector_copy_matches_the_pinned_manifest() {
    assert_eq!(
        sha256_hex(&bytes("manifest.json")),
        MANIFEST_SHA256,
        "secureplan-vectors/manifest.json differs from the pinned manifest"
    );
    let manifest = json("manifest.json");
    let files = manifest["files"].as_array().expect("manifest files");
    assert!(!files.is_empty());
    for file in files {
        let name = file["path"].as_str().expect("path");
        let data = bytes(name);
        assert_eq!(data.len() as u64, file["bytes"].as_u64().expect("bytes"), "{name}");
        assert_eq!(sha256_hex(&data), file["sha256"].as_str().expect("sha256"), "{name}");
    }
}

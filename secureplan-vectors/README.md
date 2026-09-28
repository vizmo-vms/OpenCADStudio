# SecurePlan CAD protocol vectors (CON-07)

Canonical synthetic vectors shared by the SecurePlan web app and the SecurePlan CAD desktop (`vizmo-vms/OpenCADStudio`, branch `secureplan`, directory `secureplan-vectors/`). The requirements are in [the specification](../../docs/spec/desktop-cad-spec.md); this file pins the byte-level conventions the specification leaves open. `manifest.json` lists every file with its SHA-256. Do not edit generated files by hand: change `scripts/generate-protocol-vectors.mjs` (or a hand-written schema or this file), run it, and follow the pin-bump procedure in [`desktop/README.md`](../README.md).

| File | Contents |
| --- | --- |
| `schemas/*.schema.json` | JSON Schemas (draft 2020-12) for every BRG-07 message and payload. Hand-written. |
| `launch-urls.json` | BRG-01 launch URLs and BRG-02 origin eligibility. |
| `handshake.json` | BRG-03 upgrade checks and each BRG-04 handshake step, with rejections. |
| `sealed-frames.json` | AES-256-GCM sealed frames and rejections (tampered, replayed, out of order, reflected). |
| `transfers.json` | BRG-06 transfer event sequences and limits. |
| `placement.json` | CON-01 page placement and CON-05 float32 precision cases. |
| `base-identity.json` | PUB-06 base-identity samples. |
| `snap/` | Valid SPSNAP files and one invalid file per CON-05 rule, described by `snap/cases.json`. |
| `messages/`, `payloads/` | Valid and invalid message and payload samples. |

## Encodings

- **Binary values in JSON** (pairing id, token, nonces, proofs) are base64url without padding: 16 bytes → 22 characters, 32 bytes → 43 characters.
- **Launch URL:** `secureplan-cad://pair?v=1&origin=…&pairing=…&token=…&survey=…&intent=…`. Values are percent-encoded; each of the six parameters appears exactly once, in any order. Unknown parameters and fragments are rejected. `survey` is a lowercase UUID v4.
- **Origins** are ASCII serialized origins as browsers send them: lowercase scheme and host, no default port, no path, userinfo or trailing slash. `https` origins are eligible; `http://localhost[:port]` and `http://127.0.0.1[:port]` only with the developer setting.
- **Versions** are `MAJOR.MINOR.PATCH` without leading zeros and compared numerically.

## Handshake (BRG-04)

1. The web sends plaintext `hello`. The desktop answers `challenge` with `proofD = HMAC-SHA256(token, "desktop" ‖ nonceW ‖ nonceD)`, where the HMAC key is the 32 raw token bytes and the nonces are raw bytes.
2. The web sends `prove` with `proofW = HMAC-SHA256(token, "web" ‖ nonceD ‖ nonceW)`.
3. Both sides derive 32-byte keys with HKDF-SHA256: key material = token, salt = `nonceW ‖ nonceD`, info = the UTF-8 label `secureplan-cad web→desktop` or `secureplan-cad desktop→web` (the arrow is U+2192).
4. The desktop sends `welcome` as its first sealed frame. The web sends no sealed frame before it has accepted `welcome`.

`hello`, `challenge` and `prove` are the only plaintext frames. A binary frame before the handshake completes, an unknown or consumed pairing, a failed proof, or a handshake longer than 10 s closes the connection.

## Sealed frames

- AES-256-GCM with the sender's directional key. The 12-byte nonce is four zero bytes followed by the per-direction frame counter as a big-endian u64, starting at 0 and increasing by one for every sealed frame in that direction. The receiver uses its own expected counter, so a replayed, reordered or dropped frame fails authentication and closes the session.
- The additional authenticated data is one byte: `0x01` for a control message, `0x02` for a transfer chunk.
- A control message is the UTF-8 JSON of the message (at most 1,048,576 bytes), sent as a WebSocket **text** frame containing base64url (no padding) of `ciphertext ‖ 16-byte tag`.
- A transfer chunk is `u32le transferId ‖ u32le seq ‖ payload` (payload 1 to 1,048,576 bytes), sent as a WebSocket **binary** frame containing `ciphertext ‖ tag`.

## Transfers (BRG-06)

`transferStart` announces a transfer before the message that references it. Transfer ids are unique per session and direction and are never reused. Chunks carry `seq` 0, 1, 2, …; the transfer completes when the received length equals `byteLength` and the SHA-256 matches. A zero-length transfer completes on `transferStart`. At most three transfers are in flight per session. The limit is split by direction so that `transferStart` frames crossing on the wire can never exceed it: at most two web-to-desktop and at most one desktop-to-web. A sender counts only its own unfinished outgoing transfers and waits for one to finish rather than exceed its share (the desktop sends each transfer whole before the next). A receiver refuses a `transferStart` that would give it more unfinished incoming transfers than the sender's share, which a conforming sender never causes. Cases with a `receiver` field apply to that receiver only. 30 s without progress fails the transfer. Limits: 52,428,800 bytes, or 8,388,608 bytes for the two `+json` payload media types. A transfer failure closes the session with `transferFailed`.

## Messages

Every control message has `type` and `requestId`. Responses (`applyProgress`, `applyResult`, `convertResult`, `exportResult`) reuse the request's `requestId`. `openSession` and `overlayUpdate` carry a required boolean `surveyEmpty`: true only when the web draft the overlay was built from has no design elements and no comments, the CON-03/PUB-02 empty survey. The desktop uses it, not the overlay, to decide empty-survey placement, because comments are not in the overlay. The web sends `overlayUpdate` again whenever that emptiness changes. `exportRequest` carries the captured snapshot's `cadPlan.mapping`; the desktop composes the export with that mapping, never its own stored one, because a later Apply can re-align the plan while keeping identical drawing bytes. Unknown types or fields are rejected, as are the `semantic` rules in `messages/invalid.json`: JSON payload size, page area ≤ 64,000,000 pt², and `heightMm = heightPt · widthMm / widthPt` within 1e-6 relative. `applyProgress` with step `awaitingConfirmation` (progress `null`) tells the desktop that the web is showing its Apply confirmation dialog (SecurePlan CAD 0.2.2).

## Overlay and export payloads (schema version 2)

From SecurePlan CAD 0.2.2 the overlay and export payloads are `schemaVersion` 2 and carry the web canvas's device drawing; each side rejects the other version, so an older desktop fails cleanly. Both carry `icons`: each entry is `{id, mediaType, data}` with `data` the standard padded base64 of the exact bytes the web canvas draws (a custom catalog image, a bundled catalog SVG, or a lucide glyph serialized as a standalone SVG). The web sets `id` to the first 16 lowercase hex characters of SHA-256(UTF-8 media type ‖ 0x00 ‖ bytes), so identical bytes share one entry; receivers treat it as an opaque key. A device's `iconId` names its entry, or is `null` for the desktop's built-in standard symbol. Beyond the schemas, both sides enforce the `semantic` rules in `payloads/invalid.json`: icon ids are unique, every `iconId` names an entry, and `data` is canonical padded base64 (length a multiple of 4) of 1 to 262,144 bytes.

## Page placement and SPSNAP

See the `description` fields of `placement.json` and `snap/cases.json`. The SPSNAP reader rejects each rule listed in `snap/cases.json`; reserved flag bits (all but bit 0) must be zero. A gzip body is exactly one gzip member whose deflate stream ends at the file's final 8-byte CRC-32/ISIZE trailer; a second member or trailing data is a `counts` rejection, even where a platform's gzip decoder would accept it. A published PDF page has no `/UserUnit` (or `/UserUnit 1`).

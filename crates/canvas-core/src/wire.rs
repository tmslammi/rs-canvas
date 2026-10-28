//! Binary frames for the page/server document path.
//!
//! Snapshots and patches travel as WebSocket binary frames. The frame header
//! carries the wire version, the frame kind, the sender's document revision and
//! the payload length, so a receiver can reject a future or malformed frame
//! before decoding the document:
//!
//! ```text
//! offset  size  field
//! 0       8     magic bytes, b"RSCWIRE\n"
//! 8       1     wire version (1)
//! 9       1     frame kind (1 = snapshot, 2 = patch)
//! 10      8     document revision, little-endian u64
//! 18      4     payload length, little-endian u32
//! 22      ..    payload
//! ```
//!
//! A snapshot payload is the existing versioned document envelope from
//! [`crate::envelope`] (`RSCDOC\r\n...`). A patch payload is the frozen v1
//! element layout from [`crate::binary`] as a bare element sequence. Neither
//! path builds a JSON value: the server and wasm decode straight into the
//! model, and the browser shell passes the bytes through untouched.
//!
//! The wire magic is deliberately different from the document magic: a frame
//! is not a document, and a receiver that is handed the wrong bytes can reject
//! them before touching the payload.
//!
//! For a server-to-page frame the revision is the sender's document revision.
//! For a page-to-server snapshot it is the page's base: the last server
//! revision it applied (zero before the first server frame). The server
//! records the base and returns it in the frame outcome, but revisions are
//! catch-up metadata only — element-level last-writer-wins decides every
//! merge, and absence never deletes. Page-to-server patches ignore the field.

use std::fmt;

use crate::binary;
use crate::envelope;
use crate::model::{Document, Element};
use crate::serialization::{self, SchemaError};

/// Magic bytes at the start of every binary wire frame.
pub const MAGIC: &[u8; 8] = b"RSCWIRE\n";
/// The wire framing version. Bump only for a header/layout change; the
/// document payload keeps its own schema version inside the envelope.
pub const VERSION: u8 = 1;
/// Magic, version, kind, revision and payload length.
pub const HEADER_LEN: usize = MAGIC.len() + 1 + 1 + 8 + 4;

/// What a frame carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameKind {
    /// A full document, payload wrapped in the document envelope.
    Snapshot = 1,
    /// A set of full elements, tombstones included.
    Patch = 2,
}

impl FrameKind {
    pub fn id(self) -> u8 {
        self as u8
    }

    pub fn from_id(id: u8) -> Option<Self> {
        match id {
            1 => Some(FrameKind::Snapshot),
            2 => Some(FrameKind::Patch),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub enum WireError {
    /// The bytes are not a wire frame; a JSON control frame or document bytes
    /// must not be treated as one.
    NotWire,
    /// Fewer bytes than the fixed header.
    TruncatedHeader,
    /// The header declares more payload than the frame carries.
    TruncatedPayload { declared: usize, actual: usize },
    /// The header declares less payload than the frame carries.
    TrailingBytes { declared: usize, actual: usize },
    /// A payload larger than the 32-bit length field can describe.
    PayloadTooLarge(usize),
    /// A framing version this build does not understand.
    UnsupportedVersion(u8),
    /// An unknown frame kind.
    UnknownKind(u8),
    /// A snapshot decoder was handed a patch, or the reverse.
    WrongKind {
        expected: FrameKind,
        actual: FrameKind,
    },
    /// A snapshot payload is not a `RSCDOC` envelope.
    NotDocumentEnvelope,
    /// The document envelope or its binary payload is invalid.
    Document(SchemaError),
    /// The patch payload is not a valid v1 element list.
    Patch(String),
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WireError::NotWire => write!(f, "not a binary wire frame"),
            WireError::TruncatedHeader => write!(f, "truncated wire frame header"),
            WireError::TruncatedPayload { declared, actual } => write!(
                f,
                "truncated wire frame payload: header declares {declared} bytes, found {actual}"
            ),
            WireError::TrailingBytes { declared, actual } => write!(
                f,
                "unexpected bytes after wire frame payload: header declares {declared} bytes, found {actual}"
            ),
            WireError::PayloadTooLarge(len) => {
                write!(f, "wire frame payload of {len} bytes is too large")
            }
            WireError::UnsupportedVersion(version) => {
                write!(f, "wire frame version {version} is not supported by this app")
            }
            WireError::UnknownKind(kind) => write!(f, "wire frame kind {kind} is unknown"),
            WireError::WrongKind { expected, actual } => write!(
                f,
                "wire frame kind {:?} cannot be decoded as {:?}",
                actual, expected
            ),
            WireError::NotDocumentEnvelope => {
                write!(f, "wire snapshot payload is not a binary document envelope")
            }
            WireError::Document(error) => write!(f, "invalid wire snapshot: {error}"),
            WireError::Patch(message) => write!(f, "invalid wire patch: {message}"),
        }
    }
}

impl std::error::Error for WireError {}

/// A parsed header whose payload has already been length-checked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header<'a> {
    pub version: u8,
    pub kind: FrameKind,
    pub revision: u64,
    pub payload: &'a [u8],
}

/// Whether `bytes` starts with the wire magic.
pub fn is_wire(bytes: &[u8]) -> bool {
    bytes.starts_with(MAGIC)
}

/// Parse the header and return the payload slice. Does not decode the payload.
pub fn parse(bytes: &[u8]) -> Result<Header<'_>, WireError> {
    if !is_wire(bytes) {
        return Err(WireError::NotWire);
    }
    if bytes.len() < HEADER_LEN {
        return Err(WireError::TruncatedHeader);
    }
    let version = bytes[8];
    if version != VERSION {
        return Err(WireError::UnsupportedVersion(version));
    }
    let kind_id = bytes[9];
    let kind = FrameKind::from_id(kind_id).ok_or(WireError::UnknownKind(kind_id))?;
    let revision = u64::from_le_bytes(bytes[10..18].try_into().expect("eight bytes"));
    let declared = u32::from_le_bytes(bytes[18..22].try_into().expect("four bytes")) as usize;
    let actual = bytes.len() - HEADER_LEN;
    if actual < declared {
        return Err(WireError::TruncatedPayload { declared, actual });
    }
    if actual > declared {
        return Err(WireError::TrailingBytes { declared, actual });
    }
    Ok(Header {
        version,
        kind,
        revision,
        payload: &bytes[HEADER_LEN..],
    })
}

/// The frame kind, after validating the header and payload length.
pub fn kind(bytes: &[u8]) -> Result<FrameKind, WireError> {
    Ok(parse(bytes)?.kind)
}

/// Encode a full document as a snapshot frame. The payload is the same
/// versioned document envelope persistence uses.
pub fn encode_snapshot(revision: u64, doc: &Document) -> Result<Vec<u8>, WireError> {
    let payload = serialization::to_bytes(doc).map_err(WireError::Document)?;
    encode_frame(FrameKind::Snapshot, revision, &payload)
}

/// Encode a set of full elements (tombstones included) as a patch frame.
pub fn encode_patch(revision: u64, put: &[Element]) -> Result<Vec<u8>, WireError> {
    let payload =
        binary::encode_elements(put).map_err(|error| WireError::Patch(error.to_string()))?;
    encode_frame(FrameKind::Patch, revision, &payload)
}

/// Decode a snapshot frame into its revision and document.
pub fn decode_snapshot(bytes: &[u8]) -> Result<(u64, Document), WireError> {
    let header = parse(bytes)?;
    expect_kind(header.kind, FrameKind::Snapshot)?;
    if !envelope::is_binary(header.payload) {
        return Err(WireError::NotDocumentEnvelope);
    }
    let doc = serialization::from_bytes(header.payload).map_err(WireError::Document)?;
    Ok((header.revision, doc))
}

/// Decode a patch frame into its revision and full elements.
pub fn decode_patch(bytes: &[u8]) -> Result<(u64, Vec<Element>), WireError> {
    let header = parse(bytes)?;
    expect_kind(header.kind, FrameKind::Patch)?;
    let elements = binary::decode_elements(header.payload)
        .map_err(|error| WireError::Patch(error.to_string()))?;
    Ok((header.revision, elements))
}

fn expect_kind(actual: FrameKind, expected: FrameKind) -> Result<(), WireError> {
    if actual == expected {
        Ok(())
    } else {
        Err(WireError::WrongKind { expected, actual })
    }
}

fn encode_frame(kind: FrameKind, revision: u64, payload: &[u8]) -> Result<Vec<u8>, WireError> {
    let len =
        u32::try_from(payload.len()).map_err(|_| WireError::PayloadTooLarge(payload.len()))?;
    let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
    out.extend_from_slice(MAGIC);
    out.push(VERSION);
    out.push(kind.id());
    out.extend_from_slice(&revision.to_le_bytes());
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(payload);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Rect;
    use crate::model::{ElementKind, ShapeKind, Style};

    fn shape(x: f64) -> ElementKind {
        ElementKind::Shape {
            shape: ShapeKind::Rectangle,
            rect: Rect::new(x, 0.0, 10.0, 10.0),
            style: Style::default(),
            bound_text_id: None,
        }
    }

    fn document() -> Document {
        let mut doc = Document::new();
        doc.add(shape(0.0), 1);
        doc.add(shape(20.0), 2);
        doc
    }

    #[test]
    fn snapshot_round_trips_and_carries_the_document_envelope() {
        let doc = document();
        let bytes = encode_snapshot(7, &doc).unwrap();
        let header = parse(&bytes).unwrap();
        assert_eq!(header.version, VERSION);
        assert_eq!(header.kind, FrameKind::Snapshot);
        assert_eq!(header.revision, 7);
        assert!(envelope::is_binary(header.payload));
        assert_eq!(bytes[HEADER_LEN..HEADER_LEN + 8], *envelope::MAGIC);
        let (revision, back) = decode_snapshot(&bytes).unwrap();
        assert_eq!(revision, 7);
        assert_eq!(back, doc);
    }

    #[test]
    fn patch_round_trips_full_elements_with_tombstones() {
        let mut doc = document();
        let id = doc.elements.keys().next().unwrap().clone();
        doc.element_mut(&id).unwrap().is_deleted = true;
        let put: Vec<Element> = doc.elements.into_values().collect();
        let bytes = encode_patch(11, &put).unwrap();
        let header = parse(&bytes).unwrap();
        assert_eq!(header.kind, FrameKind::Patch);
        assert_eq!(header.revision, 11);
        assert!(!envelope::is_binary(header.payload));
        let (revision, decoded) = decode_patch(&bytes).unwrap();
        assert_eq!(revision, 11);
        assert_eq!(decoded, put);
        assert!(decoded.iter().any(|element| element.is_deleted));
    }

    #[test]
    fn empty_patch_round_trips() {
        let bytes = encode_patch(0, &[]).unwrap();
        let (_, put) = decode_patch(&bytes).unwrap();
        assert!(put.is_empty());
    }

    #[test]
    fn header_layout_is_frozen() {
        let bytes = encode_patch(0x0102_0304_0506_0708, &[]).unwrap();
        assert_eq!(&bytes[..8], MAGIC);
        assert_eq!(bytes[8], 1, "wire version");
        assert_eq!(bytes[9], 2, "patch kind");
        assert_eq!(
            &bytes[10..18],
            &[0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01],
            "revision is little-endian"
        );
        // A postcard sequence still carries its one-byte length prefix.
        assert_eq!(&bytes[18..22], &[1, 0, 0, 0], "payload length");
        assert_eq!(bytes.len(), HEADER_LEN + 1);
    }

    #[test]
    fn document_bytes_and_json_are_not_wire_frames() {
        let doc = document();
        let document_bytes = serialization::to_bytes(&doc).unwrap();
        assert!(!is_wire(&document_bytes));
        assert!(matches!(parse(&document_bytes), Err(WireError::NotWire)));
        let json = serialization::to_json(&doc).unwrap();
        assert!(matches!(parse(json.as_bytes()), Err(WireError::NotWire)));
    }

    #[test]
    fn future_versions_and_unknown_kinds_are_rejected_before_the_payload() {
        let mut bytes = encode_snapshot(1, &document()).unwrap();
        bytes[8] = 99;
        assert!(matches!(
            parse(&bytes),
            Err(WireError::UnsupportedVersion(99))
        ));
        let mut bytes = encode_snapshot(1, &document()).unwrap();
        bytes[9] = 99;
        assert!(matches!(parse(&bytes), Err(WireError::UnknownKind(99))));
    }

    #[test]
    fn truncated_headers_and_payloads_are_rejected() {
        let bytes = encode_snapshot(1, &document()).unwrap();
        for len in MAGIC.len()..HEADER_LEN {
            assert!(matches!(
                parse(&bytes[..len]),
                Err(WireError::TruncatedHeader)
            ));
        }
        assert!(matches!(
            parse(&bytes[..bytes.len() - 1]),
            Err(WireError::TruncatedPayload { .. })
        ));
    }

    #[test]
    fn trailing_bytes_and_wrong_kinds_are_rejected() {
        let mut bytes = encode_patch(1, &[]).unwrap();
        bytes.push(0);
        assert!(matches!(
            parse(&bytes),
            Err(WireError::TrailingBytes { .. })
        ));

        let snapshot = encode_snapshot(1, &document()).unwrap();
        assert!(matches!(
            decode_patch(&snapshot),
            Err(WireError::WrongKind { .. })
        ));
        let patch = encode_patch(1, &[]).unwrap();
        assert!(matches!(
            decode_snapshot(&patch),
            Err(WireError::WrongKind { .. })
        ));
    }

    #[test]
    fn snapshots_must_carry_a_document_envelope_and_patches_must_decode() {
        let mut snapshot = encode_snapshot(1, &document()).unwrap();
        // Replace the payload with JSON of the same length class: the framing
        // is intact but the payload contract is not.
        let payload = serialization::to_json(&document()).unwrap();
        snapshot.truncate(HEADER_LEN);
        snapshot.extend_from_slice(payload.as_bytes());
        let declared = snapshot.len() - HEADER_LEN;
        snapshot[18..22].copy_from_slice(&(declared as u32).to_le_bytes());
        assert!(matches!(
            decode_snapshot(&snapshot),
            Err(WireError::NotDocumentEnvelope)
        ));

        let corrupt = encode_frame(FrameKind::Patch, 1, &[0xff; 8]).unwrap();
        assert!(matches!(decode_patch(&corrupt), Err(WireError::Patch(_))));
    }
}

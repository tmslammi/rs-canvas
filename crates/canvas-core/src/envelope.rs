//! The versioned binary document envelope.
//!
//! A persisted document is a fixed header followed by an opaque payload:
//!
//! ```text
//! offset  size  field
//! 0       8     magic bytes, b"RSCDOC\r\n"
//! 8       4     schema version, little-endian u32
//! 12      1     codec id (1 = postcard)
//! 13      4     payload length, little-endian u32
//! 17      ..    codec payload
//! ```
//!
//! The envelope is deliberately independent of the payload codec: a future
//! schema or codec is rejected by id/version before any payload is decoded, and
//! a future binary format can reuse the header. `serialization::from_bytes`
//! rejects anything without the magic.

use std::fmt;

/// Magic bytes at the start of every binary document.
pub const MAGIC: &[u8; 8] = b"RSCDOC\r\n";
/// Magic plus version, codec id and payload length.
pub const HEADER_LEN: usize = MAGIC.len() + 4 + 1 + 4;

/// Payload encodings understood inside the envelope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Codec {
    /// `postcard`'s positional encoding with the explicit v1 binary schema in
    /// `crate::binary`.
    Postcard = 1,
}

impl Codec {
    pub fn id(self) -> u8 {
        self as u8
    }

    pub fn from_id(id: u8) -> Option<Self> {
        match id {
            1 => Some(Codec::Postcard),
            _ => None,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum EnvelopeError {
    /// The bytes do not carry the binary magic.
    NotBinary,
    /// Fewer bytes than the fixed header.
    TruncatedHeader,
    /// The header declares more payload than the input carries.
    TruncatedPayload { declared: usize, actual: usize },
    /// The header declares less payload than the input carries.
    TrailingBytes { declared: usize, actual: usize },
    /// A payload larger than the 32-bit length field can describe.
    PayloadTooLarge(usize),
    /// The codec id is unknown to this build.
    UnsupportedCodec(u8),
}

impl fmt::Display for EnvelopeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EnvelopeError::NotBinary => write!(f, "not a binary document"),
            EnvelopeError::TruncatedHeader => write!(f, "truncated document header"),
            EnvelopeError::TruncatedPayload { declared, actual } => write!(
                f,
                "truncated document payload: header declares {declared} bytes, found {actual}"
            ),
            EnvelopeError::TrailingBytes { declared, actual } => write!(
                f,
                "unexpected bytes after document payload: header declares {declared} bytes, found {actual}"
            ),
            EnvelopeError::PayloadTooLarge(len) => {
                write!(f, "document payload of {len} bytes is too large")
            }
            EnvelopeError::UnsupportedCodec(id) => {
                write!(f, "document codec {id} is not supported by this app")
            }
        }
    }
}

impl std::error::Error for EnvelopeError {}

/// A decoded, still-opaque document payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Envelope<'a> {
    pub schema_version: u32,
    pub codec: Codec,
    pub payload: &'a [u8],
}

/// Whether `bytes` starts with the binary document magic.
pub fn is_binary(bytes: &[u8]) -> bool {
    bytes.starts_with(MAGIC)
}

/// Wrap `payload` in the binary document envelope.
pub fn encode(schema_version: u32, codec: Codec, payload: &[u8]) -> Result<Vec<u8>, EnvelopeError> {
    let len =
        u32::try_from(payload.len()).map_err(|_| EnvelopeError::PayloadTooLarge(payload.len()))?;
    let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&schema_version.to_le_bytes());
    out.push(codec.id());
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(payload);
    Ok(out)
}

/// Parse the header and return the payload slice. Does not decode the payload.
pub fn decode(bytes: &[u8]) -> Result<Envelope<'_>, EnvelopeError> {
    if !is_binary(bytes) {
        return Err(EnvelopeError::NotBinary);
    }
    if bytes.len() < HEADER_LEN {
        return Err(EnvelopeError::TruncatedHeader);
    }
    let schema_version = u32::from_le_bytes(bytes[8..12].try_into().expect("four bytes"));
    let codec_id = bytes[12];
    let declared = u32::from_le_bytes(bytes[13..17].try_into().expect("four bytes")) as usize;
    let actual = bytes.len() - HEADER_LEN;
    if actual < declared {
        return Err(EnvelopeError::TruncatedPayload { declared, actual });
    }
    if actual > declared {
        return Err(EnvelopeError::TrailingBytes { declared, actual });
    }
    let codec = Codec::from_id(codec_id).ok_or(EnvelopeError::UnsupportedCodec(codec_id))?;
    Ok(Envelope {
        schema_version,
        codec,
        payload: &bytes[HEADER_LEN..],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_header_and_payload() {
        let bytes = encode(7, Codec::Postcard, b"payload").unwrap();
        assert!(is_binary(&bytes));
        let envelope = decode(&bytes).unwrap();
        assert_eq!(envelope.schema_version, 7);
        assert_eq!(envelope.codec, Codec::Postcard);
        assert_eq!(envelope.payload, b"payload");
    }

    #[test]
    fn json_is_not_binary() {
        let json = br#"{"schemaVersion":1,"elements":{}}"#;
        assert!(!is_binary(json));
        assert_eq!(decode(json), Err(EnvelopeError::NotBinary));
    }

    #[test]
    fn truncated_header_is_rejected() {
        let bytes = encode(1, Codec::Postcard, b"payload").unwrap();
        for len in MAGIC.len()..HEADER_LEN {
            assert_eq!(
                decode(&bytes[..len]),
                Err(EnvelopeError::TruncatedHeader),
                "length {len}"
            );
        }
    }

    #[test]
    fn truncated_payload_is_rejected() {
        let bytes = encode(1, Codec::Postcard, b"payload").unwrap();
        assert_eq!(
            decode(&bytes[..bytes.len() - 1]),
            Err(EnvelopeError::TruncatedPayload {
                declared: 7,
                actual: 6
            })
        );
    }

    #[test]
    fn trailing_bytes_are_rejected() {
        let mut bytes = encode(1, Codec::Postcard, b"payload").unwrap();
        bytes.push(0);
        assert_eq!(
            decode(&bytes),
            Err(EnvelopeError::TrailingBytes {
                declared: 7,
                actual: 8
            })
        );
    }

    #[test]
    fn unknown_codec_is_rejected() {
        let mut bytes = encode(1, Codec::Postcard, b"payload").unwrap();
        bytes[12] = 99;
        assert_eq!(decode(&bytes), Err(EnvelopeError::UnsupportedCodec(99)));
    }
}

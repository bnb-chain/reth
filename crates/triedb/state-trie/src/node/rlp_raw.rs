//! Raw RLP helpers for trie node decoding.
//!
//! This module contains a minimal, idiomatic Rust port of go-ethereum's
//! `readKind` and `readSize` helpers, which perform low-level inspection of
//! RLP-encoded bytes while enforcing canonical form.

use std::fmt;

/// Error types that can occur while reading raw RLP data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RlpRawError {
    /// The operation attempted to read past the end of the buffer.
    UnexpectedEof,
    /// Non-canonical size information encountered.
    CanonSize,
    /// The RLP value claims to be larger than the provided buffer.
    ValueTooLarge,
}

impl fmt::Display for RlpRawError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RlpRawError::UnexpectedEof => write!(f, "unexpected end of input"),
            RlpRawError::CanonSize => write!(f, "non-canonical size information"),
            RlpRawError::ValueTooLarge => write!(f, "value larger than input"),
        }
    }
}

impl std::error::Error for RlpRawError {}

/// Kind represents the kind of value contained in an RLP stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A single byte, 0x00 – 0x7f.
    Byte,
    /// A byte-string (arbitrary length).
    String,
    /// A list of RLP values.
    List,
}

/// Read the tag information (kind, tag-size and content-size) from the beginning of `buf`.
///
/// This is a direct translation of Go-Ethereum's `readKind` helper that operates on a byte slice
/// and performs the basic canonical-form checks required by RLP.
pub fn read_kind(buf: &[u8]) -> Result<(Kind, u64, u64), RlpRawError> {
    if buf.is_empty() {
        return Err(RlpRawError::UnexpectedEof);
    }

    let b = buf[0];
    let (kind, tag_size, content_size, err): (Kind, u64, u64, Option<RlpRawError>) =
        match b {
            0x00..=0x7F => (Kind::Byte, 0, 1, None),
            0x80..=0xB7 => {
                let cs = (b - 0x80) as u64;
                // Reject strings that should've been single bytes.
                if cs == 1 && buf.len() > 1 && buf[1] < 128 {
                    (Kind::String, 1, 0, Some(RlpRawError::CanonSize))
                } else {
                    (Kind::String, 1, cs, None)
                }
            }
            0xB8..=0xBF => {
                let size_len = b - 0xB7; // number of bytes used to encode the size
                let tag_sz = (size_len as u64) + 1;
                match read_size(&buf[1..], size_len) {
                    Ok(cs) => (Kind::String, tag_sz, cs, None),
                    Err(e) => (Kind::String, tag_sz, 0, Some(e)),
                }
            }
            0xC0..=0xF7 => {
                let cs = (b - 0xC0) as u64;
                (Kind::List, 1, cs, None)
            }
            _ => {
                // 0xF8..=0xFF
                let size_len = b - 0xF7; // number of bytes used to encode the size
                let tag_sz = (size_len as u64) + 1;
                match read_size(&buf[1..], size_len) {
                    Ok(cs) => (Kind::List, tag_sz, cs, None),
                    Err(e) => (Kind::List, tag_sz, 0, Some(e)),
                }
            }
        };

    if let Some(e) = err {
        return Err(e);
    }

    // Reject values larger than the input slice.
    if content_size > (buf.len() as u64).saturating_sub(tag_size) {
        return Err(RlpRawError::ValueTooLarge);
    }

    Ok((kind, tag_size, content_size))
}

/// Reads a big-endian size of length `slen` (1–8) from `b`.
/// Performs canonical-form checks identical to the Go implementation.
pub fn read_size(b: &[u8], slen: u8) -> Result<u64, RlpRawError> {
    if (slen as usize) > b.len() {
        return Err(RlpRawError::UnexpectedEof);
    }

    let s = match slen {
        1 => b[0] as u64,
        2 => ((b[0] as u64) << 8) | (b[1] as u64),
        3 => ((b[0] as u64) << 16) | ((b[1] as u64) << 8) | (b[2] as u64),
        4 => ((b[0] as u64) << 24)
            | ((b[1] as u64) << 16)
            | ((b[2] as u64) << 8)
            | (b[3] as u64),
        5 => ((b[0] as u64) << 32)
            | ((b[1] as u64) << 24)
            | ((b[2] as u64) << 16)
            | ((b[3] as u64) << 8)
            | (b[4] as u64),
        6 => ((b[0] as u64) << 40)
            | ((b[1] as u64) << 32)
            | ((b[2] as u64) << 24)
            | ((b[3] as u64) << 16)
            | ((b[4] as u64) << 8)
            | (b[5] as u64),
        7 => ((b[0] as u64) << 48)
            | ((b[1] as u64) << 40)
            | ((b[2] as u64) << 32)
            | ((b[3] as u64) << 24)
            | ((b[4] as u64) << 16)
            | ((b[5] as u64) << 8)
            | (b[6] as u64),
        8 => ((b[0] as u64) << 56)
            | ((b[1] as u64) << 48)
            | ((b[2] as u64) << 40)
            | ((b[3] as u64) << 32)
            | ((b[4] as u64) << 24)
            | ((b[5] as u64) << 16)
            | ((b[6] as u64) << 8)
            | (b[7] as u64),
        _ => 0, // unreachable due to earlier checks, but keeps compiler happy
    };

    // Reject sizes < 56 (shouldn't have separate size) and sizes with leading zero bytes.
    if s < 56 || b[0] == 0 {
        return Err(RlpRawError::CanonSize);
    }

    Ok(s)
}

/// Splits the provided buffer into the first RLP value's kind, its content bytes,
/// and any remaining bytes after that value.
///
/// This mirrors go-ethereum's `Split` helper.
pub fn split<'a>(b: &'a [u8]) -> Result<(Kind, &'a [u8], &'a [u8]), RlpRawError> {
    let (kind, tag_size, content_size) = match read_kind(b) {
        Ok(res) => res,
        Err(e) => return Err(e),
    };

    let ts = tag_size as usize;
    let cs = content_size as usize;

    Ok((kind, &b[ts..ts + cs], &b[ts + cs..]))
}

/// Write bytes to the output buffer with RLP string header encoding
/// Similar to Go's encBuffer.writeBytes method
pub fn write_bytes(out: &mut dyn alloy_rlp::BufMut, b: &[u8]) {
    if b.len() == 1 && b[0] <= 0x7F {
        // fits single byte, no string header needed
        out.put_u8(b[0]);
    } else {
        // encode string header and then the bytes
        let mut temp_buf = Vec::new();
        encode_string_header(&mut temp_buf, b.len());
        temp_buf.extend_from_slice(b);
        out.put_slice(&temp_buf);
    }
}

/// Encode RLP string header for the given length
/// This follows the RLP encoding rules:
/// - If length < 56, use single byte: 0x80 + length
/// - If length >= 56, use: 0xB7 + length_of_length, followed by length bytes
fn encode_string_header(out: &mut Vec<u8>, length: usize) {
    if length < 56 {
        // Single byte header: 0x80 + length
        out.push(0x80 + (length as u8));
    } else {
        // Multi-byte header: 0xB7 + length_of_length, followed by length
        // Similar to Go's implementation: sizesize := putint(buf.sizebuf[1:], uint64(size))
        let mut size_buf = [0u8; 8]; // Buffer for size bytes
        let size_size = putint(&mut size_buf[1..], length as u64);

        // Set header byte: 0xB7 + size_size
        size_buf[0] = 0xB7 + size_size as u8;

        // Write header byte and size bytes
        out.push(size_buf[0]);
        out.extend_from_slice(&size_buf[1..size_size + 1]);
    }
}

/// Put integer into byte buffer (big-endian encoding)
/// Similar to Go's putint function
/// Returns the number of bytes written
pub fn putint(b: &mut [u8], i: u64) -> usize {
    match i {
        i if i < (1 << 8) => {
            b[0] = i as u8;
            1
        }
        i if i < (1 << 16) => {
            b[0] = (i >> 8) as u8;
            b[1] = i as u8;
            2
        }
        i if i < (1 << 24) => {
            b[0] = (i >> 16) as u8;
            b[1] = (i >> 8) as u8;
            b[2] = i as u8;
            3
        }
        i if i < (1 << 32) => {
            b[0] = (i >> 24) as u8;
            b[1] = (i >> 16) as u8;
            b[2] = (i >> 8) as u8;
            b[3] = i as u8;
            4
        }
        i if i < (1 << 40) => {
            b[0] = (i >> 32) as u8;
            b[1] = (i >> 24) as u8;
            b[2] = (i >> 16) as u8;
            b[3] = (i >> 8) as u8;
            b[4] = i as u8;
            5
        }
        i if i < (1 << 48) => {
            b[0] = (i >> 40) as u8;
            b[1] = (i >> 32) as u8;
            b[2] = (i >> 24) as u8;
            b[3] = (i >> 16) as u8;
            b[4] = (i >> 8) as u8;
            b[5] = i as u8;
            6
        }
        i if i < (1 << 56) => {
            b[0] = (i >> 48) as u8;
            b[1] = (i >> 40) as u8;
            b[2] = (i >> 32) as u8;
            b[3] = (i >> 24) as u8;
            b[4] = (i >> 16) as u8;
            b[5] = (i >> 8) as u8;
            b[6] = i as u8;
            7
        }
        _ => {
            b[0] = (i >> 56) as u8;
            b[1] = (i >> 48) as u8;
            b[2] = (i >> 40) as u8;
            b[3] = (i >> 32) as u8;
            b[4] = (i >> 24) as u8;
            b[5] = (i >> 16) as u8;
            b[6] = (i >> 8) as u8;
            b[7] = i as u8;
            8
        }
    }
}

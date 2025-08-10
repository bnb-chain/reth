//! Key encoding utilities for trie operations

/// Calculate the common prefix length between two byte arrays
pub fn common_prefix_length(a: &[u8], b: &[u8]) -> usize {
    let mut length = 0;
    let min_len = a.len().min(b.len());

    for i in 0..min_len {
        if a[i] != b[i] {
            break;
        }
        length += 1;
    }

    length
}

/// Convert a key to nibbles + terminator format
/// Equivalent to BSC's keybytesToHex function
pub fn key_to_nibbles(key: &[u8]) -> Vec<u8> {
    let l = key.len() * 2 + 1;
    let mut nibbles = vec![0u8; l];

    // Convert each byte to two nibbles
    for (i, &b) in key.iter().enumerate() {
        nibbles[i * 2] = b / 16;      // High nibble
        nibbles[i * 2 + 1] = b % 16;  // Low nibble
    }

    // Add terminator
    nibbles[l - 1] = 16;

    nibbles
}

/// Check if a nibble array has a terminator (value 16) at the end
pub fn has_terminator(nibbles: &[u8]) -> bool {
    !nibbles.is_empty() && nibbles[nibbles.len() - 1] == 16
}

/// Check if a hex key has the terminator flag (16).
/// This matches BSC's hasTerm function.
pub fn has_term(hex: &[u8]) -> bool {
    !hex.is_empty() && hex[hex.len() - 1] == 16
}

/// Convert hex-encoded key to compact encoding.
/// This matches BSC's hexToCompact function from trie/encoding.go.
///
/// Compact encoding is defined by the Ethereum Yellow Paper and contains:
/// - A flag byte indicating odd/even length and terminator presence
/// - The compressed nibbles
pub fn hex_to_compact(hex: &[u8]) -> Vec<u8> {
    let mut terminator = 0u8;
    let mut hex_copy = hex;

    // Check for terminator and remove it
    if has_term(hex) {
        terminator = 1;
        hex_copy = &hex[..hex.len() - 1];
    }

    let mut buf = vec![0u8; hex_copy.len() / 2 + 1];
    buf[0] = terminator << 5; // the flag byte (bit 5 for terminator)

    // Handle odd length
    if hex_copy.len() & 1 == 1 {
        buf[0] |= 1 << 4; // odd flag (bit 4)
        buf[0] |= hex_copy[0]; // first nibble is contained in the first byte
        hex_copy = &hex_copy[1..];
    }

    // Pack remaining nibbles
    decode_nibbles(hex_copy, &mut buf[1..]);
    buf
}

/// Convert compact encoding back to hex.
/// This matches BSC's compactToHex function exactly.
pub fn compact_to_hex(compact: &[u8]) -> Vec<u8> {
    if compact.is_empty() {
        return compact.to_vec();
    }

    let base = keybytes_to_hex(compact);

    // Delete terminator flag
    let base = if base[0] < 2 {
        base[..base.len() - 1].to_vec()
    } else {
        base
    };

    // Apply odd flag
    let chop = 2 - (base[0] & 1) as usize;
    base[chop..].to_vec()
}

/// Convert key bytes to hex encoding (with terminator).
/// This matches BSC's keybytesToHex function.
fn keybytes_to_hex(key: &[u8]) -> Vec<u8> {
    let l = key.len() * 2 + 1;
    let mut nibbles = vec![0u8; l];

    for (i, &b) in key.iter().enumerate() {
        nibbles[i * 2] = b / 16;      // High nibble (using division like BSC)
        nibbles[i * 2 + 1] = b % 16;  // Low nibble (using modulo like BSC)
    }

    nibbles[l - 1] = 16; // Terminator
    nibbles
}

/// Convert hex nibbles back to key bytes.
/// This matches BSC's hexToKeybytes function.
/// This can only be used for keys of even length.
pub fn hex_to_keybytes(hex: &[u8]) -> Vec<u8> {
    let mut hex_copy = hex;

    // Remove terminator if present
    if has_term(hex) {
        hex_copy = &hex[..hex.len() - 1];
    }

    // Check for even length
    if hex_copy.len() & 1 != 0 {
        panic!("can't convert hex key of odd length");
    }

    let mut key = vec![0u8; hex_copy.len() / 2];
    decode_nibbles(hex_copy, &mut key);
    key
}

/// Write hex key into the given slice, omitting the termination flag.
/// The dst slice must be at least 2x as large as the key.
/// This matches BSC's writeHexKey function.
pub fn write_hex_key<'a>(dst: &'a mut [u8], key: &[u8]) -> &'a [u8] {
    assert!(dst.len() >= 2 * key.len(), "dst slice too small");

    for (i, &b) in key.iter().enumerate() {
        dst[i * 2] = b / 16;      // High nibble
        dst[i * 2 + 1] = b % 16;  // Low nibble
    }

    &dst[..2 * key.len()]
}

/// Pack nibbles into bytes (helper function).
/// Each pair of nibbles becomes one byte.
fn decode_nibbles(nibbles: &[u8], buf: &mut [u8]) {
    for i in 0..buf.len() {
        if i * 2 + 1 < nibbles.len() {
            buf[i] = (nibbles[i * 2] << 4) | (nibbles[i * 2 + 1] & 0x0f);
        } else if i * 2 < nibbles.len() {
            buf[i] = nibbles[i * 2] << 4;
        }
    }
}

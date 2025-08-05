/// Trie keys are dealt with in three distinct encodings:
///
/// KEYBYTES encoding contains the actual key and nothing else. This encoding is the
/// input to most API functions.
///
/// HEX encoding contains one byte for each nibble of the key and an optional trailing
/// 'terminator' byte of value 0x10 which indicates whether or not the node at the key
/// contains a value. Hex key encoding is used for nodes loaded in memory because it's
/// convenient to access.
///
/// COMPACT encoding is defined by the Ethereum Yellow Paper (it's called "hex prefix
/// encoding" there) and contains the bytes of the key and a flag. The high nibble of the
/// first byte contains the flag; the lowest bit encoding the oddness of the length and
/// the second-lowest encoding whether the node at the key is a value node. The low nibble
/// of the first byte is zero in the case of an even number of nibbles and the first nibble
/// in the case of an odd number. All remaining nibbles (now an even number) fit properly
/// into the remaining bytes. Compact encoding is used for nodes stored on disk.

/// Convert hex encoding to compact encoding
pub fn hex_to_compact(hex: &[u8]) -> Vec<u8> {
    let mut terminator = 0u8;
    let mut hex = hex.to_vec();

    if has_term(&hex) {
        terminator = 1;
        hex.pop(); // remove terminator
    }

    let mut buf = vec![0u8; hex.len() / 2 + 1];
    buf[0] = terminator << 5; // the flag byte

    if hex.len() & 1 == 1 {
        buf[0] |= 1 << 4; // odd flag
        buf[0] |= hex[0]; // first nibble is contained in the first byte
        hex = hex[1..].to_vec();
    }

    decode_nibbles(&hex, &mut buf[1..]);
    buf
}

/// Convert compact encoding to hex encoding
pub fn compact_to_hex(compact: &[u8]) -> Vec<u8> {
    if compact.is_empty() {
        return compact.to_vec();
    }

    let mut base = keybytes_to_hex(compact);

    // delete terminator flag
    if base[0] < 2 {
        base.pop();
    }

    // apply odd flag
    let chop = 2 - (base[0] & 1);
    base[chop as usize..].to_vec()
}

/// Convert keybytes to hex encoding
pub fn keybytes_to_hex(str: &[u8]) -> Vec<u8> {
    let l = str.len() * 2 + 1;
    let mut nibbles = vec![0u8; l];

    for (i, &b) in str.iter().enumerate() {
        nibbles[i * 2] = b / 16;
        nibbles[i * 2 + 1] = b % 16;
    }

    nibbles[l - 1] = 16; // terminator
    nibbles
}

/// Convert hex encoding to keybytes
pub fn hex_to_keybytes(hex: &[u8]) -> Vec<u8> {
    if has_term(hex) {
        hex_to_keybytes_internal(&hex[..hex.len() - 1])
    } else {
        hex_to_keybytes_internal(hex)
    }
}

/// Internal function to convert hex to keybytes
fn hex_to_keybytes_internal(hex: &[u8]) -> Vec<u8> {
    if hex.len() & 1 != 0 {
        panic!("can't convert hex key of odd length into bytes");
    }

    let mut key = vec![0u8; hex.len() / 2];
    decode_nibbles(hex, &mut key);
    key
}

/// Decode nibbles into bytes
fn decode_nibbles(nibbles: &[u8], bytes: &mut [u8]) {
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = nibbles[2 * i] * 16 + nibbles[2 * i + 1];
    }
}

/// Find the length of the common prefix of two byte slices
pub fn prefix_len(a: &[u8], b: &[u8]) -> usize {
    let len = a.len().min(b.len());
    for i in 0..len {
        if a[i] != b[i] {
            return i;
        }
    }
    len
}

/// Check if a hex key has a terminator
pub fn has_term(s: &[u8]) -> bool {
    !s.is_empty() && s[s.len() - 1] == 16
}

/// Write hex key to destination buffer
pub fn write_hex_key<'a>(dst: &'a mut [u8], key: &[u8]) -> &'a [u8] {
    let hex = keybytes_to_hex(key);
    dst[..hex.len()].copy_from_slice(&hex);
    &dst[..hex.len()]
}

/// Concatenate byte slices
pub fn concat(s1: &[u8], s2: &[u8]) -> Vec<u8> {
    let mut result = Vec::with_capacity(s1.len() + s2.len());
    result.extend_from_slice(s1);
    result.extend_from_slice(s2);
    result
}

/// Concatenate multiple byte slices
pub fn concat_multiple(slices: &[&[u8]]) -> Vec<u8> {
    let total_len: usize = slices.iter().map(|s| s.len()).sum();
    let mut result = Vec::with_capacity(total_len);
    for slice in slices {
        result.extend_from_slice(slice);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hex_to_compact() {
        let hex = vec![1, 2, 3, 16]; // with terminator
        let compact = hex_to_compact(&hex);
        assert_eq!(compact[0] & 0x20, 0x20); // terminator flag set

        let hex_no_term = vec![1, 2, 3]; // without terminator
        let compact_no_term = hex_to_compact(&hex_no_term);
        assert_eq!(compact_no_term[0] & 0x20, 0); // terminator flag not set
    }

    #[test]
    fn test_compact_to_hex() {
        let compact = vec![0x20, 0x12, 0x34]; // with terminator
        let hex = compact_to_hex(&compact);
        assert_eq!(hex[hex.len() - 1], 16); // has terminator

        let compact_no_term = vec![0x00, 0x12, 0x34]; // without terminator
        let hex_no_term = compact_to_hex(&compact_no_term);
        assert_ne!(hex_no_term[hex_no_term.len() - 1], 16); // no terminator
    }

    #[test]
    fn test_keybytes_to_hex() {
        let key = vec![0x12, 0x34];
        let hex = keybytes_to_hex(&key);
        assert_eq!(hex, vec![1, 2, 3, 4, 16]); // terminator at end
    }

    #[test]
    fn test_hex_to_keybytes() {
        let hex = vec![1, 2, 3, 4, 16]; // with terminator
        let key = hex_to_keybytes(&hex);
        assert_eq!(key, vec![0x12, 0x34]);
    }

    #[test]
    fn test_prefix_len() {
        let a = vec![1, 2, 3, 4];
        let b = vec![1, 2, 5, 6];
        assert_eq!(prefix_len(&a, &b), 2);

        let c = vec![1, 2, 3];
        assert_eq!(prefix_len(&a, &c), 3);
    }

    #[test]
    fn test_has_term() {
        let with_term = vec![1, 2, 3, 16];
        assert!(has_term(&with_term));

        let without_term = vec![1, 2, 3];
        assert!(!has_term(&without_term));
    }

    #[test]
    fn test_concat() {
        let a = vec![1, 2];
        let b = vec![3, 4];
        let result = concat(&a, &b);
        assert_eq!(result, vec![1, 2, 3, 4]);
    }
}

// made by CasualArclamp
//! The file container: `"DS" | name_len | name | length (u32, big endian) |
//! data | CRC-32 of data`. The frames carry this stream, zero-padded to a
//! whole number of frames; the length says where it ends.

use decdsss_fec::crc::crc32;

const MAGIC: &[u8; 2] = b"DS";

/// Longest file name stored.
pub const NAME_MAX: usize = 100;

/// Wraps a file. The name is cut to `NAME_MAX` bytes and stripped of any
/// directory part.
pub fn pack(name: &str, data: &[u8]) -> Vec<u8> {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let mut name: Vec<u8> = base.bytes().take(NAME_MAX).collect();
    // Keep it valid UTF-8 if the cut fell inside a character.
    while std::str::from_utf8(&name).is_err() {
        name.pop();
    }
    let mut v = Vec::with_capacity(data.len() + name.len() + 11);
    v.extend_from_slice(MAGIC);
    v.push(name.len() as u8);
    v.extend_from_slice(&name);
    v.extend_from_slice(&(data.len() as u32).to_be_bytes());
    v.extend_from_slice(data);
    v.extend_from_slice(&crc32(data).to_be_bytes());
    v
}

/// Total bytes of the container whose first bytes are `head`, if `head` holds
/// enough of the front to say.
pub fn total_len(head: &[u8]) -> Option<usize> {
    if head.len() < 3 || &head[..2] != MAGIC {
        return None;
    }
    let n = usize::from(head[2]);
    let len: [u8; 4] = head.get(3 + n..7 + n)?.try_into().ok()?;
    Some(7 + n + u32::from_be_bytes(len) as usize + 4)
}

/// Unwraps a container (trailing padding is ignored). `None` if it is cut
/// short, not a container, or fails its CRC.
pub fn unpack(bytes: &[u8]) -> Option<(String, Vec<u8>)> {
    let total = total_len(bytes)?;
    let bytes = bytes.get(..total)?;
    let n = usize::from(bytes[2]);
    let name = std::str::from_utf8(&bytes[3..3 + n]).ok()?.to_string();
    let data = &bytes[7 + n..total - 4];
    let crc = u32::from_be_bytes(bytes[total - 4..].try_into().ok()?);
    (crc32(data) == crc).then(|| (name, data.to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_with_padding() {
        let data: Vec<u8> = (0..1000u32).map(|i| (i * 7) as u8).collect();
        let mut c = pack("pics/holiday.webp", &data);
        let len = c.len();
        c.extend_from_slice(&[0; 40]);
        assert_eq!(total_len(&c[..20]), Some(len));
        assert_eq!(unpack(&c), Some(("holiday.webp".to_string(), data)));
    }

    #[test]
    fn rejects_damage_and_truncation() {
        let c = pack("a.txt", b"hello world");
        let mut bad = c.clone();
        bad[10] ^= 1;
        assert_eq!(unpack(&bad), None);
        assert_eq!(unpack(&c[..c.len() - 1]), None);
        assert_eq!(unpack(b"XX\x00"), None);
        assert_eq!(total_len(b"DS"), None);
    }

    #[test]
    fn long_names_are_cut_on_a_character_boundary() {
        let name = "é".repeat(80); // 160 bytes
        let (n, _) = unpack(&pack(&name, b"x")).unwrap();
        assert!(n.len() <= NAME_MAX && n.chars().all(|c| c == 'é'));
    }
}

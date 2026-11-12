//! Room codes.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};

/// Letters and digits minus the easily confused ones (0/O, 1/I/L), so codes
/// survive being read aloud or retyped.
const ALPHABET: &[u8] = b"23456789ABCDEFGHJKMNPQRSTUVWXYZ";
pub const NEW_CODE_LEN: usize = 6;

/// Generate a random room code, e.g. "7KQ2XF".
///
/// Uses std's OS-seeded `RandomState` to avoid a dependency. That's fine for
/// picking a code, but codes aren't secrets: once there's a signaling server,
/// it should mint codes (and guarantee uniqueness) instead.
pub fn generate_code() -> String {
    let mut bits = RandomState::new().build_hasher().finish();
    (0..NEW_CODE_LEN)
        .map(|_| {
            if bits < ALPHABET.len() as u64 {
                bits = RandomState::new().build_hasher().finish();
            }
            let c = ALPHABET[(bits % ALPHABET.len() as u64) as usize];
            bits /= ALPHABET.len() as u64;
            c as char
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use mpvlink_proto::{MAX_ROOM_LEN as MAX_CODE_LEN, MIN_ROOM_LEN as MIN_CODE_LEN};

    #[test]
    fn codes_use_the_alphabet_and_are_joinable() {
        for _ in 0..1000 {
            let code = generate_code();
            assert_eq!(code.len(), NEW_CODE_LEN);
            assert!(code.bytes().all(|b| ALPHABET.contains(&b)), "{code}");
        }
        assert!((MIN_CODE_LEN..=MAX_CODE_LEN).contains(&NEW_CODE_LEN));
    }

    #[test]
    fn codes_vary() {
        let codes: std::collections::HashSet<_> = (0..100).map(|_| generate_code()).collect();
        assert!(codes.len() > 95);
    }
}

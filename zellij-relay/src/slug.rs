//! Random slug generation. Phase 1 uses 8 characters of base32-ish alphabet
//! (no ambiguous glyphs). Entropy is more than enough for a dev environment;
//! production hardening (collision retries, banned-word filtering) is out of
//! scope for this phase.

use rand::Rng;

const ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";
const SLUG_LEN: usize = 8;

pub fn generate() -> String {
    let mut rng = rand::thread_rng();
    (0..SLUG_LEN)
        .map(|_| ALPHABET[rng.gen_range(0..ALPHABET.len())] as char)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn charset_and_length_and_uniqueness() {
        let expected_alphabet: HashSet<char> = ALPHABET.iter().map(|b| *b as char).collect();
        let mut seen: HashSet<String> = HashSet::new();
        for _ in 0..10_000 {
            let slug = generate();
            assert_eq!(slug.chars().count(), SLUG_LEN);
            for ch in slug.chars() {
                assert!(expected_alphabet.contains(&ch), "{slug:?} contains {ch:?}");
            }
            assert!(seen.insert(slug.clone()), "duplicate slug: {slug:?}");
        }
    }
}

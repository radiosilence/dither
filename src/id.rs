//! Public identifiers for images, albums and uploads.
//!
//! Ten characters of base62 from the thread's CSPRNG: about 59.5 bits. Every
//! album is unlisted, so its address is the only thing keeping it from
//! strangers, and imgur's seven characters are short enough that people
//! enumerate them. At ten, a guesser making a thousand requests a second
//! against a million stored images expects a hit in about thirty years.

const ALPHABET: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
pub const LEN: usize = 10;

pub fn new() -> String {
    (0..LEN)
        .map(|_| char::from(ALPHABET[rand::random_range(0..ALPHABET.len())]))
        .collect()
}

/// Whether `s` could be an id: checked before any lookup, so a request for
/// anything else is answered without touching the database.
pub fn valid(s: &str) -> bool {
    s.len() == LEN && s.bytes().all(|b| b.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_valid_and_differ() {
        let a = new();
        assert!(valid(&a), "{a}");
        assert_ne!(a, new());
    }

    #[test]
    fn only_ids_are_valid() {
        assert!(valid("abcDEF1234"));
        assert!(!valid("abcDEF123"));
        assert!(!valid("abcDEF12345"));
        assert!(!valid("abc-EF1234"));
        assert!(!valid("../../etc1"));
        assert!(!valid("abcdéf1234"));
    }

    #[test]
    fn every_character_of_the_alphabet_turns_up() {
        let seen: std::collections::HashSet<char> = (0..2000)
            .flat_map(|_| new().chars().collect::<Vec<_>>())
            .collect();
        assert_eq!(seen.len(), 62);
    }
}

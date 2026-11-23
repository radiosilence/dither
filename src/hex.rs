//! Bytes as lower-case hex, and back.

pub fn encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A SHA-256 from its 64 hex digits.
pub fn decode32(text: &str) -> Option<[u8; 32]> {
    let text = text.as_bytes();
    if text.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, pair) in text.chunks(2).enumerate() {
        let digit = |c: u8| (c as char).to_digit(16);
        out[i] = u8::try_from(digit(pair[0])? * 16 + digit(pair[1])?).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    #[test]
    fn it_goes_there_and_back() {
        let bytes: [u8; 32] = std::array::from_fn(|i| (i * 9) as u8);
        assert_eq!(super::decode32(&super::encode(&bytes)), Some(bytes));
        assert_eq!(super::decode32("zz"), None);
        assert_eq!(super::decode32(&"g".repeat(64)), None);
    }
}

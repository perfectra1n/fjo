//! Base64, because wiki page content arrives that way.
//!
//! # Why this is hand-rolled
//!
//! `gea` has no base64 dependency, and `gitea-core` keeps its own fourteen-line encoder
//! private for HTTP Basic. Adding a crate to the tree — and to `deny.toml` — for the forty lines
//! below is a worse trade than writing them: the alphabet is fixed by RFC 4648 and cannot drift,
//! and the round-trip test below is the whole specification.
//!
//! # Why decoding is lenient
//!
//! [`decode`] returns the input unchanged when it is not valid base64, and that is deliberate.
//! The specification marks `WikiPage.content_base64` as encoded, but says **nothing** about
//! `sidebar` and `footer`, which Gitea also encodes — the field names carry no hint. A
//! future or patched instance that sends one of them as plain text must not make
//! `gea wiki view` fail; showing the text is strictly better than showing an error, and a page
//! whose body genuinely looks like base64 is decoded either way.

/// RFC 4648 standard alphabet.
const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Encode bytes as standard, padded base64.
pub fn encode(input: impl AsRef<[u8]>) -> String {
    let bytes = input.as_ref();
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 0x3f) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Decode base64 into text, or hand back the input when it is not base64.
///
/// See the module comment: the API does not reliably say which fields are encoded, so guessing
/// wrong in the *lenient* direction shows the user their page instead of an error.
pub fn decode(input: &str) -> String {
    match decode_bytes(input) {
        // Lossy rather than strict: a wiki page can contain any byte sequence someone committed,
        // and one invalid UTF-8 byte in a 40 KB page should cost a replacement character, not
        // the whole command.
        Some(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        None => input.to_owned(),
    }
}

/// The strict decoder. `None` when `input` is not valid standard base64.
///
/// Whitespace — including the newlines some encoders insert every 76 characters — is skipped,
/// and both the standard (`+/`) and URL-safe (`-_`) alphabets are accepted, because a value that
/// round-trips through a JSON document may have been produced by either.
pub fn decode_bytes(input: &str) -> Option<Vec<u8>> {
    let mut out: Vec<u8> = Vec::with_capacity(input.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    let mut padding = 0usize;

    for ch in input.chars() {
        if ch.is_ascii_whitespace() {
            continue;
        }
        if ch == '=' {
            padding += 1;
            continue;
        }
        // Padding is only ever trailing; a `=` in the middle means this is not base64 at all
        // (a query string, say), and accepting it would silently truncate the value.
        if padding > 0 {
            return None;
        }
        let value = match ch {
            'A'..='Z' => ch as u32 - 'A' as u32,
            'a'..='z' => ch as u32 - 'a' as u32 + 26,
            '0'..='9' => ch as u32 - '0' as u32 + 52,
            '+' | '-' => 62,
            '/' | '_' => 63,
            _ => return None,
        };
        acc = acc << 6 | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits & 0xff) as u8);
        }
    }
    // Leftover bits must be zero for the input to have been produced by an encoder; anything
    // else is a truncated or corrupt value.
    if bits >= 6 || (acc & ((1 << bits) - 1)) != 0 {
        return None;
    }
    // Base64 pads to at most two characters, and a value made only of padding is not an
    // encoding of the empty string — it is some other string that happens to end in `=`, and
    // decoding it to nothing would silently discard the user's content.
    if padding > 2 || (out.is_empty() && input.chars().any(|c| !c.is_ascii_whitespace())) {
        return None;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn padding_is_correct_for_every_input_length() {
        assert_eq!(encode(""), "");
        assert_eq!(encode("f"), "Zg==");
        assert_eq!(encode("fo"), "Zm8=");
        assert_eq!(encode("foo"), "Zm9v");
        assert_eq!(encode("foob"), "Zm9vYg==");
        assert_eq!(encode("fooba"), "Zm9vYmE=");
        assert_eq!(encode("foobar"), "Zm9vYmFy");
    }

    #[test]
    fn everything_encoded_decodes_back() {
        for text in [
            "",
            "#Heading\n\nA wiki page.\n",
            "unicode: 日本語 🚀",
            "a line\r\nwith crlf\r\n",
            "```rust\nfn main() {}\n```",
        ] {
            assert_eq!(decode(&encode(text)), text, "{text:?} did not round-trip");
        }
    }

    /// Bug this prevents — and the reason the decoder is lenient: the specification says
    /// `content_base64` is encoded but says nothing about `sidebar` and `footer`, which Gitea
    /// also encodes. If an instance ever sends one of those as plain text, `gea wiki view` must
    /// print the text rather than fail.
    #[test]
    fn text_that_is_not_base64_is_passed_through_unchanged() {
        for text in ["# Heading\n\nnot base64 at all", "hello, world!", "a?b=c&d=e", "==="] {
            assert_eq!(decode(text), text, "{text:?} should have been passed through");
        }
    }

    /// Bug this prevents: newline-wrapped base64 (what many encoders emit at 76 columns)
    /// failing the strict decode and then being shown to the user as base64 gibberish.
    #[test]
    fn wrapped_base64_still_decodes() {
        let long = "x".repeat(200);
        let wrapped: String = encode(&long)
            .as_bytes()
            .chunks(76)
            .map(|c| String::from_utf8_lossy(c).into_owned())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(wrapped.contains('\n'), "the fixture must actually wrap");
        assert_eq!(decode(&wrapped), long);
    }

    /// Bug this prevents: accepting a value that merely *looks* base64-ish and emitting
    /// mojibake. `decode_bytes` is strict so that `decode`'s fallback can be trusted.
    #[test]
    fn the_strict_decoder_rejects_non_base64() {
        assert!(decode_bytes("not base64!").is_none());
        assert!(decode_bytes("Zm9v=YmFy").is_none(), "padding in the middle");
        assert!(decode_bytes("Zg=").is_some(), "short padding is tolerated");
        assert!(decode_bytes("Z").is_none(), "six leftover bits cannot be a byte");
    }

    /// Base64 that arrived through a URL-safe encoder still decodes; the two alphabets differ
    /// only in the last two characters and nothing downstream cares which was used.
    #[test]
    fn the_url_safe_alphabet_is_accepted_too() {
        // 0xfb 0xff encodes as `+/8=` in standard and `-_8=` URL-safe.
        assert_eq!(decode_bytes("-_8=").unwrap(), decode_bytes("+/8=").unwrap());
    }
}

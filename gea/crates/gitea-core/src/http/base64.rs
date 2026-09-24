//! Standard base64, because the API carries data in it.
//!
//! # Why this is here rather than a dependency
//!
//! The alphabet is fixed by RFC 4648 §4 and cannot drift, the whole implementation is under a
//! hundred lines, and `gitea-core` already needed an encoder for HTTP Basic. A crate — plus a
//! `deny.toml` entry, plus a supply-chain surface — for that is a poor trade in a published
//! runtime whose selling point is a small dependency tree.
//!
//! # Why it is public
//!
//! Base64 is not an implementation detail of this crate; it is part of the **API's data model**.
//! `WikiPage.content_base64`, `CreateFileOptions.content`, `ChangeFileOperation.content` and
//! both avatar endpoints all carry base64 in a JSON body, so every consumer of the generated
//! client needs a codec to use them at all. Keeping it private forced `gea` to hand-roll a
//! second copy under `cmd/wiki/b64.rs`, with a module comment saying so.
//!
//! Both RFC 4648 alphabets are implemented, and the reason is worth recording because this
//! comment used to say the opposite. The standard alphabet (§4) is what the *spec* carries, and
//! that remained true. The URL-safe alphabet (§5) is required by PKCE — RFC 7636 §4.2 fixes the
//! `code_challenge` as unpadded base64url of a SHA-256 digest — and PKCE lives on Gitea's
//! OAuth2 endpoints, which are under the web root and so never appear in the spec at all. The
//! old reasoning was sound about the thing it was looking at; the OAuth flow is outside it.

/// RFC 4648 §4 standard alphabet.
const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// RFC 4648 §5 URL-safe alphabet. Identical but for the last two characters, `-_` in place of
/// `+/`, so that a value survives being put in a query string without percent-encoding.
const URL_ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Encode bytes as standard, padded base64.
pub fn encode(input: impl AsRef<[u8]>) -> String {
    encode_with(input.as_ref(), ALPHABET, Pad::Yes)
}

/// Encode bytes as URL-safe base64 with **no** `=` padding, per RFC 4648 §5.
///
/// This is the shape PKCE wants. RFC 7636 §4.2 defines the `code_challenge` as
/// `BASE64URL-ENCODE(SHA256(ASCII(code_verifier)))`, and §3 notes that the encoding there omits
/// padding. Sending the padded form is the classic PKCE bug: `=` percent-encodes to `%3D` in a
/// query string, the server compares the literal strings, and every exchange fails the challenge
/// with an error that says nothing about padding.
pub fn encode_url_nopad(input: impl AsRef<[u8]>) -> String {
    encode_with(input.as_ref(), URL_ALPHABET, Pad::No)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pad {
    Yes,
    No,
}

fn encode_with(bytes: &[u8], alphabet: &[u8; 64], pad: Pad) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(alphabet[(n >> (18 - 6 * i) & 0x3f) as usize] as char);
            } else if pad == Pad::Yes {
                out.push('=');
            }
        }
    }
    out
}

/// Decode standard base64, or `None` if `input` is not valid standard base64.
///
/// ASCII whitespace is ignored anywhere, because encoders that wrap at 76 columns are common
/// and a line break is not a decoding error. Padding is required to be trailing and to bring the
/// length to a multiple of four: a `=` in the middle means this was never base64, and accepting
/// it would turn "some text that happens to contain `=`" into plausible-looking bytes.
pub fn decode(input: &str) -> Option<Vec<u8>> {
    let clean: Vec<u8> = input.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if !clean.len().is_multiple_of(4) {
        return None;
    }
    let payload = match clean.iter().position(|&b| b == b'=') {
        // Padding must be the last one or two bytes and nothing but padding may follow.
        Some(at) if at + 2 < clean.len() || at + 4 <= clean.len() => return None,
        Some(at) if clean[at..].iter().any(|&b| b != b'=') => return None,
        Some(at) => &clean[..at],
        None => &clean[..],
    };

    let mut out = Vec::with_capacity(payload.len() / 4 * 3);
    let mut acc = 0u32;
    let mut bits = 0u32;
    for &b in payload {
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        acc = acc << 6 | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    // Leftover bits must be zero. `Zm9=` decodes to the same bytes as `Zm8=` under a sloppy
    // decoder, so two different strings would round-trip to one — data corruption, quietly.
    if bits > 0 && acc & ((1 << bits) - 1) != 0 {
        return None;
    }
    Some(out)
}

/// Decode to text, handing back the input **unchanged** when it is not base64 or not UTF-8.
///
/// # Why a lenient decoder exists at all
///
/// The spec marks `WikiPage.content_base64` as encoded but says nothing about `sidebar` and
/// `footer`, which Gitea also encodes — the field names carry no hint, and a future or patched
/// instance could send either as plain text. A viewer must not fail on that: showing the text is
/// strictly better than showing an error, and a body that genuinely is base64 decodes either
/// way. Use [`decode`] wherever a wrong guess would be worse than a failure.
pub fn decode_lenient(input: &str) -> String {
    match decode(input) {
        Some(bytes) => String::from_utf8(bytes).unwrap_or_else(|_| input.to_owned()),
        None => input.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 4648 §10's own vectors. Padding is where hand-rolled encoders go wrong.
    #[test]
    fn rfc_4648_test_vectors() {
        for (plain, encoded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(encode(plain), encoded, "encoding {plain:?}");
            assert_eq!(decode(encoded).as_deref(), Some(plain.as_bytes()), "decoding {encoded:?}");
        }
    }

    /// RFC 4648 §10 again, through the URL-safe alphabet and with padding suppressed. The two
    /// differences from the standard vectors above are the entire point of the function.
    #[test]
    fn url_safe_base64_drops_padding_and_uses_minus_and_underscore() {
        for (plain, encoded) in
            [("", ""), ("f", "Zg"), ("fo", "Zm8"), ("foo", "Zm9v"), ("foob", "Zm9vYg")]
        {
            assert_eq!(encode_url_nopad(plain), encoded, "encoding {plain:?}");
        }
        // 0xfb 0xff exercises both substituted characters: standard base64 gives `+/8=`.
        assert_eq!(encode([0xfb, 0xff]), "+/8=");
        assert_eq!(encode_url_nopad([0xfb, 0xff]), "-_8");
    }

    #[test]
    fn every_byte_round_trips() {
        let all: Vec<u8> = (0..=255u8).collect();
        for len in 0..=all.len() {
            let slice = &all[..len];
            assert_eq!(decode(&encode(slice)).as_deref(), Some(slice), "length {len}");
        }
    }

    /// Bug this prevents: newline-wrapped base64 — what many encoders emit at 76 columns — being
    /// rejected and then shown to the user as gibberish.
    #[test]
    fn wrapped_input_decodes() {
        let long = encode("x".repeat(200));
        let wrapped: String = long
            .as_bytes()
            .chunks(76)
            .map(|c| format!("{}\n", String::from_utf8_lossy(c)))
            .collect();
        assert_eq!(decode(&wrapped).unwrap(), "x".repeat(200).as_bytes());
    }

    #[test]
    fn text_that_is_not_base64_is_rejected() {
        for s in [
            "# Heading\n\nnot base64 at all",
            "hello, world!", // `!`, `,` and a space are outside the alphabet
            "a?b=c&d=e",     // `=` in the middle
            "Zm9v=Yg==",     // padding in the middle
            "Zm9",           // length not a multiple of 4
            "Zm9vYg=",       // under-padded
            "Zm9=",          // non-zero leftover bits: would alias with `Zm8=`
        ] {
            assert_eq!(decode(s), None, "{s:?} should not decode");
        }
    }

    /// The lenient path is what `wiki view` wants: never fail, just show something.
    #[test]
    fn lenient_decoding_passes_through_what_it_cannot_decode() {
        assert_eq!(decode_lenient("Zm9vYmFy"), "foobar");
        for s in ["# Heading\n\nnot base64 at all", "hello, world!", "==="] {
            assert_eq!(decode_lenient(s), s, "{s:?} should have been passed through");
        }
        // Valid base64 of bytes that are not UTF-8: the input comes back rather than a panic or
        // a string full of replacement characters.
        let latin1 = encode([0xff, 0xfe]);
        assert_eq!(decode_lenient(&latin1), latin1);
    }
}

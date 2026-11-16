//! TOTP (RFC 6238) with SHA-1, 6 digits, 30 s steps — the format every
//! authenticator app supports. Small enough to own: no dependency beyond sha1.

const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
const STEP: i64 = 30;

fn base32_encode(bytes: &[u8]) -> String {
    let mut out = String::new();
    let mut buf: u32 = 0;
    let mut bits = 0;
    for b in bytes {
        buf = buf << 8 | *b as u32;
        bits += 8;
        while bits >= 5 {
            out.push(ALPHABET[((buf >> (bits - 5)) & 31) as usize] as char);
            bits -= 5;
        }
    }
    if bits > 0 {
        out.push(ALPHABET[((buf << (5 - bits)) & 31) as usize] as char);
    }
    out
}

pub fn base32_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut buf: u32 = 0;
    let mut bits = 0;
    for c in s.bytes().filter(|c| *c != b'=' && *c != b' ') {
        let v = ALPHABET.iter().position(|a| a.eq_ignore_ascii_case(&c))? as u32;
        buf = buf << 5 | v;
        bits += 5;
        if bits >= 8 {
            out.push((buf >> (bits - 8)) as u8);
            bits -= 8;
        }
    }
    Some(out)
}

/// New random secret (160 bits, the RFC 4226 recommendation).
pub fn new_secret() -> String {
    let mut raw = [0u8; 20];
    getrandom::fill(&mut raw).expect("os rng");
    base32_encode(&raw)
}

fn code_at(secret: &[u8], counter: u64) -> u32 {
    use hmac::{Hmac, KeyInit, Mac};
    let mut mac = <Hmac<sha1::Sha1> as KeyInit>::new_from_slice(secret).expect("hmac accepts any key length");
    mac.update(&counter.to_be_bytes());
    let digest = mac.finalize().into_bytes();
    let off = (digest[19] & 0x0f) as usize;
    let bin = u32::from_be_bytes([digest[off], digest[off + 1], digest[off + 2], digest[off + 3]]) & 0x7fff_ffff;
    bin % 1_000_000
}

/// Current code (used by the tests; handy from a REPL).
#[cfg_attr(not(test), allow(dead_code))]
pub fn code(secret: &str, t: i64) -> Option<String> {
    let key = base32_decode(secret)?;
    Some(format!("{:06}", code_at(&key, t.div_euclid(STEP) as u64)))
}

/// Verify a 6-digit code, accepting the previous, current and next step
/// (±30 s clock skew).
pub fn verify(secret: &str, otp: &str, t: i64) -> bool {
    let otp = otp.trim().replace(' ', "");
    if otp.len() != 6 || !otp.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    let Some(key) = base32_decode(secret) else { return false };
    let step = t.div_euclid(STEP) as u64;
    [step.wrapping_sub(1), step, step + 1].iter().any(|c| format!("{:06}", code_at(&key, *c)) == otp)
}

pub fn otpauth_url(user: &str, secret: &str, issuer: &str) -> String {
    format!(
        "otpauth://totp/{issuer}:{user}?secret={secret}&issuer={issuer}&algorithm=SHA1&digits=6&period={STEP}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::now;

    #[test]
    fn rfc6238_vector() {
        // RFC 6238 appendix B, SHA-1, secret "12345678901234567890".
        let secret = base32_encode(b"12345678901234567890");
        assert_eq!(code(&secret, 59).unwrap(), "287082");
        assert_eq!(code(&secret, 1111111109).unwrap(), "081804");
    }

    #[test]
    fn verify_window() {
        let s = new_secret();
        let t = now();
        let c = code(&s, t).unwrap();
        assert!(verify(&s, &c, t));
        assert!(verify(&s, &c, t + STEP));
        assert!(!verify(&s, &c, t + 10 * STEP));
        assert!(!verify(&s, "12345", t));
        assert!(!verify(&s, "abcdef", t));
        assert!(!verify("not base32!", &c, t));
    }

    #[test]
    fn base32_roundtrip() {
        for len in [1usize, 5, 10, 20] {
            let b: Vec<u8> = (0..len).map(|i| i as u8 * 31 + 7).collect();
            assert_eq!(base32_decode(&base32_encode(&b)).unwrap(), b);
        }
    }
}

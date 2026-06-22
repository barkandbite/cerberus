//! Randomness for salts, nonces, instance IDs, and farbling seeds — the OS
//! CSPRNG via the audited `getrandom` crate (ADR-0033).
//!
//! This is the single entropy source for everything security-critical, so it
//! **fails closed**: if the OS RNG is unavailable it panics rather than falling
//! back to a predictable stream. A predictable salt/nonce/seed would defeat the
//! vault (AEAD nonce reuse), the seal (instance id), and unlinkability (farbling
//! seed) — the exact failures `SECURITY.md` puts in scope (issue #9).

/// `n` cryptographically-secure random bytes from the OS RNG.
///
/// Panics (fail-closed) if the OS CSPRNG cannot be read — better to abort than
/// to mint predictable key material. Every shipped platform exposes a real RNG
/// (`getrandom`: `getrandom`/`/dev/urandom` on Unix, `ProcessPrng`/`BCrypt` on
/// Windows, `getentropy` on macOS).
pub(crate) fn random_bytes(n: usize) -> Vec<u8> {
    let mut out = vec![0u8; n];
    getrandom::fill(&mut out)
        .expect("OS CSPRNG (getrandom) unavailable; refusing to produce predictable key material");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_bytes_are_fresh_per_call() {
        let a = random_bytes(24);
        let b = random_bytes(24);
        assert_eq!(a.len(), 24);
        // 24 random bytes colliding is beyond astronomically unlikely.
        assert_ne!(a, b);
    }

    #[test]
    fn random_bytes_are_not_all_zero() {
        // A trivial sanity check that we got real entropy, not a zeroed buffer.
        assert!(random_bytes(32).iter().any(|&b| b != 0));
    }
}

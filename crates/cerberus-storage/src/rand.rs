//! Randomness for nonces and salts — `/dev/urandom`, std-only.

use std::io::Read;

/// `n` random bytes from the OS CSPRNG.
///
/// Falls back to a SplitMix64 stream seeded from time/PID/address entropy on
/// platforms without `/dev/urandom`. The fallback is NOT cryptographically
/// strong; it exists so non-Unix dev builds run. Every platform we ship
/// (PLAN §8 targets) has a real entropy device, and XChaCha20's 24-byte nonce
/// space keeps even the fallback collision-safe for at-rest blob counts.
pub(crate) fn random_bytes(n: usize) -> Vec<u8> {
    let mut out = vec![0u8; n];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        if f.read_exact(&mut out).is_ok() {
            return out;
        }
    }
    let mut state = fallback_seed();
    for b in &mut out {
        state = splitmix64(state);
        *b = (state >> 56) as u8;
    }
    out
}

fn fallback_seed() -> u64 {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let pid = std::process::id() as u64;
    let stack_probe = &t as *const _ as u64;
    t ^ pid.rotate_left(32) ^ stack_probe
}

fn splitmix64(state: u64) -> u64 {
    let mut z = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
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
}

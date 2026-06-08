//! Per-instance, per-session deterministic noise injected into fingerprintable
//! surfaces (the Brave "farbling" model).
//!
//! Each identity carries its own seed, so the three heads do not correlate. The
//! goal is to stop a tracker building a stable cross-site identity of the active
//! head. This is randomize-*our-own-surface*, never impersonation of another
//! browser or device (see the threat model's non-goals).
//!
//! The perturbation is deterministic given `(seed, channel, index)` and bounded
//! to ±1 per byte, so output still renders correctly. The actual JS-side shims
//! (canvas, audio, WebGL, font metrics) are emitted by [`FarblingProvider::js_prologue`]
//! and injected via the `JsEngine` seam; the real shim bodies land at M6.

/// A fingerprintable surface that farbling perturbs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel {
    /// `canvas.toDataURL` / `getImageData`.
    Canvas,
    /// `AudioContext` sample data.
    Audio,
    /// WebGL `readPixels` / parameters.
    WebglReadPixels,
    /// Font metrics (`measureText`, bounding boxes).
    FontMetrics,
}

impl Channel {
    /// A stable per-channel tag mixed into the noise function.
    fn tag(self) -> u64 {
        match self {
            Channel::Canvas => 0x01,
            Channel::Audio => 0x02,
            Channel::WebglReadPixels => 0x03,
            Channel::FontMetrics => 0x04,
        }
    }
}

/// Supplies per-head fingerprint noise and the JS prologue that installs the
/// browser-side shims. One implementation per head (distinct seeds).
pub trait FarblingProvider: Send {
    /// The head's farbling seed.
    fn seed(&self) -> u64;

    /// Deterministically perturb one byte of a fingerprintable read. Bounded to
    /// ±1 so the surface still renders/sounds correct.
    fn perturb(&self, channel: Channel, index: u64, value: u8) -> u8;

    /// The JavaScript prologue installing the fingerprint shims for this head.
    /// Injected into each realm before page scripts run.
    fn js_prologue(&self) -> String;
}

/// Deterministic, seeded farbling using a SplitMix64 mixer.
#[derive(Clone, Copy, Debug)]
pub struct SeededFarbling {
    seed: u64,
}

impl SeededFarbling {
    /// Create a provider for a head's seed.
    pub fn new(seed: u64) -> Self {
        Self { seed }
    }
}

impl FarblingProvider for SeededFarbling {
    fn seed(&self) -> u64 {
        self.seed
    }

    fn perturb(&self, channel: Channel, index: u64, value: u8) -> u8 {
        let mixed = splitmix64(
            self.seed
                ^ channel.tag().wrapping_mul(0x9E37_79B9_7F4A_7C15)
                ^ index.wrapping_mul(0xD1B5_4A32_D192_ED03),
        );
        // Map to a delta in {-1, 0, +1}: mostly perturb, occasionally leave be.
        let delta: i8 = match mixed % 3 {
            0 => -1,
            1 => 0,
            _ => 1,
        };
        value.saturating_add_signed(delta)
    }

    fn js_prologue(&self) -> String {
        // Placeholder shim. Real canvas/audio/webgl/font-metrics hooks arrive at
        // M6; they call back into a per-realm noise source keyed by this seed.
        format!(
            "/* cerberus farbling (seed={:#018x}) — shims installed at M6 */\n",
            self.seed
        )
    }
}

/// SplitMix64 — a small, fast, well-distributed finalizer. Used only for
/// fingerprint noise, never for anything security-sensitive.
fn splitmix64(seed: u64) -> u64 {
    let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn perturbation_is_deterministic() {
        let f = SeededFarbling::new(0xABCD);
        for i in 0..64 {
            assert_eq!(
                f.perturb(Channel::Canvas, i, 128),
                f.perturb(Channel::Canvas, i, 128)
            );
        }
    }

    #[test]
    fn perturbation_is_bounded_so_output_still_renders() {
        let f = SeededFarbling::new(7);
        for v in 0u8..=255 {
            for i in 0..16 {
                let out = f.perturb(Channel::Canvas, i, v);
                assert!(out.abs_diff(v) <= 1, "delta too large at v={v}, i={i}");
            }
        }
    }

    #[test]
    fn two_heads_do_not_correlate() {
        let a = SeededFarbling::new(1);
        let b = SeededFarbling::new(2);
        let differing = (0..1024u64)
            .filter(|&i| a.perturb(Channel::Canvas, i, 128) != b.perturb(Channel::Canvas, i, 128))
            .count();
        // Distinct seeds must diverge across the surface (not be near-identical).
        assert!(differing > 256, "only {differing}/1024 samples differed");
    }
}

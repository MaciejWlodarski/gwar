//! Noise suppression shared by the desktop app (`gwar-client`, model embedded) and the web app
//! (`gwar-denoise-wasm`, model downloaded): DeepFilterNet3's low-latency model with the same
//! settings everywhere (docs/latency.md, "Noise suppression").

/// Samples per frame: 10 ms at 48 kHz, the model's hop.
pub const FRAME: usize = 480;

/// How much background noise to take out, in percent: 0 is off, 100 all the model can. In
/// between, no sound is taken down by more than half a dB per percent (50 % caps it at 25 dB).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NoiseSuppression(u8);

impl NoiseSuppression {
    pub const OFF: Self = Self(0);
    pub const FULL: Self = Self(100);

    /// Above 100 counts as 100.
    pub const fn percent(percent: u8) -> Self {
        Self(if percent > 100 { 100 } else { percent })
    }

    pub const fn as_percent(self) -> u8 {
        self.0
    }

    pub const fn is_off(self) -> bool {
        self.0 == 0
    }

    /// DeepFilterNet's attenuation limit; 100 dB or more is none.
    pub fn limit_db(self) -> f32 {
        if self.0 >= 100 { 100.0 } else { f32::from(self.0) * 0.5 }
    }
}

#[cfg(feature = "model")]
mod model;
#[cfg(feature = "model")]
pub use model::Model;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strength_is_a_percentage() {
        assert_eq!(NoiseSuppression::percent(250), NoiseSuppression::FULL);
        assert_eq!(NoiseSuppression::percent(50).limit_db(), 25.0);
        assert_eq!(NoiseSuppression::FULL.limit_db(), 100.0);
    }
}

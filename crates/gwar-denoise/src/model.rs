use anyhow::{Result, bail};
use df::tract::{DfParams, DfTract, RuntimeParams};

use crate::{FRAME, NoiseSuppression};

/// libDF's thresholds on its estimate of the local SNR (dB), as its own `deep-filter` command
/// sets them: below `MIN_DB` a frame is taken for noise alone and silenced, above `MAX_DB` for
/// clean speech and left alone. The library's defaults (-10, and 30 and 20 for its two stages)
/// leave more frames alone: with a quiet background, half the gain (docs/latency.md).
const MIN_DB: f32 = -15.0;
const MAX_DB: f32 = 35.0;
/// DeepFilterNet's post-filter (the command's `--pf`) at its default strength: takes noise
/// between words further down.
const POST_FILTER_BETA: f32 = 0.02;
/// libDF: an attenuation limit of 100 dB or more is none.
const NO_LIMIT: f32 = 100.0;

/// DeepFilterNet3's low-latency model with one stream's state. It adds one frame (10 ms) of
/// delay, its overlap-add, and looks nowhere ahead.
pub struct Model {
    /// The untouched model, to start over from.
    base: DfTract,
    live: DfTract,
    limit: f32,
    noisy: ndarray::Array2<f32>,
    clean: ndarray::Array2<f32>,
}

impl Model {
    /// The model embedded in the binary.
    #[cfg(feature = "embedded-model")]
    pub fn embedded() -> Result<Self> {
        Self::new(DfParams::default())
    }

    /// The model from its archive (`DeepFilterNet3_ll_onnx.tar.gz`).
    pub fn from_archive(archive: &[u8]) -> Result<Self> {
        Self::new(DfParams::from_bytes(archive)?)
    }

    fn new(params: DfParams) -> Result<Self> {
        let runtime = RuntimeParams::default_with_ch(1)
            .with_thresholds(MIN_DB, MAX_DB, MAX_DB)
            .with_post_filter(POST_FILTER_BETA);
        let base = DfTract::new(params, &runtime)?;
        if base.hop_size != FRAME {
            bail!("DeepFilterNet hop is {} samples, not {FRAME}", base.hop_size);
        }
        Ok(Self {
            live: base.clone(),
            base,
            limit: NO_LIMIT,
            noisy: ndarray::Array2::zeros((1, FRAME)),
            clean: ndarray::Array2::zeros((1, FRAME)),
        })
    }

    /// Starts over from the untouched model (a gap in the audio).
    pub fn reset(&mut self) {
        self.live = self.base.clone();
        self.limit = NO_LIMIT;
    }

    /// Cleans `frame` in place, as hard as `strength` says (not off).
    pub fn process(&mut self, frame: &mut [f32; FRAME], strength: NoiseSuppression) -> Result<()> {
        let limit = strength.limit_db();
        if limit != self.limit {
            self.live.set_atten_lim(limit);
            self.limit = limit;
        }
        self.noisy.row_mut(0).iter_mut().zip(frame.iter()).for_each(|(n, s)| *n = *s);
        self.live.process(self.noisy.view(), self.clean.view_mut())?;
        frame.iter_mut().zip(self.clean.row(0).iter()).for_each(|(s, c)| *s = c.clamp(-1.0, 1.0));
        Ok(())
    }
}

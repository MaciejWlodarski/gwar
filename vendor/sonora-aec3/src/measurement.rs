//! Fixed-size snapshots for isolated Gwar AEC experiments. No production tuning.
#[derive(Debug, Clone, Copy, Default)]
pub struct Measurement {
    pub render_channels: usize,
    pub delay_ms: i32,
    pub filter_delay_blocks: i32,
    pub erl_db: f64,
    pub erle_db: f64,
    pub reverb_decay: f32,
    pub initial: bool,
    pub linear_usable: bool,
    pub dominant_nearend: bool,
    pub transparent: bool,
    pub gain_mean: f32,
    pub gain_min: f32,
    pub gain_high: f32,
    pub erle_mean: f32,
    pub mic_power: f32,
    pub linear_power: f32,
    pub echo_power: f32,
    pub residual_power: f32,
}

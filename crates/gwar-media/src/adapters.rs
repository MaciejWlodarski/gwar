use crate::{AudioFrame, CodecProfile, DecodedFrame, EncodedFrame, MediaError, RawFrame, Result, Size};

/// Whether a capture source can draw the pointer into its output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorMode {
    /// Omit the pointer.
    Hidden,
    /// Composite the pointer into captured frames.
    Embedded,
}

/// Screen capture limits, discovered without starting a codec.
#[derive(Debug, Clone)]
pub struct SourceCaps {
    /// Maximum physical dimensions.
    pub max_size: Size,
    /// Maximum whole frames per second.
    pub max_fps: u32,
    /// Supported pointer modes.
    pub cursor_modes: Vec<CursorMode>,
    /// Source can capture HDR; conversion to SDR is the adapter's responsibility.
    pub hdr: bool,
}

/// Requested physical output size and frame-rate cap (both nonzero).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureConfig {
    /// Output size after adapter scaling/tone mapping.
    pub size: Size,
    /// Maximum output frames per second.
    pub max_fps: u32,
    /// Pointer treatment.
    pub cursor: CursorMode,
}

/// One supported profile and its limits; capabilities are measured, not inferred from codec names.
#[derive(Debug, Clone)]
pub struct CodecCaps {
    /// Supported codec configuration.
    pub profile: CodecProfile,
    /// Maximum coded physical dimensions for this configuration.
    pub max_size: Size,
    /// Maximum frames per second at the advertised size.
    pub max_fps: u32,
    /// True only for a verified hardware path.
    pub hardware: bool,
}

/// Encoder configurations currently available on this machine.
pub type EncoderCaps = Vec<CodecCaps>;
/// Decoder configurations verified on this machine, subject to the plan's resource gates.
pub type DecoderCaps = Vec<CodecCaps>;

/// Rate controller output for one encoder instance/layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateControl {
    /// Target encoded bits per second, excluding transport overhead; nonzero.
    pub bitrate_bps: u32,
    /// Maximum encoded frames per second; nonzero.
    pub max_fps: u32,
}

/// Initial codec settings; changing profile or size requires a new configuration and keyframe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncoderConfig {
    /// Codec profile/level to produce.
    pub profile: CodecProfile,
    /// Physical coded size.
    pub size: Size,
    /// Initial rate targets.
    pub rate: RateControl,
}

/// A screen/window/app selected through the platform picker, or a synthetic test source.
///
/// Start only while stopped. Stop is idempotent and joins/drains producers: no callback from the
/// old run may occur after it returns. Reconfiguration applies to subsequent capture, without
/// starting another run. Implementations validate configuration before changing state.
/// A sink failure ends production and is reported through `FrameSink::on_error`.
pub trait ScreenSource: Send {
    /// Discover size, fps, pointer and HDR limits.
    fn caps(&self) -> SourceCaps;
    /// Begin asynchronous delivery to an owned sink.
    fn start(&mut self, cfg: CaptureConfig, sink: Box<dyn FrameSink>) -> Result<()>;
    /// Change size/fps/pointer caps on a running source, preserving the timestamp epoch.
    fn reconfigure(&mut self, cfg: CaptureConfig) -> Result<()>;
    /// Stop and release the sink and platform capture resources.
    fn stop(&mut self);
}

/// Capture output. Callbacks are serialized by the producer; use a bounded downstream queue.
pub trait FrameSink: Send {
    /// Take ownership of one frame; an error terminates production.
    fn on_frame(&mut self, frame: RawFrame) -> Result<()>;
    /// Terminal asynchronous capture/output failure; called at most once per failed run.
    fn on_error(&mut self, error: MediaError);
}

/// Whether the platform has actually excluded Gwar's own playback from shared audio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exclusion {
    /// Own-process/process-tree playback is excluded and verified by the platform adapter.
    OwnProcessExcluded,
    /// Exclusion is unavailable or unverified; the caller must apply the plan's consent policy.
    NotExcluded,
}

/// System/application audio capture, separate from the voice engine.
///
/// Emits owned 48 kHz stereo blocks. Lifecycle and callback rules are the same as `ScreenSource`;
/// capture must never silently fall back from exclusion to the full system mix.
pub trait AudioSource: Send {
    /// Report actual exclusion support, not merely the requested configuration.
    fn exclusion(&self) -> Exclusion;
    /// Start a stopped source with an owned output sink.
    fn start(&mut self, sink: Box<dyn AudioSink>) -> Result<()>;
    /// Idempotently stop; no callback may occur after return.
    fn stop(&mut self);
}

/// Serialized 48 kHz stereo capture output; callbacks must not block the voice engine.
pub trait AudioSink: Send {
    /// Accept a captured block; an error terminates capture.
    fn on_audio(&mut self, frame: AudioFrame) -> Result<()>;
    /// Report a terminal asynchronous failure.
    fn on_error(&mut self, error: MediaError);
}

/// Control one encoder instance, including a platform-owned mobile encoder.
///
/// Higher layers route per-layer requests to the corresponding instance. Rejected rate changes
/// leave the prior settings active. Keyframe requests apply to the next accepted input picture.
pub trait EncoderControl: Send {
    /// Set nonzero bitrate/fps caps without recreating the transport.
    fn set_rate(&mut self, rate: RateControl) -> Result<()>;
    /// Request a random-access picture with required sequence/parameter headers.
    fn request_keyframe(&mut self) -> Result<()>;
}

/// Video encoding, independent of capture, SFrame and transport.
///
/// Start binds explicit output before accepting input. Preserve capture identity, expose true
/// keyframe/reference metadata, and emit complete access units in codec order. Stop drains or
/// discards pending work and fences callbacks. No adapter may substitute software AV1 encoding.
pub trait VideoEncoder: EncoderControl {
    /// Supported codec profiles and hardware limits.
    fn caps(&self) -> EncoderCaps;
    /// Configure a stopped encoder and attach its output.
    fn start(&mut self, cfg: EncoderConfig, sink: Box<dyn EncodedVideoSink>) -> Result<()>;
    /// Accept a frame; `keyframe` requests random access for this input picture.
    fn encode(&mut self, frame: RawFrame, keyframe: bool) -> Result<()>;
    /// Idempotently stop, fencing all output callbacks before return.
    fn stop(&mut self);
}

/// Complete encoded output, also the entry point for platform-owned mobile encoders.
///
/// This is before encryption/packetization. Queue with bounded memory; preserve dependency
/// metadata and capture timestamps. An error terminates the producer, reported via `on_error`.
pub trait EncodedVideoSink: Send {
    /// Accept one owned encoded access unit.
    fn on_encoded(&mut self, frame: EncodedFrame) -> Result<()>;
    /// Report a terminal asynchronous encoding/output failure.
    fn on_error(&mut self, error: MediaError);
}

/// Video decoding, independent of transport and presentation.
///
/// A slot/stream reassignment must reset before accepting a new random-access frame. Reset
/// discards references and queued pictures and fences old callbacks before return, retaining
/// configuration and sink. Stop additionally releases them. Preserve `FrameInfo` on output.
pub trait VideoDecoder: Send {
    /// Supported configurations; do not advertise unverified software/hardware capabilities.
    fn caps(&self) -> DecoderCaps;
    /// Configure a stopped decoder with explicit presentation output.
    fn start(&mut self, profile: CodecProfile, sink: Box<dyn VideoSink>) -> Result<()>;
    /// Accept one complete, decrypted access unit in decode order.
    fn decode(&mut self, frame: EncodedFrame) -> Result<()>;
    /// Discard reference state and fence pending output after a loss or source switch.
    fn reset(&mut self);
    /// Idempotently stop, fencing callbacks and releasing the sink.
    fn stop(&mut self);
}

/// Decoded video output. A renderer may enqueue for its UI/GPU thread without copying surfaces.
///
/// Presentation scheduling, slot identity and rejection of late/out-of-order frames belong to
/// the watch pipeline; this boundary preserves capture identity for that policy and measurement.
pub trait VideoSink: Send {
    /// Accept a decoded frame; return an error to terminate decoding on output failure.
    fn present(&mut self, frame: DecodedFrame) -> Result<()>;
    /// Report a terminal asynchronous decoding/presentation failure.
    fn on_error(&mut self, error: MediaError);
}

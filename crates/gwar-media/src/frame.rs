use std::{any::Any, sync::Arc, time::Duration};

use crate::{MediaError, Result};

/// Physical pixels, independent of a window's logical/DPI size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Size {
    /// Pixel columns; must be nonzero at adapter boundaries.
    pub width: u32,
    /// Pixel rows; must be nonzero at adapter boundaries.
    pub height: u32,
}

impl Size {
    pub(crate) fn validate(self) -> Result<()> {
        if self.width == 0 || self.height == 0 {
            return Err(MediaError::Invalid("frame size must be nonzero"));
        }
        Ok(())
    }
}

/// CPU plane layout; YUV is 8-bit 4:2:0 with ceil-divided chroma dimensions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// One plane of blue, green, red, alpha bytes (straight alpha).
    Bgra,
    /// Y plane followed by an interleaved UV plane.
    Nv12,
    /// Separate Y, U and V planes.
    I420,
}

/// Owned plane bytes. A clone shares storage; padding at the end of the last row is optional.
#[derive(Debug, Clone)]
pub struct Plane {
    /// Bytes including any padding between rows.
    pub data: Arc<[u8]>,
    /// Distance between row starts, in bytes.
    pub stride: usize,
}

/// Validated CPU planes; dimensions and strides cannot change after construction.
#[derive(Debug, Clone)]
pub struct CpuFrame {
    size: Size,
    format: PixelFormat,
    planes: Vec<Plane>,
}

impl CpuFrame {
    /// Checks plane count, strides, lengths and arithmetic, including odd-sized YUV frames.
    pub fn new(size: Size, format: PixelFormat, planes: Vec<Plane>) -> Result<Self> {
        size.validate()?;
        let w = usize::try_from(size.width).map_err(|_| MediaError::Invalid("width overflow"))?;
        let h = usize::try_from(size.height).map_err(|_| MediaError::Invalid("height overflow"))?;
        let cw = w.div_ceil(2);
        let ch = h.div_ceil(2);
        let layout = match format {
            PixelFormat::Bgra => vec![(w.checked_mul(4), h)],
            PixelFormat::Nv12 => vec![(Some(w), h), (cw.checked_mul(2), ch)],
            PixelFormat::I420 => vec![(Some(w), h), (Some(cw), ch), (Some(cw), ch)],
        };
        if planes.len() != layout.len() {
            return Err(MediaError::Invalid("wrong plane count"));
        }
        for (plane, (row_bytes, rows)) in planes.iter().zip(layout) {
            validate_plane(plane.data.len(), plane.stride, row_bytes, rows)?;
        }
        Ok(Self { size, format, planes })
    }

    /// Physical dimensions.
    pub fn size(&self) -> Size {
        self.size
    }

    /// Plane layout.
    pub fn format(&self) -> PixelFormat {
        self.format
    }

    /// Validated planes, in the order specified by the format.
    pub fn planes(&self) -> &[Plane] {
        &self.planes
    }
}

pub(crate) fn validate_plane(len: usize, stride: usize, row_bytes: Option<usize>, rows: usize) -> Result<()> {
    let row_bytes = row_bytes.ok_or(MediaError::Invalid("row size overflow"))?;
    let required = rows
        .checked_sub(1)
        .and_then(|r| r.checked_mul(stride))
        .and_then(|v| v.checked_add(row_bytes))
        .ok_or(MediaError::Invalid("plane size overflow"))?;
    if stride < row_bytes || len < required {
        return Err(MediaError::Invalid("plane is truncated or stride is too small"));
    }
    Ok(())
}

/// An owned platform surface, downcast by the matching adapter with `as_any`.
///
/// The implementation retains the native resource until the last `Arc` is dropped and enforces
/// the OS's thread/fence rules. Never pass a borrowed integer/pointer as if it owned a surface.
/// Platform crates supply concrete types; this crate imports no OS APIs and uses no unsafe code.
pub trait VideoSurface: Any + Send + Sync {
    /// Access the adapter's concrete surface wrapper without copying pixel data.
    fn as_any(&self) -> &dyn Any;
}

/// Shared CPU storage or an owned native surface for a zero-copy adapter path.
#[derive(Clone)]
pub enum VideoBuffer {
    /// Validated software-accessible planes.
    Cpu(CpuFrame),
    /// Native texture/sample-buffer wrapper and its physical dimensions.
    Surface {
        /// Physical dimensions of the retained resource.
        size: Size,
        /// Owned, thread-safe platform wrapper.
        surface: Arc<dyn VideoSurface>,
    },
}

impl VideoBuffer {
    /// Physical dimensions for encoder/renderer configuration.
    pub fn size(&self) -> Size {
        match self {
            Self::Cpu(frame) => frame.size(),
            Self::Surface { size, .. } => *size,
        }
    }
}

/// Identity and time preserved through encode, transport and decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameInfo {
    /// Monotonic capture time relative to the session's shared epoch.
    pub capture_ts: Duration,
    /// Monotonically increasing within one source run; not an RTP sequence number.
    pub frame_id: u64,
}

/// Captured SDR frame. GPU adapters own color conversion/tone mapping before this boundary.
#[derive(Clone)]
pub struct RawFrame {
    /// Capture identity; preserve this on encoded and decoded output.
    pub info: FrameInfo,
    /// Owned CPU planes or retained native surface.
    pub buffer: VideoBuffer,
}

/// Decoder output uses the same owned CPU/surface representation as capture.
pub type DecodedFrame = RawFrame;

/// Video codecs understood by the native media core. This does not promise encoder availability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    /// VP8.
    Vp8,
    /// VP9.
    Vp9,
    /// H.264/AVC.
    H264,
    /// AV1 (hardware encode only under the native codec policy).
    Av1,
}

/// Negotiated codec configuration; H.264 retains constraint bits as well as profile and level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecProfile {
    /// VP8's single profile.
    Vp8,
    /// VP9 profile number.
    Vp9 {
        /// Profile number from the codec bitstream.
        profile: u8,
    },
    /// H.264's three `profile-level-id` bytes.
    H264 {
        /// SPS profile identifier.
        profile_idc: u8,
        /// SPS constraint/compatibility byte.
        profile_iop: u8,
        /// SPS level identifier.
        level_idc: u8,
    },
    /// AV1 sequence parameters used by capability negotiation.
    Av1 {
        /// Sequence profile.
        profile: u8,
        /// Sequence level index.
        level_idx: u8,
        /// High tier if true; main tier otherwise.
        high_tier: bool,
        /// Coded bit depth.
        bit_depth: u8,
    },
}

impl CodecProfile {
    /// Codec associated with this profile.
    pub fn codec(self) -> Codec {
        match self {
            Self::Vp8 => Codec::Vp8,
            Self::Vp9 { .. } => Codec::Vp9,
            Self::H264 { .. } => Codec::H264,
            Self::Av1 { .. } => Codec::Av1,
        }
    }
}

/// Reference information from the encoder, before encryption/packetization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameDependencies {
    /// No other picture is required: a true random-access frame, not merely an intra picture.
    Independent,
    /// The adapter cannot expose dependencies. Never invent SVC/DD metadata from this.
    Unknown,
    /// Frame IDs referenced by this picture, within this encoder stream.
    Known {
        /// Referenced `FrameInfo::frame_id`s (not transport-rewritten IDs).
        references: Vec<u64>,
    },
}

/// Optional layer metadata, available on random-access pictures as well as delta pictures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayerInfo {
    /// Zero-based spatial layer.
    pub spatial_id: u8,
    /// Zero-based temporal layer.
    pub temporal_id: u8,
    /// Adapter-produced AV1 Dependency Descriptor RTP extension, when available.
    /// The transport must map its frame numbers and templates before sending it.
    pub dependency_descriptor: Option<Arc<[u8]>>,
}

/// One complete encoded access unit, before SFrame, RTP packetization or after their reversal.
#[derive(Debug, Clone)]
pub struct EncodedFrame {
    /// Capture timestamp and identity carried through the codec.
    pub info: FrameInfo,
    /// Codec of this access unit.
    pub codec: Codec,
    /// VP8/VP9 frame bytes, H.264 Annex B (SPS/PPS with every IDR), or an AV1 temporal unit.
    /// B2 defines encryption canonicalization; this type does not perform it.
    pub data: Arc<[u8]>,
    /// Random-access status and available reference metadata.
    pub dependencies: FrameDependencies,
    /// Layer/Dependency Descriptor metadata, including on keyframes.
    pub layer: Option<LayerInfo>,
}

impl EncodedFrame {
    /// True only when decoding can begin without preceding pictures.
    pub fn is_keyframe(&self) -> bool {
        self.dependencies == FrameDependencies::Independent
    }
}

/// Owned interleaved 48 kHz stereo f32 audio, with the same epoch as video capture.
#[derive(Debug, Clone)]
pub struct AudioFrame {
    /// Capture time of the first stereo sample pair.
    pub capture_ts: Duration,
    samples: Arc<[f32]>,
}

impl AudioFrame {
    /// Rejects incomplete stereo pairs, empty blocks and non-finite samples.
    pub fn new(capture_ts: Duration, samples: Arc<[f32]>) -> Result<Self> {
        if samples.is_empty() || !samples.len().is_multiple_of(2) || samples.iter().any(|s| !s.is_finite()) {
            return Err(MediaError::Invalid("invalid stereo audio block"));
        }
        Ok(Self { capture_ts, samples })
    }

    /// Interleaved left/right samples at 48 kHz; nominal range is -1 to 1.
    pub fn samples(&self) -> &[f32] {
        &self.samples
    }
}

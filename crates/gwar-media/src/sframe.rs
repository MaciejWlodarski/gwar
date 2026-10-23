//! Codec byte layouts for RFC 9605 screen video. Keys and replay/counter lifetimes belong
//! to the caller (`gwar-client` reuses its voice key state in a separate screen instance).
//! No RTP payload descriptor is included. H.264/AV1 need their own future layouts.

use sframe::{
    frame::{EncryptedFrameView, FrameCounter, MediaFrameView},
    header::SframeHeader,
    key::{DecryptionKey, EncryptionKey},
};

/// Maximum encoded plaintext access unit (2 MiB), checked before allocation or encryption.
pub const MAX_FRAME_BYTES: usize = 2 * 1024 * 1024;
/// Maximum wire frame: plaintext bound plus the widest RFC header and GCM tag.
pub const MAX_WIRE_BYTES: usize = MAX_FRAME_BYTES + 17 + 16;

/// Screen codec layout; codec changes in one run must share the same counter/replay state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum VideoLayout {
    /// `header || ciphertext || tag`, with `header` as AAD.
    Vp9,
    /// `clear || header || ciphertext || tag`, with `header || clear` as AAD.
    Vp8,
}

/// A malformed/oversize frame, or failed authentication. No unauthenticated payload is returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvalidFrame;

impl std::fmt::Display for InvalidFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("invalid screen SFrame")
    }
}
impl std::error::Error for InvalidFrame {}

impl VideoLayout {
    /// Clear bytes: none for VP9; VP8 uses its frame-tag bit 0 (0 = key, 1 = delta).
    /// This is a byte split, not a codec validator: clear bytes are preserved exactly.
    pub fn clear_len(self, frame: &[u8]) -> Result<usize, InvalidFrame> {
        let length = match self {
            Self::Vp9 => 0,
            Self::Vp8 => match frame.first().ok_or(InvalidFrame)? & 1 {
                0 => 10,
                _ => 3,
            },
        };
        (frame.len() >= length).then_some(length).ok_or(InvalidFrame)
    }

    /// Seal a complete encoded frame with an already-derived, one-directional SFrame key.
    /// The caller must never reuse a counter under that key (including across codec changes).
    /// Empty VP9 and prefix-only VP8 are valid byte layouts; decoder validity is separate.
    pub fn seal(
        self,
        key: &EncryptionKey,
        counter: &mut impl FrameCounter,
        plaintext: &[u8],
    ) -> Result<Vec<u8>, InvalidFrame> {
        if plaintext.len() > MAX_FRAME_BYTES {
            return Err(InvalidFrame);
        }
        let clear = self.clear_len(plaintext)?;
        let (metadata, payload) = plaintext.split_at(clear);
        let frame = MediaFrameView::try_with_meta_data(counter, payload, metadata).map_err(|_| InvalidFrame)?;
        let encrypted = frame.encrypt(key).map_err(|_| InvalidFrame)?;
        Ok(encrypted.as_ref().to_vec())
    }

    /// Parse a bounded wire frame before key lookup, rejecting non-minimal RFC headers.
    /// Prefix bytes are untrusted until [`VideoFrame::open`] authenticates them.
    pub fn parse(self, wire: &[u8]) -> Result<VideoFrame<'_>, InvalidFrame> {
        if wire.len() > MAX_WIRE_BYTES {
            return Err(InvalidFrame);
        }
        let clear = self.clear_len(wire)?;
        let (metadata, encrypted) = wire.split_at(clear);
        let header = SframeHeader::deserialize(encrypted).map_err(|_| InvalidFrame)?;
        let canonical = SframeHeader::new(header.key_id(), header.counter());
        let mut bytes = [0; 17];
        canonical.serialize(&mut bytes[..canonical.len()]).map_err(|_| InvalidFrame)?;
        if encrypted.get(..canonical.len()) != Some(&bytes[..canonical.len()])
            || encrypted.len() < canonical.len() + 16
            || wire.len() - canonical.len() - 16 > MAX_FRAME_BYTES
        {
            return Err(InvalidFrame);
        }
        let view = EncryptedFrameView::try_with_meta_data(encrypted, metadata).map_err(|_| InvalidFrame)?;
        Ok(VideoFrame { view })
    }

    /// Authenticate and open one frame. The caller supplies replay protection separately.
    pub fn open(self, key: &DecryptionKey, wire: &[u8]) -> Result<Vec<u8>, InvalidFrame> {
        self.parse(wire)?.open(key)
    }
}

/// A bounded, structurally valid wire frame awaiting authentication and replay admission.
pub struct VideoFrame<'a> {
    view: EncryptedFrameView<'a>,
}

impl VideoFrame<'_> {
    /// Unauthenticated RFC header, for bounded key lookup and replay pre-screening only.
    pub fn header(&self) -> &SframeHeader {
        self.view.header()
    }

    /// Authenticate the header and clear prefix; return the complete encoded access unit.
    pub fn open(&self, key: &DecryptionKey) -> Result<Vec<u8>, InvalidFrame> {
        self.view.decrypt(key).map(|frame| frame.as_ref().to_vec()).map_err(|_| InvalidFrame)
    }
}

#[cfg(test)]
mod tests;

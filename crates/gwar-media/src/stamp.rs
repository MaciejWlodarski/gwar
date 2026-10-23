//! Loopback capture-time barcode, inspired by screen-share spike S3.
//!
//! The top quarter contains 32 equal-width black/white cells, MSB first: a 4-bit `0xa` marker,
//! 20 low bits of capture milliseconds, then CRC-8 (polynomial 0x07, initial 0) over the three
//! marker/payload bytes. Black is one. Unlike the browser e2e counter barcode, this measures
//! time. Its period is 2^20 ms; interpret only loopback delays below half that period.
//! Normalized cells survive resizing. Write at >=128x32; read at >=64x16, including after 0.5x
//! scaling. Readback supports BGRA, NV12 and I420 (YUV uses the luma plane, including limited range).

use std::time::Duration;

use crate::{CpuFrame, MediaError, PixelFormat, Result, Size, frame::validate_plane};

const MASK: u32 = (1 << 20) - 1;
/// Milliseconds before the 20-bit timestamp wraps (about 17.5 minutes).
pub const PERIOD_MS: u32 = MASK + 1;

/// Capture milliseconds modulo 2^20. Keep full `FrameInfo` timestamps outside the image too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameStamp(u32);

impl FrameStamp {
    /// Encode a monotonic capture timestamp relative to the pipeline's shared epoch.
    pub fn from_capture_time(capture_ts: Duration) -> Self {
        Self((capture_ts.as_millis() & u128::from(MASK)) as u32)
    }

    /// Low 20 capture-millisecond bits, without marker or checksum.
    pub fn millis(self) -> u32 {
        self.0
    }

    /// Loopback age in whole milliseconds, rejecting future/ambiguous stamps.
    ///
    /// Capture and `now` must share the same epoch. Delays of half a period or more cannot be
    /// distinguished from future frames and return `None`; this is not clock synchronization.
    pub fn age_at(self, now: Duration) -> Option<Duration> {
        let elapsed = (Self::from_capture_time(now).0.wrapping_sub(self.0)) & MASK;
        (elapsed < PERIOD_MS / 2).then(|| Duration::from_millis(u64::from(elapsed)))
    }

    fn word(self) -> u32 {
        let prefix = (0xa << 20) | self.0;
        (prefix << 8) | u32::from(crc(&prefix.to_be_bytes()[1..]))
    }

    fn from_word(word: u32) -> Result<Self> {
        let prefix = word >> 8;
        if prefix >> 20 != 0xa || crc(&prefix.to_be_bytes()[1..]) != word as u8 {
            return Err(MediaError::Invalid("invalid stamp marker or checksum"));
        }
        Ok(Self(prefix & MASK))
    }
}

fn crc(bytes: &[u8]) -> u8 {
    let mut value = 0u8;
    for byte in bytes {
        value ^= byte;
        for _ in 0..8 {
            value = if value & 0x80 != 0 { (value << 1) ^ 0x07 } else { value << 1 };
        }
    }
    value
}

/// Draw a capture-time barcode over the top quarter of an existing mutable BGRA image.
/// Validates the entire layout before writing, preserves stride padding and sets alpha to 255.
pub fn write_bgra(size: Size, stride: usize, data: &mut [u8], stamp: FrameStamp) -> Result<()> {
    size.validate()?;
    if size.width < 128 || size.height < 32 {
        return Err(MediaError::Invalid("stamp writer needs at least 128x32"));
    }
    let w = size.width as usize;
    let h = size.height as usize;
    validate_plane(data.len(), stride, w.checked_mul(4), h)?;
    let word = stamp.word();
    for cell in 0..32 {
        let color = if word & (1 << (31 - cell)) != 0 { 0 } else { 255 };
        for y in 0..h / 4 {
            for x in cell * w / 32..(cell + 1) * w / 32 {
                data[y * stride + x * 4..y * stride + x * 4 + 4].copy_from_slice(&[color, color, color, 255]);
            }
        }
    }
    Ok(())
}

/// Read a stamp from a captured/decoded CPU image, rejecting low contrast or a bad marker/CRC.
/// Samples cell interiors rather than edges so resampling and YUV range conversion are tolerated.
pub fn read(frame: &CpuFrame) -> Result<FrameStamp> {
    let size = frame.size();
    if size.width < 64 || size.height < 16 {
        return Err(MediaError::Invalid("stamp reader needs at least 64x16"));
    }
    let w = size.width as usize;
    let h = size.height as usize;
    let plane = &frame.planes()[0];
    let mut word = 0u32;
    for cell in 0..32 {
        let left = cell * w / 32;
        let right = (cell + 1) * w / 32;
        let inset = (right - left) / 4;
        let mut sum = 0u64;
        let mut count = 0u64;
        for y in h / 16..(3 * h / 16).max(h / 16 + 1) {
            for x in left + inset..right - inset {
                let offset = y * plane.stride;
                sum += match frame.format() {
                    PixelFormat::Bgra => {
                        let pixel = &plane.data[offset + x * 4..offset + x * 4 + 3];
                        (u64::from(pixel[0]) + u64::from(pixel[1]) + u64::from(pixel[2])) / 3
                    }
                    PixelFormat::Nv12 | PixelFormat::I420 => u64::from(plane.data[offset + x]),
                };
                count += 1;
            }
        }
        let mean = sum / count;
        let bit = if mean <= 96 {
            1
        } else if mean >= 160 {
            0
        } else {
            return Err(MediaError::Invalid("stamp has insufficient contrast"));
        };
        word = (word << 1) | bit;
    }
    FrameStamp::from_word(word)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Plane;

    fn image(size: Size, millis: u64) -> CpuFrame {
        let stride = size.width as usize * 4 + 12;
        let mut data = vec![73; stride * size.height as usize];
        write_bgra(size, stride, &mut data, FrameStamp::from_capture_time(Duration::from_millis(millis))).unwrap();
        assert_eq!(data[size.width as usize * 4], 73, "row padding changed");
        CpuFrame::new(size, PixelFormat::Bgra, vec![Plane { data: data.into(), stride }]).unwrap()
    }

    // Independent box-filter downsampling and nearest-neighbor upsampling of the whole image.
    fn resize(source: &CpuFrame, twice: bool) -> CpuFrame {
        let size = source.size();
        let factor = if twice { 2 } else { 1 };
        let divisor = if twice { 1 } else { 2 };
        let out = Size { width: size.width * factor / divisor, height: size.height * factor / divisor };
        let stride = out.width as usize * 4;
        let mut data = vec![0; stride * out.height as usize];
        let input = &source.planes()[0];
        for y in 0..out.height as usize {
            for x in 0..out.width as usize {
                for c in 0..4 {
                    let value = if twice {
                        u16::from(input.data[(y / 2) * input.stride + (x / 2) * 4 + c])
                    } else {
                        let mut sum = 0u16;
                        for dy in 0..2 {
                            for dx in 0..2 {
                                sum += u16::from(input.data[(y * 2 + dy) * input.stride + (x * 2 + dx) * 4 + c]);
                            }
                        }
                        sum / 4
                    };
                    data[y * stride + x * 4 + c] = value as u8;
                }
            }
        }
        CpuFrame::new(out, PixelFormat::Bgra, vec![Plane { data: data.into(), stride }]).unwrap()
    }

    #[test]
    fn checksum_vector_and_all_single_bit_corruptions() {
        assert_eq!(crc(b"123456789"), 0xf4); // standard CRC-8 check vector
        for value in [0, 1, 0x55555, 0xaaaaa, MASK] {
            let stamp = FrameStamp(value);
            assert_eq!(FrameStamp::from_word(stamp.word()), Ok(stamp));
            for bit in 0..32 {
                assert!(FrameStamp::from_word(stamp.word() ^ (1 << bit)).is_err());
            }
        }
    }

    #[test]
    fn stamp_survives_half_and_double_scaling_with_padded_and_odd_rows() {
        for size in [Size { width: 128, height: 32 }, Size { width: 642, height: 362 }] {
            for millis in [0, 1, 255, 65535, 0x55555, 0xaaaaa, u64::from(MASK), u64::from(PERIOD_MS) + 37] {
                let image = image(size, millis);
                let expected = FrameStamp::from_capture_time(Duration::from_millis(millis));
                assert_eq!(read(&image), Ok(expected));
                assert_eq!(read(&resize(&image, false)), Ok(expected));
                assert_eq!(read(&resize(&image, true)), Ok(expected));
            }
        }
    }

    #[test]
    fn limited_range_yuv_readback_and_corrupt_images() {
        let bgra = image(Size { width: 128, height: 32 }, 123456);
        let y: Vec<u8> = (0..32)
            .flat_map(|row| {
                let bgra = &bgra;
                (0..128).map(
                    move |x| if bgra.planes()[0].data[row * bgra.planes()[0].stride + x * 4] == 0 { 16 } else { 235 },
                )
            })
            .collect();
        for format in [PixelFormat::Nv12, PixelFormat::I420] {
            let mut planes = vec![Plane { data: y.clone().into(), stride: 128 }];
            if format == PixelFormat::Nv12 {
                planes.push(Plane { data: vec![128; 128 * 16].into(), stride: 128 });
            } else {
                planes.extend((0..2).map(|_| Plane { data: vec![128; 64 * 16].into(), stride: 64 }));
            }
            let frame = CpuFrame::new(bgra.size(), format, planes).unwrap();
            assert_eq!(read(&frame).unwrap().millis(), 123456);
        }
        for gray in [0, 128, 255] {
            let frame = CpuFrame::new(
                bgra.size(),
                PixelFormat::Bgra,
                vec![Plane { data: vec![gray; 128 * 32 * 4].into(), stride: 128 * 4 }],
            )
            .unwrap();
            assert!(read(&frame).is_err());
        }
        let mut corrupt = bgra.planes()[0].data.to_vec();
        // Flip one complete payload cell, including its sampling interior.
        for row in 0..8 {
            for x in 32..36 {
                for c in 0..3 {
                    corrupt[row * bgra.planes()[0].stride + x * 4 + c] ^= 255;
                }
            }
        }
        assert!(
            read(
                &CpuFrame::new(
                    bgra.size(),
                    PixelFormat::Bgra,
                    vec![Plane { data: corrupt.into(), stride: bgra.planes()[0].stride }]
                )
                .unwrap()
            )
            .is_err()
        );
    }

    #[test]
    fn wraparound_and_ambiguous_ages() {
        let capture = Duration::from_millis(u64::from(PERIOD_MS) - 3);
        let stamp = FrameStamp::from_capture_time(capture);
        assert_eq!(stamp.age_at(capture + Duration::from_millis(10)), Some(Duration::from_millis(10)));
        assert_eq!(stamp.age_at(capture - Duration::from_millis(1)), None);
        assert_eq!(stamp.age_at(capture + Duration::from_millis(u64::from(PERIOD_MS / 2))), None);
    }

    #[test]
    fn bad_writer_layout_is_rejected_before_mutation() {
        let mut data = vec![42; 128 * 32 * 4];
        let before = data.clone();
        assert!(write_bgra(Size { width: 128, height: 32 }, usize::MAX, &mut data, FrameStamp(0)).is_err());
        assert_eq!(data, before);
        assert!(write_bgra(Size { width: 127, height: 32 }, 127 * 4, &mut data, FrameStamp(0)).is_err());
        assert!(write_bgra(Size { width: 128, height: 32 }, 128 * 4, &mut data[..10], FrameStamp(0)).is_err());
    }
}

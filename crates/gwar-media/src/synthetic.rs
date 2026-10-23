use std::{
    sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crate::{
    CaptureConfig, CpuFrame, CursorMode, FrameInfo, FrameSink, MediaError, PixelFormat, Plane, RawFrame, Result,
    ScreenSource, Size, SourceCaps, VideoBuffer,
    stamp::{self, FrameStamp},
};

const MAX_SIZE: Size = Size { width: 3840, height: 2160 };
const MAX_FPS: u32 = 240;

/// Screen-like BGRA fixture: scrolling bitmap text, a moving block and capture-time barcode.
///
/// `render` is deterministic and unpaced for codec tests/benchmarks. `ScreenSource::start` owns
/// one paced worker; it skips missed deadlines rather than bursting to catch up. Stop wakes the
/// worker and joins it, including any in-progress callback. Drop also stops it. A sink must not
/// synchronously call back into this source (including stop). No OS/codec/font dependencies.
pub struct SyntheticScreenSource {
    epoch: Instant,
    worker: Option<Worker>,
}

struct Worker {
    commands: SyncSender<Command>,
    thread: JoinHandle<()>,
}

enum Command {
    Configure(CaptureConfig, SyncSender<()>),
    Stop,
}

impl SyntheticScreenSource {
    /// Use the same monotonic epoch as the receiving loopback measurement pipeline.
    pub fn new(epoch: Instant) -> Self {
        Self { epoch, worker: None }
    }

    /// Produce one frame without sleeping. Capture time drives motion; frame ID is preserved.
    /// Supports 128x32 through 3840x2160, 1–240 fps, hidden or embedded synthetic pointer.
    pub fn render(cfg: CaptureConfig, capture_ts: Duration, frame_id: u64) -> Result<RawFrame> {
        validate(cfg)?;
        let w = cfg.size.width as usize;
        let h = cfg.size.height as usize;
        let stride = w * 4;
        let mut data = vec![0; stride * h];
        for pixel in data.as_chunks_mut::<4>().0 {
            pixel.copy_from_slice(&[32, 28, 24, 255]);
        }
        // Integer motion from the shared clock, independent of requested frame rate.
        let millis = capture_ts.as_millis();
        let scroll = (millis / 80 % 24) as usize;
        let text = b"GWAR NATIVE MEDIA 0123456789";
        for baseline in (h / 4..h + 24).step_by(24) {
            for (column, letter) in text.iter().cycle().take(w.div_ceil(12)).enumerate() {
                for (x, bits) in glyph(*letter).iter().enumerate() {
                    for y in 0..7 {
                        let px = column * 12 + x * 2;
                        let py = baseline + y * 2;
                        if bits & (1 << y) != 0 && py >= scroll && py - scroll >= h / 4 {
                            rect(&mut data, cfg.size, px, py - scroll, 2, 2, [205, 214, 222, 255]);
                        }
                    }
                }
            }
        }
        let block_w = (w / 8).max(8);
        let block_h = (h / 6).max(4);
        let x = (millis / 8 % (w - block_w + 1) as u128) as usize;
        rect(&mut data, cfg.size, x, h * 2 / 3, block_w, block_h, [88, 180, 64, 255]);
        if cfg.cursor == CursorMode::Embedded {
            for row in 0..10 {
                rect(&mut data, cfg.size, w / 2, h / 2 + row, row / 2 + 1, 1, [255; 4]);
            }
        }
        stamp::write_bgra(cfg.size, stride, &mut data, FrameStamp::from_capture_time(capture_ts))?;
        let cpu = CpuFrame::new(cfg.size, PixelFormat::Bgra, vec![Plane { data: data.into(), stride }])?;
        Ok(RawFrame { info: FrameInfo { capture_ts, frame_id }, buffer: VideoBuffer::Cpu(cpu) })
    }
}

impl ScreenSource for SyntheticScreenSource {
    fn caps(&self) -> SourceCaps {
        SourceCaps {
            max_size: MAX_SIZE,
            max_fps: MAX_FPS,
            cursor_modes: vec![CursorMode::Hidden, CursorMode::Embedded],
            hdr: false,
        }
    }

    fn start(&mut self, cfg: CaptureConfig, mut sink: Box<dyn FrameSink>) -> Result<()> {
        validate(cfg)?;
        if self.worker.is_some() {
            return Err(MediaError::State("screen source is already started; stop it before restarting"));
        }
        if self.epoch > Instant::now() {
            return Err(MediaError::Invalid("capture epoch is in the future"));
        }
        let (commands, receiver) = mpsc::sync_channel(1);
        let epoch = self.epoch;
        let thread = thread::Builder::new()
            .name("gwar-test-screen".into())
            .spawn(move || {
                if let Err(error) = run(cfg, epoch, receiver, sink.as_mut()) {
                    sink.on_error(error);
                }
            })
            .map_err(|error| MediaError::Adapter(error.to_string()))?;
        self.worker = Some(Worker { commands, thread });
        Ok(())
    }

    fn reconfigure(&mut self, cfg: CaptureConfig) -> Result<()> {
        validate(cfg)?;
        let worker = self.worker.as_ref().ok_or(MediaError::State("screen source is stopped"))?;
        let (acknowledge, applied) = mpsc::sync_channel(1);
        worker
            .commands
            .send(Command::Configure(cfg, acknowledge))
            .map_err(|_| MediaError::State("screen source worker has ended"))?;
        applied.recv().map_err(|_| MediaError::State("screen source worker has ended"))
    }

    fn stop(&mut self) {
        if let Some(worker) = self.worker.take() {
            let _ = worker.commands.send(Command::Stop);
            let _ = worker.thread.join();
        }
    }
}

impl Drop for SyntheticScreenSource {
    fn drop(&mut self) {
        self.stop();
    }
}

fn validate(cfg: CaptureConfig) -> Result<()> {
    if cfg.size.width < 128
        || cfg.size.height < 32
        || cfg.size.width > MAX_SIZE.width
        || cfg.size.height > MAX_SIZE.height
        || cfg.max_fps == 0
        || cfg.max_fps > MAX_FPS
    {
        return Err(MediaError::Invalid("unsupported synthetic capture configuration"));
    }
    Ok(())
}

fn run(mut cfg: CaptureConfig, epoch: Instant, commands: Receiver<Command>, sink: &mut dyn FrameSink) -> Result<()> {
    let mut deadline = Instant::now();
    let mut id = 0;
    loop {
        match commands.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Command::Stop) | Err(RecvTimeoutError::Disconnected) => return Ok(()),
            Ok(Command::Configure(next, applied)) => {
                cfg = next;
                deadline = Instant::now();
                let _ = applied.send(());
            }
            Err(RecvTimeoutError::Timeout) => {
                let captured = Instant::now();
                sink.on_frame(SyntheticScreenSource::render(cfg, captured.duration_since(epoch), id)?)?;
                id = id.checked_add(1).ok_or(MediaError::State("synthetic frame ID exhausted"))?;
                let interval = Duration::from_secs_f64(1.0 / f64::from(cfg.max_fps));
                deadline = captured + interval;
                if deadline < Instant::now() {
                    deadline = Instant::now() + interval;
                }
            }
        }
    }
}

fn rect(data: &mut [u8], size: Size, x: usize, y: usize, w: usize, h: usize, color: [u8; 4]) {
    for row in y..(y + h).min(size.height as usize) {
        for column in x..(x + w).min(size.width as usize) {
            let offset = (row * size.width as usize + column) * 4;
            data[offset..offset + 4].copy_from_slice(&color);
        }
    }
}

// Five columns, seven low bits per column; just the fixture's fixed ASCII alphabet.
fn glyph(letter: u8) -> [u8; 5] {
    match letter {
        b'A' => [0x7e, 0x11, 0x11, 0x11, 0x7e],
        b'D' => [0x7f, 0x41, 0x41, 0x22, 0x1c],
        b'E' => [0x7f, 0x49, 0x49, 0x49, 0x41],
        b'G' => [0x3e, 0x41, 0x49, 0x49, 0x7a],
        b'I' => [0x00, 0x41, 0x7f, 0x41, 0x00],
        b'M' => [0x7f, 0x02, 0x0c, 0x02, 0x7f],
        b'N' => [0x7f, 0x04, 0x08, 0x10, 0x7f],
        b'R' => [0x7f, 0x09, 0x19, 0x29, 0x46],
        b'T' => [0x01, 0x01, 0x7f, 0x01, 0x01],
        b'V' => [0x1f, 0x20, 0x40, 0x20, 0x1f],
        b'W' => [0x7f, 0x20, 0x18, 0x20, 0x7f],
        b'0' => [0x3e, 0x51, 0x49, 0x45, 0x3e],
        b'1' => [0x00, 0x42, 0x7f, 0x40, 0x00],
        b'2' => [0x42, 0x61, 0x51, 0x49, 0x46],
        b'3' => [0x21, 0x41, 0x45, 0x4b, 0x31],
        b'4' => [0x18, 0x14, 0x12, 0x7f, 0x10],
        b'5' => [0x27, 0x45, 0x45, 0x45, 0x39],
        b'6' => [0x3c, 0x4a, 0x49, 0x49, 0x30],
        b'7' => [0x01, 0x71, 0x09, 0x05, 0x03],
        b'8' => [0x36, 0x49, 0x49, 0x49, 0x36],
        b'9' => [0x06, 0x49, 0x49, 0x29, 0x1e],
        _ => [0; 5],
    }
}

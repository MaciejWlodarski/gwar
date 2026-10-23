use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc::{self, RecvTimeoutError},
    },
    time::{Duration, Instant},
};

use gwar_media::*;

fn cfg() -> CaptureConfig {
    CaptureConfig { size: Size { width: 320, height: 180 }, max_fps: 30, cursor: CursorMode::Hidden }
}

fn cpu(frame: &RawFrame) -> &CpuFrame {
    let VideoBuffer::Cpu(cpu) = &frame.buffer else { panic!("expected CPU fixture") };
    cpu
}

#[test]
fn cpu_planes_validate_odd_yuv_dimensions_strides_and_lengths() {
    let size = Size { width: 3, height: 3 };
    let plane = |len, stride| Plane { data: vec![0; len].into(), stride };
    // The last row need not include padding. Chroma dimensions are ceil-divided.
    assert!(CpuFrame::new(size, PixelFormat::Bgra, vec![plane(44, 16)]).is_ok());
    assert!(CpuFrame::new(size, PixelFormat::Bgra, vec![plane(43, 16)]).is_err());
    assert!(CpuFrame::new(size, PixelFormat::Bgra, vec![plane(36, 11)]).is_err());
    assert!(CpuFrame::new(size, PixelFormat::Nv12, vec![plane(9, 3), plane(8, 4)]).is_ok());
    assert!(CpuFrame::new(size, PixelFormat::Nv12, vec![plane(9, 3), plane(6, 3)]).is_err());
    assert!(CpuFrame::new(size, PixelFormat::I420, vec![plane(9, 3), plane(4, 2), plane(4, 2)]).is_ok());
    assert!(CpuFrame::new(size, PixelFormat::I420, vec![plane(9, 3)]).is_err());
    assert!(CpuFrame::new(size, PixelFormat::Bgra, vec![plane(44, usize::MAX)]).is_err());
    assert!(CpuFrame::new(Size { width: 0, height: 3 }, PixelFormat::Bgra, vec![plane(12, 4)]).is_err());
}

#[test]
fn synthetic_content_moves_below_barcode_and_preserves_capture_identity() {
    let a = SyntheticScreenSource::render(cfg(), Duration::from_millis(120), 7).unwrap();
    let repeat = SyntheticScreenSource::render(cfg(), Duration::from_millis(120), 7).unwrap();
    let b = SyntheticScreenSource::render(cfg(), Duration::from_millis(360), 8).unwrap();
    assert_eq!(a.info.frame_id, 7);
    assert_eq!(stamp::read(cpu(&a)).unwrap().millis(), 120);
    let first = &cpu(&a).planes()[0].data;
    let second = &cpu(&b).planes()[0].data;
    assert_eq!(first, &cpu(&repeat).planes()[0].data);
    let row = cpu(&a).planes()[0].stride;
    let text = cfg().size.height as usize / 4 * row;
    let block = cfg().size.height as usize * 2 / 3 * row;
    assert_ne!(&first[text..block], &second[text..block], "text must scroll");
    assert_ne!(&first[block..], &second[block..], "block must move");
    let mut cursor = cfg();
    cursor.cursor = CursorMode::Embedded;
    let pointed = SyntheticScreenSource::render(cursor, a.info.capture_ts, 7).unwrap();
    assert_ne!(first, &cpu(&pointed).planes()[0].data);
}

struct Collector(mpsc::Sender<std::result::Result<RawFrame, MediaError>>);
impl FrameSink for Collector {
    fn on_frame(&mut self, frame: RawFrame) -> Result<()> {
        self.0.send(Ok(frame)).map_err(|e| MediaError::Adapter(e.to_string()))
    }
    fn on_error(&mut self, error: MediaError) {
        let _ = self.0.send(Err(error));
    }
}

#[test]
fn source_reconfigure_stop_restart_and_drop_fence_old_callbacks() {
    let mut source: Box<dyn ScreenSource> = Box::new(SyntheticScreenSource::new(Instant::now()));
    let (tx, rx) = mpsc::channel();
    assert!(source.reconfigure(cfg()).is_err());
    source.stop();
    source.start(cfg(), Box::new(Collector(tx.clone()))).unwrap();
    let first = rx.recv_timeout(Duration::from_secs(3)).unwrap().unwrap();
    assert_eq!(first.buffer.size(), cfg().size);
    assert!(source.start(cfg(), Box::new(Collector(tx))).is_err());
    let mut next = cfg();
    next.max_fps = 0;
    assert!(source.reconfigure(next).is_err());
    next.size = Size { width: 640, height: 360 };
    next.max_fps = 60;
    source.reconfigure(next).unwrap();
    // Old frames may already be in our downstream queue, but new callbacks use the new size.
    let changed = loop {
        let frame = rx.recv_timeout(Duration::from_secs(3)).unwrap().unwrap();
        if frame.buffer.size() == next.size {
            break frame;
        }
    };
    assert!(changed.info.capture_ts >= first.info.capture_ts);
    assert!(changed.info.frame_id > first.info.frame_id);
    assert_eq!(stamp::read(cpu(&changed)).unwrap(), stamp::FrameStamp::from_capture_time(changed.info.capture_ts));
    source.stop();
    for frame in rx.try_iter() {
        frame.unwrap();
    }
    assert!(matches!(rx.recv_timeout(Duration::from_secs(1)), Err(RecvTimeoutError::Disconnected)));
    source.stop();
    let (tx, rx) = mpsc::channel();
    source.start(cfg(), Box::new(Collector(tx))).unwrap();
    let restarted = rx.recv_timeout(Duration::from_secs(3)).unwrap().unwrap();
    assert_eq!(restarted.info.frame_id, 0);
    assert!(restarted.info.capture_ts >= changed.info.capture_ts);
    drop(source);
    for frame in rx.try_iter() {
        frame.unwrap();
    }
    assert!(matches!(rx.recv_timeout(Duration::from_secs(1)), Err(RecvTimeoutError::Disconnected)));
}

struct RejectingSink(mpsc::Sender<MediaError>);
impl FrameSink for RejectingSink {
    fn on_frame(&mut self, _: RawFrame) -> Result<()> {
        Err(MediaError::Adapter("full output queue".into()))
    }
    fn on_error(&mut self, error: MediaError) {
        self.0.send(error).unwrap();
    }
}

#[test]
fn sink_failure_is_reported_once_and_worker_can_be_joined() {
    let mut source = SyntheticScreenSource::new(Instant::now());
    let (tx, rx) = mpsc::channel();
    source.start(cfg(), Box::new(RejectingSink(tx))).unwrap();
    assert_eq!(rx.recv_timeout(Duration::from_secs(3)).unwrap(), MediaError::Adapter("full output queue".into()));
    assert!(matches!(rx.recv_timeout(Duration::from_secs(1)), Err(RecvTimeoutError::Disconnected)));
    assert!(source.reconfigure(cfg()).is_err());
    source.stop();
}

#[test]
fn invalid_configuration_does_not_start_capture() {
    let mut source = SyntheticScreenSource::new(Instant::now());
    for config in [
        CaptureConfig { max_fps: 0, ..cfg() },
        CaptureConfig { max_fps: 241, ..cfg() },
        CaptureConfig { size: Size { width: 127, height: 32 }, ..cfg() },
        CaptureConfig { size: Size { width: 3841, height: 2160 }, ..cfg() },
    ] {
        let (tx, rx) = mpsc::channel();
        assert!(source.start(config, Box::new(Collector(tx))).is_err());
        assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Disconnected)));
        assert!(SyntheticScreenSource::render(config, Duration::ZERO, 0).is_err());
    }
}

struct Surface(Arc<AtomicUsize>);
impl VideoSurface for Surface {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
impl Drop for Surface {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn surface_and_cpu_clones_retain_owned_storage() {
    let drops = Arc::new(AtomicUsize::new(0));
    let buffer = VideoBuffer::Surface { size: cfg().size, surface: Arc::new(Surface(drops.clone())) };
    let clone = buffer.clone();
    drop(buffer);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    let VideoBuffer::Surface { surface, .. } = &clone else { unreachable!() };
    assert!(surface.as_any().downcast_ref::<Surface>().is_some());
    drop(clone);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    let frame = SyntheticScreenSource::render(cfg(), Duration::ZERO, 0).unwrap();
    let copy = frame.clone();
    assert!(Arc::ptr_eq(&cpu(&frame).planes()[0].data, &cpu(&copy).planes()[0].data));
}

#[test]
fn keyframes_can_carry_av1_layer_metadata_and_adapters_are_object_safe() {
    let frame = EncodedFrame {
        info: FrameInfo { capture_ts: Duration::ZERO, frame_id: 0 },
        codec: Codec::Av1,
        data: Arc::from([1u8, 2]),
        dependencies: FrameDependencies::Independent,
        layer: Some(LayerInfo { spatial_id: 0, temporal_id: 0, dependency_descriptor: Some(Arc::from([0x80u8])) }),
    };
    assert!(frame.is_keyframe());
    assert!(!EncodedFrame { dependencies: FrameDependencies::Unknown, ..frame }.is_keyframe());
    let _: Option<Box<dyn ScreenSource>> = None;
    let _: Option<Box<dyn AudioSource>> = None;
    let _: Option<Box<dyn VideoEncoder>> = None;
    let _: Option<Box<dyn VideoDecoder>> = None;
    let _: Option<Box<dyn VideoSink>> = None;
    let _: Option<Box<dyn EncoderControl>> = None;
    let _: Option<Box<dyn EncodedVideoSink>> = None;
}

#[test]
fn audio_requires_complete_finite_stereo_blocks() {
    for samples in [vec![], vec![0.0], vec![0.0, f32::NAN], vec![f32::INFINITY, 0.0]] {
        assert!(AudioFrame::new(Duration::ZERO, samples.into()).is_err());
    }
    let frame = AudioFrame::new(Duration::from_millis(42), Arc::from([0.25, -0.25])).unwrap();
    assert_eq!(frame.samples(), &[0.25, -0.25]);
    assert_eq!(frame.capture_ts, Duration::from_millis(42));
}

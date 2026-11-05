//! The always-on pipeline: capture → encoder → rolling buffer of short
//! segment files on disk. Old segments are deleted as soon as they fall out
//! of the buffer window (or out of a manual recording).
//!
//! MP4 mode: frames go to the H.264 encoder (one MP4 per segment), sound is
//! kept as raw PCM next to it. AVI mode: frames become JPEGs in a record file.

use crate::avi;
use crate::capture::{self, Capturer, MonitorInfo};
use crate::mf;
use crate::settings::Format;
use crossbeam_channel::{bounded, unbounded, Receiver, RecvTimeoutError, Sender, TrySendError};
use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use windows::Win32::Media::MediaFoundation::{IMFDXGIDeviceManager, IMFSample};

const SEGMENT_MS: u64 = 10_000;
const MAX_RAW_QUEUE: usize = 8;

pub enum AppEvent {
    Saved { path: PathBuf, secs: f64, bytes: u64, what: String, truncated: bool },
    Error(String),
}

pub enum WriterMsg {
    /// JPEG frame (AVI mode).
    Video { slot: u64, data: Vec<u8> },
    /// NV12 frame (MP4 mode).
    Raw { slot: u64, data: Vec<u8> },
    /// NV12 frame on the video card (MP4, GPU mode).
    Gpu { slot: u64, sample: mf::Mf<IMFSample> },
    /// PCM block for one sound track (index into the clip's track list).
    Audio { track: u8, pos: u64, data: Vec<u8> },
    Save { from_ms: u64, out: PathBuf, what: String },
    Stop,
}

#[derive(Default)]
pub struct Shared {
    pub buffer_secs: AtomicU64,
    pub buffered_ms: AtomicU64,
    pub disk_bytes: AtomicU64,
    pub frames_sent: AtomicU64,
    pub frames_dropped: AtomicU64,
    pub fps_x10: AtomicU64,
    pub level_sys: AtomicU32,
    pub level_chat: AtomicU32,
    pub level_mic: AtomicU32,
    /// Voice-chat apps currently captured on their own track.
    pub chat_found: Mutex<Vec<String>>,
    pub error: Mutex<Option<String>>,
    pub audio_note: Mutex<Option<String>>,
    pub format_note: Mutex<Option<String>>,
    pub saving: AtomicU32,
    pub show_cursor: AtomicBool,
    raw_queued: AtomicUsize,
    /// GPU mode: lets the encoder use the capture device (set before the first frame).
    gpu_manager: Mutex<Option<mf::Mf<IMFDXGIDeviceManager>>>,
}

pub struct PipelineConfig {
    pub monitor: MonitorInfo,
    pub fps: u32,
    #[allow(dead_code)] // output size, kept for diagnostics
    pub out_w: u32,
    #[allow(dead_code)]
    pub out_h: u32,
    pub quality: u8,
    pub bitrate: u32,
    pub format: Format,
    pub game_audio: bool,
    pub chat_track: bool,
    pub chat_apps: Vec<String>,
    pub mic: bool,
    pub temp_dir: PathBuf,
    pub key: String,
}

pub struct Pipeline {
    pub shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    writer_tx: Sender<WriterMsg>,
    threads: Vec<JoinHandle<()>>,
    writer: Option<JoinHandle<()>>,
    t0: Instant,
    pub key: String,
    pub fps: u32,
    #[allow(dead_code)] // output size, kept for diagnostics
    pub out_w: u32,
    #[allow(dead_code)]
    pub out_h: u32,
    pub format: Format,
    /// Frames go capture → encoder on the video card (MP4 only).
    pub gpu: bool,
    pub tracks: Vec<crate::audio::Track>,
}

struct Frame {
    slot: u64,
    data: Vec<u8>,
}

#[derive(Clone, Copy)]
struct Spec {
    fps: u32,
    w: u32,
    h: u32,
    audio_tracks: u8,
    format: Format,
    bitrate: u32,
}

impl Pipeline {
    pub fn start(mut cfg: PipelineConfig, buffer_secs: u64, events: Sender<AppEvent>) -> Pipeline {
        let _ = fs::create_dir_all(&cfg.temp_dir);
        let shared = Arc::new(Shared::default());
        shared.buffer_secs.store(buffer_secs, Ordering::Relaxed);
        let mut gpu = false;
        if cfg.format == Format::Mp4 {
            // Probe on a worker thread: the UI thread is a COM STA (set up by the
            // window system), and Media Foundation objects made there misbehave.
            // Then check whether frames can stay on the video card (much less CPU).
            let probe = std::thread::scope(|s| {
                s.spawn(|| {
                    mf::com_init();
                    mf::probe(&cfg.temp_dir, cfg.out_w, cfg.out_h, cfg.fps, cfg.bitrate)?;
                    Ok::<bool, String>(std::env::var_os("RB_NOGPU").is_none()
                        && crate::gpu::probe(&cfg.monitor, &cfg.temp_dir, cfg.out_w, cfg.out_h, cfg.fps, cfg.bitrate).is_ok())
                })
                .join()
                .unwrap_or_else(|_| Err("сбой проверки".into()))
            });
            match probe {
                Ok(g) => gpu = g,
                Err(e) => {
                    cfg.format = Format::Avi;
                    *shared.format_note.lock().unwrap() = Some(format!("MP4 недоступен ({e}) — пишу в AVI"));
                }
            }
        }
        let stop = Arc::new(AtomicBool::new(false));
        let t0 = Instant::now();
        let (writer_tx, writer_rx) = unbounded::<WriterMsg>();
        let any_audio = cfg.game_audio || cfg.chat_track || cfg.mic;
        let mix_only = cfg.format == Format::Avi;
        let tracks = if !any_audio {
            Vec::new()
        } else if mix_only {
            vec![crate::audio::Track::Mix]
        } else {
            crate::audio::tracks(cfg.game_audio, cfg.chat_track, cfg.mic)
        };
        let mut threads = Vec::new();

        let frame_tx = if cfg.format == Format::Avi {
            let (frame_tx, frame_rx) = bounded::<Frame>(6);
            let encoders = if cfg.fps >= 60 { 4 } else { 3 };
            for _ in 0..encoders {
                let rx = frame_rx.clone();
                let tx = writer_tx.clone();
                let (w, h, q) = (cfg.out_w, cfg.out_h, cfg.quality);
                threads.push(std::thread::spawn(move || encoder_thread(rx, tx, w, h, q)));
            }
            Some(frame_tx)
        } else {
            None
        };

        if any_audio {
            let acfg = crate::audio::AudioConfig {
                game: cfg.game_audio,
                chat: cfg.chat_track,
                mic: cfg.mic,
                chat_apps: cfg.chat_apps.clone(),
                mix_only,
            };
            let (st, tx, sh) = (stop.clone(), writer_tx.clone(), shared.clone());
            threads.push(std::thread::spawn(move || crate::audio::run(acfg, st, tx, t0, sh)));
        }

        let spec = Spec {
            fps: cfg.fps,
            w: cfg.out_w,
            h: cfg.out_h,
            audio_tracks: tracks.len() as u8,
            format: cfg.format,
            bitrate: cfg.bitrate,
        };
        let writer = {
            let sh = shared.clone();
            let dir = cfg.temp_dir.clone();
            let ev = events.clone();
            std::thread::spawn(move || writer_thread(writer_rx, sh, t0, spec, dir, ev))
        };

        let key = cfg.key.clone();
        {
            let sh = shared.clone();
            let st = stop.clone();
            let wtx = writer_tx.clone();
            threads.insert(0, std::thread::spawn(move || capture_thread(cfg, gpu, sh, st, frame_tx, wtx, t0)));
        }

        Pipeline {
            shared,
            stop,
            writer_tx,
            threads,
            writer: Some(writer),
            t0,
            key,
            fps: spec.fps,
            out_w: spec.w,
            out_h: spec.h,
            format: spec.format,
            gpu,
            tracks,
        }
    }

    pub fn now_ms(&self) -> u64 {
        self.t0.elapsed().as_millis() as u64
    }

    pub fn save_last(&self, secs: u64, out: PathBuf, what: String) {
        let from_ms = self.now_ms().saturating_sub(secs * 1000);
        let _ = self.writer_tx.send(WriterMsg::Save { from_ms, out, what });
    }

    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
        let _ = self.writer_tx.send(WriterMsg::Stop);
        if let Some(w) = self.writer.take() {
            let _ = w.join();
        }
    }
}

fn capture_thread(
    cfg: PipelineConfig,
    gpu: bool,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    jpeg_tx: Option<Sender<Frame>>,
    writer_tx: Sender<WriterMsg>,
    t0: Instant,
) {
    let mut cap = match Capturer::new(&cfg.monitor) {
        Ok(c) => c,
        Err(e) => {
            *shared.error.lock().unwrap() = Some(e);
            return;
        }
    };
    // GPU mode: pictures stay on the video card and are converted there.
    let mut conv: Option<crate::gpu::Converter> = None;
    if gpu {
        cap.gpu = true;
        match unsafe { crate::gpu::device_manager(cap.device()) } {
            Ok(m) => *shared.gpu_manager.lock().unwrap() = Some(mf::Mf(m)),
            Err(e) => {
                *shared.error.lock().unwrap() = Some(e);
                return;
            }
        }
    }
    let fps = cfg.fps as f64;
    let (w, h) = (cfg.out_w, cfg.out_h);
    let mut scaled = if gpu { Vec::new() } else { vec![0u8; (w * h * 4) as usize] };
    let mut have = false;
    let mut dirty = false;
    let mut last_sent = Instant::now() - Duration::from_secs(10);
    let mut fps_window = (Instant::now(), 0u64);
    while !stop.load(Ordering::Relaxed) {
        let slot = (t0.elapsed().as_secs_f64() * fps) as u64;
        match cap.grab(0) {
            Ok(true) if gpu => {
                have = true;
                dirty = true;
                *shared.error.lock().unwrap() = None;
            }
            Ok(true) => {
                capture::scale_bgra_par(&cap.frame, cap.width, cap.height, &mut scaled, w, h);
                if shared.show_cursor.load(Ordering::Relaxed) {
                    capture::draw_pointer(&cap.pointer, cap.width, cap.height, &mut scaled, w, h);
                }
                have = true;
                dirty = true;
                *shared.error.lock().unwrap() = None;
            }
            Ok(false) => {}
            Err(e) => {
                *shared.error.lock().unwrap() = Some(e);
                std::thread::sleep(Duration::from_millis(500));
                continue;
            }
        }
        // Send changed pictures, plus a refresh at least once a second so
        // every part of the buffer has a picture to start from.
        if have && (dirty || last_sent.elapsed() >= Duration::from_secs(1)) {
            let sent = match &jpeg_tx {
                Some(tx) => match tx.try_send(Frame { slot, data: scaled.clone() }) {
                    Ok(()) => true,
                    Err(TrySendError::Full(_)) => false,
                    Err(TrySendError::Disconnected(_)) => break,
                },
                None if gpu => {
                    if shared.raw_queued.load(Ordering::Relaxed) >= MAX_RAW_QUEUE {
                        false
                    } else if let Some(latest) = cap.latest().cloned() {
                        if conv.as_ref().is_none_or(|c| c.in_w != cap.width || c.in_h != cap.height) {
                            match crate::gpu::Converter::new(cap.device(), cap.width, cap.height, w, h, cfg.fps) {
                                Ok(c) => conv = Some(c),
                                Err(e) => {
                                    *shared.error.lock().unwrap() = Some(e);
                                    std::thread::sleep(Duration::from_millis(500));
                                    continue;
                                }
                            }
                        }
                        let c = conv.as_mut().unwrap();
                        let pointer = shared.show_cursor.load(Ordering::Relaxed).then_some(&cap.pointer);
                        let src = c.compose(&latest, pointer);
                        match c.convert(&src) {
                            Ok(sample) => {
                                shared.raw_queued.fetch_add(1, Ordering::Relaxed);
                                if writer_tx.send(WriterMsg::Gpu { slot, sample }).is_err() {
                                    break;
                                }
                                true
                            }
                            Err(e) => {
                                *shared.error.lock().unwrap() = Some(e);
                                false
                            }
                        }
                    } else {
                        false
                    }
                }
                None => {
                    if shared.raw_queued.load(Ordering::Relaxed) >= MAX_RAW_QUEUE {
                        false
                    } else {
                        let mut nv12 = Vec::new();
                        mf::bgra_to_nv12_par(&scaled, w as usize, h as usize, &mut nv12);
                        shared.raw_queued.fetch_add(1, Ordering::Relaxed);
                        if writer_tx.send(WriterMsg::Raw { slot, data: nv12 }).is_err() {
                            break;
                        }
                        true
                    }
                }
            };
            if sent {
                dirty = false;
                last_sent = Instant::now();
                shared.frames_sent.fetch_add(1, Ordering::Relaxed);
                fps_window.1 += 1;
            } else {
                shared.frames_dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
        if fps_window.0.elapsed() >= Duration::from_secs(1) {
            let el = fps_window.0.elapsed().as_secs_f64();
            shared.fps_x10.store((fps_window.1 as f64 * 10.0 / el) as u64, Ordering::Relaxed);
            fps_window = (Instant::now(), 0);
        }
        let next = t0 + Duration::from_secs_f64((slot + 1) as f64 / fps);
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        }
    }
}

fn encoder_thread(rx: Receiver<Frame>, tx: Sender<WriterMsg>, w: u32, h: u32, q: u8) {
    while let Ok(f) = rx.recv() {
        let mut out = Vec::with_capacity((w * h / 4) as usize);
        let enc = jpeg_encoder::Encoder::new(&mut out, q);
        if enc.encode(&f.data, w as u16, h as u16, jpeg_encoder::ColorType::Bgra).is_ok()
            && tx.send(WriterMsg::Video { slot: f.slot, data: out }).is_err()
        {
            break;
        }
    }
}

pub struct Segment {
    /// Record file: JPEG frames + PCM (AVI mode) or only PCM (MP4 mode).
    pub path: PathBuf,
    pub mp4: Option<PathBuf>,
    pub base_slot: AtomicU64,
    pub has_video: AtomicBool,
    pub ready: AtomicBool,
    pub start_ms: u64,
    pub end_ms: AtomicU64,
    pub bytes: AtomicU64,
}

impl Drop for Segment {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        if let Some(m) = &self.mp4 {
            let _ = fs::remove_file(m);
        }
    }
}

struct Cur {
    rec: BufWriter<File>,
    seg: Arc<Segment>,
    vw: Option<mf::VideoSegWriter>,
}

fn close(cur: &mut Option<Cur>, segs: &mut VecDeque<Arc<Segment>>) {
    if let Some(mut c) = cur.take() {
        let _ = c.rec.flush();
        let seg = c.seg.clone();
        match c.vw.take() {
            Some(vw) => {
                std::thread::spawn(move || {
                    mf::com_init();
                    let _ = vw.finish();
                    if let Some(m) = &seg.mp4 {
                        let n = fs::metadata(m).map(|m| m.len()).unwrap_or(0);
                        seg.bytes.fetch_add(n, Ordering::Relaxed);
                    }
                    seg.ready.store(true, Ordering::Release);
                });
            }
            None => seg.ready.store(true, Ordering::Release),
        }
        segs.push_back(c.seg);
    }
}

fn writer_thread(
    rx: Receiver<WriterMsg>,
    shared: Arc<Shared>,
    t0: Instant,
    spec: Spec,
    dir: PathBuf,
    events: Sender<AppEvent>,
) {
    if spec.format == Format::Mp4 {
        mf::com_init();
    }
    let mut segs: VecDeque<Arc<Segment>> = VecDeque::new();
    let mut cur: Option<Cur> = None;
    let mut n = 0u64;
    let mut reported = false;
    let mut report = |msg: String| {
        if !reported {
            reported = true;
            let _ = events.send(AppEvent::Error(msg));
        }
    };

    loop {
        let msg = match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(m) => Some(m),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        let now = t0.elapsed().as_millis() as u64;

        // Open a segment when needed.
        let mut ensure = |cur: &mut Option<Cur>, report: &mut dyn FnMut(String)| -> bool {
            if cur.is_some() {
                return true;
            }
            let path = dir.join(format!("seg{n:06}.buf"));
            let mp4 = (spec.format == Format::Mp4).then(|| dir.join(format!("seg{n:06}.mp4")));
            n += 1;
            match File::create(&path) {
                Ok(f) => {
                    let seg = Arc::new(Segment {
                        path,
                        mp4,
                        base_slot: AtomicU64::new(0),
                        has_video: AtomicBool::new(false),
                        ready: AtomicBool::new(false),
                        start_ms: now,
                        end_ms: AtomicU64::new(now),
                        bytes: AtomicU64::new(0),
                    });
                    *cur = Some(Cur { rec: BufWriter::with_capacity(1 << 20, f), seg, vw: None });
                    true
                }
                Err(e) => {
                    report(format!("Нет записи на диск: {e}"));
                    false
                }
            }
        };

        match msg {
            Some(WriterMsg::Video { slot, data }) => {
                if ensure(&mut cur, &mut report) {
                    let c = cur.as_mut().unwrap();
                    if avi::write_record(&mut c.rec, avi::KIND_VIDEO, slot, &data).is_ok() {
                        c.seg.end_ms.store(now, Ordering::Relaxed);
                        c.seg.bytes.fetch_add((data.len() + avi::REC_HEADER) as u64, Ordering::Relaxed);
                        c.seg.has_video.store(true, Ordering::Relaxed);
                    }
                }
            }
            Some(m @ (WriterMsg::Raw { .. } | WriterMsg::Gpu { .. })) => {
                // A picture in memory (NV12 bytes) or on the video card (GPU mode).
                let (slot, frame) = match m {
                    WriterMsg::Raw { slot, data } => (slot, Ok(data)),
                    WriterMsg::Gpu { slot, sample } => (slot, Err(sample)),
                    _ => unreachable!(),
                };
                shared.raw_queued.fetch_sub(1, Ordering::Relaxed);
                // MP4 segments always start on a fresh frame (= key frame).
                if cur.as_ref().is_some_and(|c| now.saturating_sub(c.seg.start_ms) >= SEGMENT_MS) {
                    close(&mut cur, &mut segs);
                }
                if ensure(&mut cur, &mut report) {
                    let c = cur.as_mut().unwrap();
                    if c.vw.is_none() {
                        let path = c.seg.mp4.as_ref().unwrap();
                        let made = match (&frame, shared.gpu_manager.lock().unwrap().as_ref()) {
                            (Err(_), Some(m)) => mf::VideoSegWriter::create_gpu(path, spec.w, spec.h, spec.fps, spec.bitrate, &m.0),
                            (Err(_), None) => Err("нет видеокарты для кодировщика".into()),
                            (Ok(_), _) => mf::VideoSegWriter::create(path, spec.w, spec.h, spec.fps, spec.bitrate),
                        };
                        match made {
                            Ok(v) => {
                                c.vw = Some(v);
                                c.seg.base_slot.store(slot, Ordering::Relaxed);
                                c.seg.has_video.store(true, Ordering::Relaxed);
                            }
                            Err(e) => report(format!("Кодировщик видео: {e}")),
                        }
                    }
                    if let Some(vw) = c.vw.as_mut() {
                        let base = c.seg.base_slot.load(Ordering::Relaxed);
                        if slot >= base {
                            let r = match &frame {
                                Ok(data) => vw.write(data, slot - base),
                                Err(sample) => vw.write_sample(&sample.0, slot - base),
                            };
                            if let Err(e) = r {
                                report(format!("Кодировщик видео: {e}"));
                            }
                            c.seg.end_ms.store(now, Ordering::Relaxed);
                        }
                    }
                }
            }
            Some(WriterMsg::Audio { track, pos, data }) => {
                if ensure(&mut cur, &mut report) {
                    let c = cur.as_mut().unwrap();
                    if avi::write_record(&mut c.rec, avi::KIND_AUDIO + track, pos, &data).is_ok() {
                        c.seg.end_ms.store(now, Ordering::Relaxed);
                        c.seg.bytes.fetch_add((data.len() + avi::REC_HEADER) as u64, Ordering::Relaxed);
                    }
                }
            }
            Some(WriterMsg::Stop) => break,
            Some(WriterMsg::Save { from_ms, out, what }) => {
                close(&mut cur, &mut segs);
                let list: Vec<Arc<Segment>> =
                    segs.iter().filter(|s| s.end_ms.load(Ordering::Relaxed) >= from_ms).cloned().collect();
                let clip = avi::ClipSpec { from_ms, to_ms: now, fps: spec.fps, width: spec.w, height: spec.h, audio_tracks: spec.audio_tracks };
                let ev = events.clone();
                let sh = shared.clone();
                sh.saving.fetch_add(1, Ordering::Relaxed);
                std::thread::spawn(move || {
                    save_clip(&list, &clip, spec.format, &out, what, &ev);
                    drop(list);
                    sh.saving.fetch_sub(1, Ordering::Relaxed);
                });
            }
            None => {}
        }
        if spec.format == Format::Avi && cur.as_ref().is_some_and(|c| now.saturating_sub(c.seg.start_ms) >= SEGMENT_MS) {
            close(&mut cur, &mut segs);
        }
        let window = shared.buffer_secs.load(Ordering::Relaxed) * 1000;
        let keep_from = now.saturating_sub(window);
        while segs.front().is_some_and(|s| s.end_ms.load(Ordering::Relaxed) < keep_from) {
            segs.pop_front();
        }
        let oldest = segs.front().map(|s| s.start_ms).or(cur.as_ref().map(|c| c.seg.start_ms));
        let buffered = oldest.map_or(0, |o| now.saturating_sub(o)).min(window);
        shared.buffered_ms.store(buffered, Ordering::Relaxed);
        let mut bytes: u64 = segs.iter().map(|s| s.bytes.load(Ordering::Relaxed)).sum();
        if let Some(c) = &cur {
            bytes += c.seg.bytes.load(Ordering::Relaxed);
            if c.vw.is_some() {
                bytes += spec.bitrate as u64 / 8 * now.saturating_sub(c.seg.start_ms) / 1000;
            }
        }
        shared.disk_bytes.store(bytes, Ordering::Relaxed);
    }
    close(&mut cur, &mut segs);
    // Let finalisers finish before the files are removed.
    let t = Instant::now();
    while segs.iter().any(|s| !s.ready.load(Ordering::Acquire)) && t.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(segs);
    let _ = fs::remove_dir(&dir);
}

fn save_clip(list: &[Arc<Segment>], clip: &avi::ClipSpec, format: Format, out: &PathBuf, what: String, ev: &Sender<AppEvent>) {
    if let Some(d) = out.parent() {
        let _ = fs::create_dir_all(d);
    }
    let t = Instant::now();
    while list.iter().any(|s| !s.ready.load(Ordering::Acquire)) && t.elapsed() < Duration::from_secs(15) {
        std::thread::sleep(Duration::from_millis(20));
    }
    if std::env::var_os("RB_PROF").is_some() {
        println!("prof: waited for segments {:?}", t.elapsed());
    }
    let r = match format {
        Format::Avi => {
            let paths: Vec<PathBuf> = list.iter().map(|s| s.path.clone()).collect();
            avi::assemble(&paths, clip, out).map_err(|e| e.to_string())
        }
        Format::Mp4 => {
            let refs: Vec<mf::SegRef> = list
                .iter()
                .filter(|s| s.has_video.load(Ordering::Relaxed))
                .map(|s| mf::SegRef {
                    mp4: s.mp4.clone().unwrap(),
                    audio: s.path.clone(),
                    base_slot: s.base_slot.load(Ordering::Relaxed),
                })
                .collect();
            mf::save_mp4(&refs, clip, out)
        }
    };
    match r {
        Ok(r) => {
            let _ = ev.send(AppEvent::Saved {
                path: out.clone(),
                secs: r.frames as f64 / clip.fps as f64,
                bytes: r.bytes,
                what,
                truncated: r.truncated,
            });
        }
        Err(e) => {
            let _ = fs::remove_file(out);
            let _ = ev.send(AppEvent::Error(format!("Не удалось сохранить: {e}")));
        }
    }
}

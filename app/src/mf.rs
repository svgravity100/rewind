//! H.264 / MP4 through Windows Media Foundation (uses the GPU encoder when
//! there is one). The buffer keeps short video-only MP4 segments; saving
//! copies their compressed frames into one MP4 without re-encoding and
//! encodes the sound to AAC in one go (so there are no gaps between parts).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::mem::ManuallyDrop;
use crossbeam_channel::{Receiver, Sender};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
use windows::core::{GUID, HSTRING};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

const HNS: i64 = 10_000_000;

pub fn startup() -> Result<(), String> {
    static R: OnceLock<Result<(), String>> = OnceLock::new();
    R.get_or_init(|| unsafe {
        com_init();
        MFStartup(MF_VERSION, MFSTARTUP_FULL).map_err(|e| format!("Media Foundation: {}", e.message()))
    })
    .clone()
}

pub fn com_init() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
}

fn e(what: &str) -> impl Fn(windows::core::Error) -> String + '_ {
    move |x| format!("{what}: {}", x.message())
}

fn pack(a: u32, b: u32) -> u64 {
    ((a as u64) << 32) | b as u64
}

unsafe fn video_type(subtype: &GUID, w: u32, h: u32, fps: u32) -> windows::core::Result<IMFMediaType> {
    let t = MFCreateMediaType()?;
    t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
    t.SetGUID(&MF_MT_SUBTYPE, subtype)?;
    t.SetUINT64(&MF_MT_FRAME_SIZE, pack(w, h))?;
    t.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
    t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
    t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
    Ok(t)
}

unsafe fn pcm_type() -> windows::core::Result<IMFMediaType> {
    let t = MFCreateMediaType()?;
    t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
    t.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
    t.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2)?;
    t.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, crate::audio::RATE)?;
    t.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
    t.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, 4)?;
    t.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, crate::audio::RATE * 4)?;
    Ok(t)
}

unsafe fn aac_type() -> windows::core::Result<IMFMediaType> {
    let t = MFCreateMediaType()?;
    t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
    t.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
    t.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2)?;
    t.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, crate::audio::RATE)?;
    t.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
    t.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, 24_000)?; // 192 kbit/s
    Ok(t)
}

unsafe fn sink_writer(path: &Path, hardware: bool, d3d: Option<&IMFDXGIDeviceManager>) -> windows::core::Result<IMFSinkWriter> {
    let mut attrs: Option<IMFAttributes> = None;
    MFCreateAttributes(&mut attrs, 4)?;
    let attrs = attrs.unwrap();
    attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, hardware as u32)?;
    // Files are written as fast as possible, never paced to real time.
    attrs.SetUINT32(&MF_SINK_WRITER_DISABLE_THROTTLING, 1)?;
    attrs.SetGUID(&MF_TRANSCODE_CONTAINERTYPE, &MFTranscodeContainerType_MPEG4)?;
    if let Some(m) = d3d {
        // Frames come as video-card textures (GPU mode).
        attrs.SetUnknown(&MF_SINK_WRITER_D3D_MANAGER, m)?;
    }
    MFCreateSinkWriterFromURL(&HSTRING::from(path.as_os_str()), None, &attrs)
}

unsafe fn sample_from(data: &[u8], time: i64, dur: i64) -> windows::core::Result<IMFSample> {
    let buf = MFCreateMemoryBuffer(data.len() as u32)?;
    let mut p: *mut u8 = std::ptr::null_mut();
    buf.Lock(&mut p, None, None)?;
    std::ptr::copy_nonoverlapping(data.as_ptr(), p, data.len());
    buf.Unlock()?;
    buf.SetCurrentLength(data.len() as u32)?;
    let s = MFCreateSample()?;
    s.AddBuffer(&buf)?;
    s.SetSampleTime(time)?;
    s.SetSampleDuration(dur)?;
    Ok(s)
}

/// One video-only MP4 segment of the buffer.
pub struct VideoSegWriter {
    w: IMFSinkWriter,
    stream: u32,
    fps: u32,
}

// Media Foundation objects are free-threaded; the writer is finalised on a
// helper thread so the capture never stalls.
unsafe impl Send for VideoSegWriter {}

impl VideoSegWriter {
    pub fn create(path: &Path, w: u32, h: u32, fps: u32, bitrate: u32) -> Result<Self, String> {
        Self::create_with(path, w, h, fps, bitrate, true, None).or_else(|_| Self::create_with(path, w, h, fps, bitrate, false, None))
    }

    /// A writer that takes NV12 video-card textures (see `gpu`), hardware encoder only.
    pub fn create_gpu(path: &Path, w: u32, h: u32, fps: u32, bitrate: u32, d3d: &IMFDXGIDeviceManager) -> Result<Self, String> {
        Self::create_with(path, w, h, fps, bitrate, true, Some(d3d))
    }

    fn create_with(path: &Path, w: u32, h: u32, fps: u32, bitrate: u32, hw: bool, d3d: Option<&IMFDXGIDeviceManager>) -> Result<Self, String> {
        startup()?;
        unsafe {
            let sw = sink_writer(path, hw, d3d).map_err(e("Запись MP4"))?;
            let out = video_type(&MFVideoFormat_H264, w, h, fps).map_err(e("H.264"))?;
            out.SetUINT32(&MF_MT_AVG_BITRATE, bitrate).map_err(e("H.264"))?;
            out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32).map_err(e("H.264"))?;
            let _ = out.SetUINT32(&MF_MT_MAX_KEYFRAME_SPACING, fps);
            let stream = sw.AddStream(&out).map_err(e("H.264"))?;
            let inp = video_type(&MFVideoFormat_NV12, w, h, fps).map_err(e("NV12"))?;
            inp.SetUINT32(&MF_MT_DEFAULT_STRIDE, w).map_err(e("NV12"))?;
            let _ = inp.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32);
            sw.SetInputMediaType(stream, &inp, None).map_err(e("Кодировщик H.264"))?;
            sw.BeginWriting().map_err(e("Кодировщик H.264"))?;
            Ok(Self { w: sw, stream, fps })
        }
    }

    pub fn write(&mut self, nv12: &[u8], frame_index: u64) -> Result<(), String> {
        let dur = HNS / self.fps as i64;
        let t = frame_index as i64 * HNS / self.fps as i64;
        unsafe {
            let s = sample_from(nv12, t, dur).map_err(e("Кадр"))?;
            self.w.WriteSample(self.stream, &s).map_err(e("Кадр"))
        }
    }

    /// Write a ready sample (GPU mode: a video-card texture).
    pub fn write_sample(&mut self, s: &IMFSample, frame_index: u64) -> Result<(), String> {
        let dur = HNS / self.fps as i64;
        let t = frame_index as i64 * HNS / self.fps as i64;
        unsafe {
            s.SetSampleTime(t).map_err(e("Кадр"))?;
            s.SetSampleDuration(dur).map_err(e("Кадр"))?;
            self.w.WriteSample(self.stream, s).map_err(e("Кадр"))
        }
    }

    pub fn finish(self) -> Result<(), String> {
        unsafe { self.w.Finalize().map_err(e("Завершение MP4")) }
    }
}

/// Check that this PC can encode H.264 at the chosen size and frame rate
/// (writes a tiny file).
pub fn probe(dir: &Path, w: u32, h: u32, fps: u32, bitrate: u32) -> Result<(), String> {
    let _ = std::fs::create_dir_all(dir);
    let p = dir.join("probe.mp4");
    let mut v = VideoSegWriter::create(&p, w, h, fps, bitrate)?;
    let frame = vec![128u8; (w * h * 3 / 2) as usize];
    for i in 0..3 {
        v.write(&frame, i)?;
    }
    let r = v.finish();
    let _ = std::fs::remove_file(&p);
    r
}

pub fn bitrate(w: u32, h: u32, fps: u32, q: crate::settings::Quality) -> u32 {
    let bpp = match q {
        crate::settings::Quality::Low => 0.08,
        crate::settings::Quality::Medium => 0.15,
        crate::settings::Quality::High => 0.25,
    };
    // Higher frame rates need fewer bits per frame (frames are more alike).
    let fps_factor = 30.0 * (fps as f64 / 30.0).powf(0.6);
    ((w as f64 * h as f64 * fps_factor * bpp) as u32).clamp(1_000_000, 100_000_000)
}

/// `bgra_to_nv12` split over several threads (needed for 120 fps).
pub fn bgra_to_nv12_par(src: &[u8], w: usize, h: usize, dst: &mut Vec<u8>) {
    dst.resize(w * h * 3 / 2, 0);
    let bands = 4usize;
    let rows_per = ((h / 2).div_ceil(bands)) * 2; // even number of rows per band
    let (y_plane, uv) = dst.split_at_mut(w * h);
    std::thread::scope(|s| {
        let mut y_rest = y_plane;
        let mut uv_rest = uv;
        let mut row = 0;
        while row < h {
            let n = rows_per.min(h - row);
            let (yb, yr) = y_rest.split_at_mut(n * w);
            let (ub, ur) = uv_rest.split_at_mut(n / 2 * w);
            y_rest = yr;
            uv_rest = ur;
            let sb = &src[row * w * 4..(row + n) * w * 4];
            s.spawn(move || {
                let mut tmp = Vec::new();
                bgra_to_nv12(sb, w, n, &mut tmp);
                yb.copy_from_slice(&tmp[..n * w]);
                ub.copy_from_slice(&tmp[n * w..]);
            });
            row += n;
        }
    });
}

/// BGRA → NV12 (BT.709, limited range). Width and height must be even.
pub fn bgra_to_nv12(src: &[u8], w: usize, h: usize, dst: &mut Vec<u8>) {
    dst.resize(w * h * 3 / 2, 0);
    let (y_plane, uv) = dst.split_at_mut(w * h);
    for y in 0..h {
        let row = &src[y * w * 4..][..w * 4];
        let out = &mut y_plane[y * w..][..w];
        for x in 0..w {
            let (b, g, r) = (row[x * 4] as i32, row[x * 4 + 1] as i32, row[x * 4 + 2] as i32);
            out[x] = ((47 * r + 157 * g + 16 * b + 128) >> 8) as u8 + 16;
        }
    }
    for y in 0..h / 2 {
        let r0 = &src[(2 * y) * w * 4..][..w * 4];
        let r1 = &src[(2 * y + 1) * w * 4..][..w * 4];
        let out = &mut uv[y * w..][..w];
        for x in 0..w / 2 {
            let i = x * 8;
            let b = (r0[i] as i32 + r0[i + 4] as i32 + r1[i] as i32 + r1[i + 4] as i32) / 4;
            let g = (r0[i + 1] as i32 + r0[i + 5] as i32 + r1[i + 1] as i32 + r1[i + 5] as i32) / 4;
            let r = (r0[i + 2] as i32 + r0[i + 6] as i32 + r1[i + 2] as i32 + r1[i + 6] as i32) / 4;
            out[x * 2] = ((-26 * r - 87 * g + 112 * b + 128) >> 8).wrapping_add(128).clamp(16, 240) as u8;
            out[x * 2 + 1] = ((112 * r - 102 * g - 10 * b + 128) >> 8).wrapping_add(128).clamp(16, 240) as u8;
        }
    }
}

pub struct SegRef {
    pub mp4: PathBuf,
    pub audio: PathBuf,
    pub base_slot: u64,
}

fn read_video(
    path: &Path,
    mut f: impl FnMut(IMFSample, i64, bool) -> Result<bool, String>,
) -> Result<Option<IMFMediaType>, String> {
    unsafe {
        let Ok(r) = MFCreateSourceReaderFromURL(&HSTRING::from(path.as_os_str()), None) else {
            return Ok(None);
        };
        let vs = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
        let _ = r.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false);
        r.SetStreamSelection(vs, true).map_err(e("Чтение буфера"))?;
        let ty = r.GetNativeMediaType(vs, 0).map_err(e("Чтение буфера"))?;
        loop {
            let mut flags = 0u32;
            let mut ts = 0i64;
            let mut s: Option<IMFSample> = None;
            r.ReadSample(vs, 0, None, Some(&mut flags), Some(&mut ts), Some(&mut s))
                .map_err(e("Чтение буфера"))?;
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                break;
            }
            let Some(s) = s else { continue };
            let key = s.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0;
            if !f(s, ts, key)? {
                break;
            }
        }
        Ok(Some(ty))
    }
}

/// PCM waiting to be encoded, in buffer sample positions.
struct AudioOut {
    q: VecDeque<(u64, Vec<u8>)>,
    next: u64,
    start: u64,
}

type SampleSink<'a> = dyn FnMut(IMFSample) -> Result<(), String> + 'a;

impl AudioOut {
    fn new(a0: u64) -> Self {
        AudioOut { q: VecDeque::new(), next: a0, start: a0 }
    }

    fn put(&self, data: &[u8], pos: u64, sink: &mut SampleSink) -> Result<(), String> {
        let rate = crate::audio::RATE as i64;
        let n = (data.len() / 4) as i64;
        let t = (pos - self.start) as i64 * HNS / rate;
        let s = unsafe { sample_from(data, t, n * HNS / rate) }.map_err(e("Звук"))?;
        sink(s)
    }

    /// Pass on everything before sample position `limit`, filling gaps with silence.
    fn until(&mut self, limit: u64, sink: &mut SampleSink) -> Result<(), String> {
        while let Some((pos, data)) = self.q.front() {
            let (pos, n) = (*pos, (data.len() / 4) as u64);
            if pos + n <= self.next {
                self.q.pop_front();
                continue;
            }
            if pos >= limit || self.next >= limit {
                break;
            }
            let a = pos.max(self.next);
            let b = (pos + n).min(limit);
            if a >= b {
                break;
            }
            while self.next < a {
                let k = (a - self.next).min(crate::audio::RATE as u64 / 10);
                self.put(&vec![0u8; (k * 4) as usize], self.next, sink)?;
                self.next += k;
            }
            let chunk = data[((a - pos) * 4) as usize..((b - pos) * 4) as usize].to_vec();
            self.put(&chunk, a, sink)?;
            self.next = b;
            if b == pos + n {
                self.q.pop_front();
            } else {
                break;
            }
        }
        Ok(())
    }
}

/// Carries Media Foundation objects (free-threaded) between threads.
#[derive(Clone)]
pub struct Mf<T>(pub T);
unsafe impl<T> Send for Mf<T> {}

/// Windows' own AAC encoder (CLSID_AACMFTEncoder).
const CLSID_AAC_ENCODER: GUID = GUID::from_u128(0x93af0c51_2275_45d2_a35b_f2ba21caed00);

/// A standalone AAC encoder, so each sound track can be encoded on its own
/// thread instead of one after another inside the file writer.
struct Aac {
    mft: IMFTransform,
    out_size: u32,
}

impl Aac {
    /// The encoder and the compressed type it produces (for the MP4 stream).
    unsafe fn new() -> windows::core::Result<(Aac, IMFMediaType)> {
        let mft: IMFTransform = CoCreateInstance(&CLSID_AAC_ENCODER, None, CLSCTX_INPROC_SERVER)?;
        mft.SetInputType(0, &pcm_type()?, 0)?;
        let out = aac_type()?;
        out.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0)?;
        mft.SetOutputType(0, &out, 0)?;
        let ty = mft.GetOutputCurrentType(0)?;
        let out_size = mft.GetOutputStreamInfo(0)?.cbSize.max(8192);
        mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
        mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        Ok((Aac { mft, out_size }, ty))
    }

    unsafe fn input(&self, s: &IMFSample, out: &mut SampleSink) -> Result<(), String> {
        self.mft.ProcessInput(0, s, 0).map_err(e("Кодировщик AAC"))?;
        self.pull(out)
    }

    unsafe fn finish(&self, out: &mut SampleSink) -> Result<(), String> {
        self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0).map_err(e("Кодировщик AAC"))?;
        self.pull(out)
    }

    unsafe fn pull(&self, out: &mut SampleSink) -> Result<(), String> {
        loop {
            let buf = MFCreateMemoryBuffer(self.out_size).map_err(e("AAC"))?;
            let s = MFCreateSample().map_err(e("AAC"))?;
            s.AddBuffer(&buf).map_err(e("AAC"))?;
            let mut db = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(Some(s.clone())),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0u32;
            let r = self.mft.ProcessOutput(0, &mut db, &mut status);
            ManuallyDrop::drop(&mut db[0].pSample);
            ManuallyDrop::drop(&mut db[0].pEvents);
            match r {
                Ok(()) => out(s)?,
                Err(x) if x.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
                Err(x) => return Err(e("Кодировщик AAC")(x)),
            }
        }
    }
}

type AacFrames = Receiver<Result<Mf<IMFSample>, String>>;

/// Encode one sound track of the clip (positions `a0`..`limit`) to AAC,
/// sending the frames (times relative to the clip start) as they come out.
fn encode_track(aac: &Aac, segs: &[&SegRef], track: usize, a0: u64, limit: u64, tx: &Sender<Result<Mf<IMFSample>, String>>) -> Result<(), String> {
    let frame = 1024 * HNS / crate::audio::RATE as i64;
    let mut k = 0i64;
    let mut send = |s: IMFSample| -> Result<(), String> {
        unsafe {
            if s.GetSampleTime().is_err() {
                let _ = s.SetSampleTime(k * frame);
                let _ = s.SetSampleDuration(frame);
            }
        }
        k += 1;
        tx.send(Ok(Mf(s))).map_err(|_| "сохранение прервано".to_string())
    };
    let mut o = AudioOut::new(a0);
    let kind = crate::avi::KIND_AUDIO + track as u8;
    for s in segs {
        crate::avi::for_each_record(&s.audio, |kd, pos, data| {
            if kd == kind {
                o.q.push_back((pos, data.to_vec()));
            }
            Ok(())
        })?;
        o.until(limit, &mut |smp| unsafe { aac.input(&smp, &mut send) })?;
    }
    unsafe { aac.finish(&mut send) }
}

/// One sound track of the clip being written.
enum Audio {
    /// Already-encoded AAC frames coming from an encoder thread.
    Enc { rx: AacFrames, peek: Option<(i64, IMFSample)>, idx: u32, done: bool },
    /// PCM, encoded by the file writer itself (when the AAC encoder could not be made).
    Pcm { o: AudioOut, idx: u32 },
}

impl Audio {
    /// Write the sound up to `rel` (clip time) / `pos` (buffer sample position).
    unsafe fn until(&mut self, sw: &IMFSinkWriter, rel: i64, pos: u64) -> Result<(), String> {
        match self {
            Audio::Pcm { o, idx } => {
                let idx = *idx;
                o.until(pos, &mut |s| sw.WriteSample(idx, &s).map_err(e("Звук")))
            }
            Audio::Enc { rx, peek, idx, done } => loop {
                if peek.is_none() {
                    if *done {
                        return Ok(());
                    }
                    match rx.recv() {
                        Ok(Ok(Mf(s))) => *peek = Some((s.GetSampleTime().unwrap_or(0), s)),
                        Ok(Err(x)) => return Err(x),
                        Err(_) => {
                            *done = true;
                            return Ok(());
                        }
                    }
                }
                if peek.as_ref().is_some_and(|(t, _)| *t > rel) {
                    return Ok(());
                }
                let (_, s) = peek.take().unwrap();
                sw.WriteSample(*idx, &s).map_err(e("Звук"))?;
            },
        }
    }
}

/// One buffer segment read back from disk: compressed frames inside the clip
/// range (with their buffer times) and, if asked for, its sound blocks.
struct Loaded {
    video: Vec<(IMFSample, i64)>,
    pcm: Vec<(usize, u64, Vec<u8>)>,
}

// Media Foundation samples are free-threaded; they are made on a reader
// thread and handed to the writing thread.
unsafe impl Send for Loaded {}

fn load_segment(s: &SegRef, base: i64, start: i64, end: i64, ntracks: usize) -> Result<Loaded, String> {
    let mut l = Loaded { video: Vec::new(), pcm: Vec::new() };
    if ntracks > 0 {
        crate::avi::for_each_record(&s.audio, |kind, pos, data| {
            let t = kind.wrapping_sub(crate::avi::KIND_AUDIO) as usize;
            if kind >= crate::avi::KIND_AUDIO && t < ntracks {
                l.pcm.push((t, pos, data.to_vec()));
            }
            Ok(())
        })?;
    }
    read_video(&s.mp4, |smp, ts, _| {
        let t = base + ts;
        if t > end {
            return Ok(false);
        }
        if t >= start {
            l.video.push((smp, t));
        }
        Ok(true)
    })?;
    Ok(l)
}

/// Testing only: save the old way (one reader, sound encoded by the writer).
pub static SEQUENTIAL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Join buffer segments into one MP4 for the time range in `spec`.
///
/// Everything runs at once: a few threads read the segments back from disk,
/// one thread per sound track encodes it to AAC, and this thread writes the
/// picture and the sound into the file in order. Nothing is re-encoded.
pub fn save_mp4(segs: &[SegRef], spec: &crate::avi::ClipSpec, out: &Path) -> Result<crate::avi::ClipResult, String> {
    startup()?;
    com_init();
    let fps = spec.fps as i64;
    let abs = |slot: u64| slot as i64 * HNS / fps;
    let want_start = spec.from_ms as i64 * 10_000;
    let want_end = spec.to_ms as i64 * 10_000;
    let segs: Vec<&SegRef> = segs.iter().filter(|s| s.mp4.exists()).collect();
    if segs.is_empty() {
        return Err("в буфере ещё нет кадров".into());
    }

    // Start at the last key frame at or before the wanted start.
    let mut start: Option<i64> = None;
    let mut first_key: Option<i64> = None;
    let mut video_type: Option<IMFMediaType> = None;
    for s in &segs {
        let base = abs(s.base_slot);
        let ty = read_video(&s.mp4, |_, ts, key| {
            let t = base + ts;
            if key {
                if t <= want_start {
                    start = Some(t);
                } else if first_key.is_none() {
                    first_key = Some(t);
                }
            }
            Ok(t <= want_start)
        })?;
        if video_type.is_none() {
            video_type = ty;
        }
        if first_key.is_some() {
            break;
        }
    }
    let start = start.or(first_key).ok_or("в буфере ещё нет ключевых кадров")?;
    let video_type = video_type.ok_or("не удалось прочитать буфер")?;

    let prof = std::env::var_os("RB_PROF").is_some();
    let t_all = std::time::Instant::now();
    let rate = crate::audio::RATE as i64;
    let a0 = (start * rate / HNS) as u64;
    let a_end = ((want_end + HNS / fps) * rate / HNS) as u64;
    let ntracks = spec.audio_tracks as usize;
    let seq = SEQUENTIAL.load(Ordering::Relaxed);
    let encoders: Option<Vec<(Aac, IMFMediaType)>> = if seq { None } else { (0..ntracks).map(|_| unsafe { Aac::new() }.ok()).collect() };

    unsafe {
        let sw = sink_writer(out, true, None).map_err(e("Файл MP4"))?;
        let vidx = sw.AddStream(&video_type).map_err(e("Видео"))?;
        sw.SetInputMediaType(vidx, &video_type, None).map_err(e("Видео"))?;
        // One AAC stream per sound track, in the clip's track order.
        let mut audio: Vec<Audio> = Vec::new();
        let mut encs: Vec<(Mf<Aac>, Sender<Result<Mf<IMFSample>, String>>)> = Vec::new();
        match encoders {
            Some(list) => {
                for (aac, ty) in list {
                    let a = sw.AddStream(&ty).map_err(e("AAC"))?;
                    sw.SetInputMediaType(a, &ty, None).map_err(e("AAC"))?;
                    let (tx, rx) = crossbeam_channel::unbounded();
                    audio.push(Audio::Enc { rx, peek: None, idx: a, done: false });
                    encs.push((Mf(aac), tx));
                }
            }
            None => {
                for _ in 0..ntracks {
                    let a = sw.AddStream(&aac_type().map_err(e("AAC"))?).map_err(e("AAC"))?;
                    sw.SetInputMediaType(a, &pcm_type().map_err(e("PCM"))?, None).map_err(e("Кодировщик AAC"))?;
                    audio.push(Audio::Pcm { o: AudioOut::new(a0), idx: a });
                }
            }
        }
        // Readers only bring the sound along when the writer encodes it.
        let reader_tracks = if encs.is_empty() { ntracks } else { 0 };
        sw.BeginWriting().map_err(e("Файл MP4"))?;

        let mut frames = 0u64;
        let mut last_t = start;
        let mut t_wait = std::time::Duration::ZERO;
        let workers = if seq { 1 } else { std::thread::available_parallelism().map_or(2, |n| n.get()).clamp(2, 4).min(segs.len()) };
        let next = AtomicUsize::new(0);
        let slots: Vec<_> = (0..segs.len()).map(|_| crossbeam_channel::bounded::<Result<Loaded, String>>(1)).collect();
        let (ticket_tx, ticket_rx) = crossbeam_channel::unbounded::<()>();
        for _ in 0..workers + 1 {
            let _ = ticket_tx.send(());
        }
        std::thread::scope(|sc| -> Result<(), String> {
            for (k, (aac, tx)) in encs.into_iter().enumerate() {
                let segs = &segs;
                sc.spawn(move || {
                    let aac = aac;
                    com_init();
                    if let Err(x) = encode_track(&aac.0, segs, k, a0, a_end, &tx) {
                        let _ = tx.send(Err(x));
                    }
                });
            }
            // Readers stay a few segments ahead of the writing.
            for _ in 0..workers {
                let (next, slots, segs, rx) = (&next, &slots, &segs, ticket_rx.clone());
                sc.spawn(move || {
                    com_init();
                    while rx.recv().is_ok() {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(s) = segs.get(i) else { break };
                        let base = s.base_slot as i64 * HNS / fps;
                        let _ = slots[i].0.send(load_segment(s, base, start, want_end, reader_tracks));
                    }
                });
            }
            drop(ticket_rx);
            // Owned here: an early return drops it (and the sound receivers),
            // which stops the other threads.
            let ticket_tx = ticket_tx;
            let audio = &mut audio;
            for (_, rx) in &slots {
                let tw = std::time::Instant::now();
                let l = rx.recv().map_err(|_| "сбой чтения буфера".to_string())??;
                t_wait += tw.elapsed();
                let _ = ticket_tx.send(());
                for (track, pos, data) in l.pcm {
                    if let Some(Audio::Pcm { o, .. }) = audio.get_mut(track) {
                        o.q.push_back((pos, data));
                    }
                }
                for (smp, t) in l.video {
                    smp.SetSampleTime(t - start).map_err(e("Видео"))?;
                    sw.WriteSample(vidx, &smp).map_err(e("Видео"))?;
                    frames += 1;
                    last_t = last_t.max(t);
                    // Keep the sound interleaved with the picture.
                    for a in audio.iter_mut() {
                        a.until(&sw, t - start, (t * rate / HNS) as u64)?;
                    }
                }
            }
            let end = last_t + HNS / fps;
            for a in audio.iter_mut() {
                a.until(&sw, end - start, (end * rate / HNS) as u64)?;
            }
            Ok(())
        })?;
        drop(audio);
        let tf = std::time::Instant::now();
        sw.Finalize().map_err(e("Завершение MP4"))?;
        if prof {
            println!(
                "prof: total {:?}, waiting for readers {t_wait:?}, finalize {:?}, readers {workers}, aac threads {}",
                t_all.elapsed(),
                tf.elapsed(),
                if reader_tracks == 0 { ntracks } else { 0 }
            );
        }
        let bytes = std::fs::metadata(out).map(|m| m.len()).unwrap_or(0);
        let length_frames = if frames == 0 { 0 } else { ((last_t - start) * fps / HNS) as u64 + 1 };
        Ok(crate::avi::ClipResult { frames: length_frames, bytes, truncated: false })
    }
}

/// Testing: decode a clip and return its last picture as BGRA (width, height, pixels).
pub fn last_frame_bgra(p: &Path) -> Result<(u32, u32, Vec<u8>), String> {
    startup()?;
    unsafe {
        let mut attrs: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut attrs, 1).map_err(e("attrs"))?;
        let attrs = attrs.unwrap();
        attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1).map_err(e("attrs"))?;
        let r = MFCreateSourceReaderFromURL(&HSTRING::from(p.as_os_str()), &attrs).map_err(e("open"))?;
        let vs = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
        let vt = MFCreateMediaType().map_err(e("t"))?;
        vt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(e("t"))?;
        vt.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32).map_err(e("t"))?;
        r.SetCurrentMediaType(vs, None, &vt).map_err(e("video out"))?;
        let size = r.GetCurrentMediaType(vs).map_err(e("cur"))?.GetUINT64(&MF_MT_FRAME_SIZE).map_err(e("size"))?;
        let (w, h) = ((size >> 32) as u32, (size & 0xffff_ffff) as u32);
        let mut last = Vec::new();
        loop {
            let mut flags = 0u32;
            let mut s: Option<IMFSample> = None;
            r.ReadSample(vs, 0, None, Some(&mut flags), None, Some(&mut s)).map_err(e("read"))?;
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                break;
            }
            let Some(s) = s else { continue };
            let buf = s.ConvertToContiguousBuffer().map_err(e("buf"))?;
            let mut ptr: *mut u8 = std::ptr::null_mut();
            let mut len = 0u32;
            buf.Lock(&mut ptr, None, Some(&mut len)).map_err(e("lock"))?;
            let n = (w * h * 4) as usize;
            if len as usize >= n {
                last = std::slice::from_raw_parts(ptr, n).to_vec();
            }
            buf.Unlock().map_err(e("unlock"))?;
        }
        if last.is_empty() {
            return Err("нет кадров".into());
        }
        Ok((w, h, last))
    }
}

/// Decode a saved clip like a player would: count frames and sound, report
/// gaps, and save a few frames as PNG (used for testing).
pub fn inspect(p: &Path, out_dir: &Path) -> Result<String, String> {
    startup()?;
    let _ = std::fs::create_dir_all(out_dir);
    unsafe {
        let mut attrs: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut attrs, 1).map_err(e("attrs"))?;
        let attrs = attrs.unwrap();
        attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1).map_err(e("attrs"))?;
        let r = MFCreateSourceReaderFromURL(&HSTRING::from(p.as_os_str()), &attrs).map_err(e("open"))?;
        let vs = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
        let as_ = MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32;
        let vt = MFCreateMediaType().map_err(e("t"))?;
        vt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(e("t"))?;
        vt.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32).map_err(e("t"))?;
        r.SetCurrentMediaType(vs, None, &vt).map_err(e("video out"))?;
        let has_audio = r.SetCurrentMediaType(as_, None, &pcm_type().map_err(e("t"))?).is_ok();
        let mut audio_streams = 0;
        for i in 0..16u32 {
            match r.GetNativeMediaType(i, 0) {
                Ok(t) => {
                    if t.GetGUID(&MF_MT_MAJOR_TYPE).ok() == Some(MFMediaType_Audio) {
                        audio_streams += 1;
                    }
                }
                Err(_) => break,
            }
        }
        let cur = r.GetCurrentMediaType(vs).map_err(e("cur"))?;
        let size = cur.GetUINT64(&MF_MT_FRAME_SIZE).map_err(e("size"))?;
        let (w, h) = ((size >> 32) as u32, (size & 0xffff_ffff) as u32);
        let mut frames = 0u64;
        let mut max_gap = 0i64;
        let mut last_v: Option<i64> = None;
        let mut audio_samples = 0u64;
        let mut peak = 0i16;
        let mut shots = vec![1i64, 9, 11, 19, 21].into_iter().map(|s| s * HNS).collect::<VecDeque<_>>();
        let mut saved = Vec::new();
        loop {
            let mut idx = 0u32;
            let mut flags = 0u32;
            let mut ts = 0i64;
            let mut s: Option<IMFSample> = None;
            r.ReadSample(MF_SOURCE_READER_ANY_STREAM.0 as u32, 0, Some(&mut idx), Some(&mut flags), Some(&mut ts), Some(&mut s))
                .map_err(e("read"))?;
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                // One stream ended; stop when nothing more comes.
                if s.is_none() {
                    let mut f2 = 0u32;
                    let mut s2: Option<IMFSample> = None;
                    let mut i2 = 0u32;
                    let mut t2 = 0i64;
                    if r.ReadSample(MF_SOURCE_READER_ANY_STREAM.0 as u32, 0, Some(&mut i2), Some(&mut f2), Some(&mut t2), Some(&mut s2)).is_err() || s2.is_none() {
                        break;
                    }
                    s = s2;
                    idx = i2;
                    ts = t2;
                }
            }
            let Some(s) = s else { continue };
            let buf = s.ConvertToContiguousBuffer().map_err(e("buf"))?;
            let mut ptr: *mut u8 = std::ptr::null_mut();
            let mut len = 0u32;
            buf.Lock(&mut ptr, None, Some(&mut len)).map_err(e("lock"))?;
            let data = std::slice::from_raw_parts(ptr, len as usize);
            let is_video = r.GetCurrentMediaType(idx).ok().and_then(|t| t.GetGUID(&MF_MT_MAJOR_TYPE).ok()) == Some(MFMediaType_Video);
            if is_video {
                frames += 1;
                if let Some(l) = last_v {
                    max_gap = max_gap.max(ts - l);
                }
                last_v = Some(ts);
                if shots.front().is_some_and(|&t| ts >= t) {
                    shots.pop_front();
                    let path = out_dir.join(format!("frame_{:05.1}s.png", ts as f64 / 1e7));
                    let row = (w * 4) as usize;
                    if data.len() >= row * h as usize {
                        let _ = crate::capture::save_png(&data[..row * h as usize], w, h, &path);
                        saved.push(path.file_name().unwrap().to_string_lossy().into_owned());
                    }
                }
            } else {
                audio_samples += (len / 4) as u64;
                for c in data.chunks_exact(2) {
                    let v = i16::from_le_bytes([c[0], c[1]]);
                    peak = peak.max(v.saturating_abs());
                }
            }
            buf.Unlock().map_err(e("unlock"))?;
        }
        Ok(format!(
            "{w}x{h}, audio tracks {audio_streams}, video frames {frames}, last frame at {:.2}s, biggest frame gap {:.0} ms, audio {} ({:.2}s, peak {}), saved {:?}",
            last_v.unwrap_or(0) as f64 / 1e7,
            max_gap as f64 / 1e4,
            if has_audio { "yes" } else { "no" },
            audio_samples as f64 / crate::audio::RATE as f64,
            peak,
            saved
        ))
    }
}

/// Duration of an MP4 file in seconds.
pub fn mp4_duration(p: &Path) -> Option<f64> {
    startup().ok()?;
    unsafe {
        let r = MFCreateSourceReaderFromURL(&HSTRING::from(p.as_os_str()), None).ok()?;
        let v = r
            .GetPresentationAttribute(MF_SOURCE_READER_MEDIASOURCE.0 as u32, &MF_PD_DURATION)
            .ok()?;
        let d: u64 = u64::try_from(&v).ok()?;
        Some(d as f64 / 1e7)
    }
}

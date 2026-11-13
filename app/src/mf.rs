//! H.264 / MP4 through Windows Media Foundation (uses the GPU encoder when
//! there is one). The buffer keeps short video-only MP4 segments; saving
//! copies their compressed frames into one MP4 without re-encoding and
//! encodes the sound to AAC in one go (so there are no gaps between parts).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;
use crate::settings::Codec;
use std::mem::ManuallyDrop;
use crossbeam_channel::{Receiver, Sender};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
use windows::core::{Interface, GUID, HSTRING};
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
    pub fn create(path: &Path, w: u32, h: u32, fps: u32, bitrate: u32, codec: Codec) -> Result<Self, String> {
        Self::create_with(path, w, h, fps, bitrate, codec, true, None).or_else(|_| Self::create_with(path, w, h, fps, bitrate, codec, false, None))
    }

    /// A writer that takes NV12 video-card textures (see `gpu`), hardware encoder only.
    pub fn create_gpu(path: &Path, w: u32, h: u32, fps: u32, bitrate: u32, codec: Codec, d3d: &IMFDXGIDeviceManager) -> Result<Self, String> {
        Self::create_with(path, w, h, fps, bitrate, codec, true, Some(d3d))
    }

    #[allow(clippy::too_many_arguments)]
    fn create_with(path: &Path, w: u32, h: u32, fps: u32, bitrate: u32, codec: Codec, hw: bool, d3d: Option<&IMFDXGIDeviceManager>) -> Result<Self, String> {
        startup()?;
        unsafe {
            let sw = sink_writer(path, hw, d3d).map_err(e("Запись MP4"))?;
            let (subtype, profile, name) = match codec {
                Codec::H264 => (MFVideoFormat_H264, eAVEncH264VProfile_High.0 as u32, "H.264"),
                Codec::Hevc => (MFVideoFormat_HEVC, eAVEncH265VProfile_Main_420_8.0 as u32, "HEVC"),
            };
            let out = video_type(&subtype, w, h, fps).map_err(e(name))?;
            out.SetUINT32(&MF_MT_AVG_BITRATE, bitrate).map_err(e(name))?;
            out.SetUINT32(&MF_MT_MPEG2_PROFILE, profile).map_err(e(name))?;
            let _ = out.SetUINT32(&MF_MT_MAX_KEYFRAME_SPACING, fps);
            let stream = sw.AddStream(&out).map_err(e("H.264"))?;
            let inp = video_type(&MFVideoFormat_NV12, w, h, fps).map_err(e("NV12"))?;
            inp.SetUINT32(&MF_MT_DEFAULT_STRIDE, w).map_err(e("NV12"))?;
            let _ = inp.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32);
            // Variable bitrate: `bitrate` on average, up to 1.5× in busy scenes,
            // and far less when little moves (constant bitrate would pad still
            // scenes up to the full rate). Encoders that refuse it keep their own.
            let mut params: Option<IMFAttributes> = None;
            if MFCreateAttributes(&mut params, 3).is_ok() {
                let p = params.as_ref().unwrap();
                let _ = p.SetUINT32(&CODECAPI_AVEncCommonRateControlMode, eAVEncCommonRateControlMode_PeakConstrainedVBR.0 as u32);
                let _ = p.SetUINT32(&CODECAPI_AVEncCommonMeanBitRate, bitrate);
                let _ = p.SetUINT32(&CODECAPI_AVEncCommonMaxBitRate, bitrate.saturating_mul(3) / 2);
            }
            if sw.SetInputMediaType(stream, &inp, params.as_ref()).is_err() {
                sw.SetInputMediaType(stream, &inp, None).map_err(e("Кодировщик H.264"))?;
            }
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

    /// Testing: the rate control mode the encoder actually uses
    /// (0 constant, 1 variable with a peak, 2 variable, 3 quality).
    pub fn rate_mode(&self) -> Option<u32> {
        unsafe {
            let ex: IMFSinkWriterEx = self.w.cast().ok()?;
            for i in 0..8 {
                let mut cat = GUID::zeroed();
                let mut t: Option<IMFTransform> = None;
                if ex.GetTransformForStream(self.stream, i, Some(&mut cat), &mut t).is_err() {
                    break;
                }
                if let Some(api) = t.and_then(|t| t.cast::<ICodecAPI>().ok()) {
                    if let Ok(v) = api.GetValue(&CODECAPI_AVEncCommonRateControlMode) {
                        return u32::try_from(&v).ok();
                    }
                }
            }
            None
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
pub fn probe(dir: &Path, w: u32, h: u32, fps: u32, bitrate: u32, codec: Codec) -> Result<(), String> {
    let _ = std::fs::create_dir_all(dir);
    let p = dir.join("probe.mp4");
    let mut v = VideoSegWriter::create(&p, w, h, fps, bitrate, codec)?;
    let frame = vec![128u8; (w * h * 3 / 2) as usize];
    for i in 0..3 {
        v.write(&frame, i)?;
    }
    if std::env::var_os("RB_RATE_MODE").is_some() {
        println!("encoder rate control mode: {:?} (1 = variable with a peak)", v.rate_mode());
    }
    let r = v.finish();
    let _ = std::fs::remove_file(&p);
    r
}

pub fn bitrate(w: u32, h: u32, fps: u32, q: crate::settings::Quality, codec: Codec) -> u32 {
    // HEVC needs about 60 % of the bits for the same picture.
    let codec_factor = if codec == Codec::Hevc { 0.6 } else { 1.0 };
    let bpp = match q {
        crate::settings::Quality::Low => 0.08,
        crate::settings::Quality::Medium => 0.15,
        crate::settings::Quality::High => 0.25,
    };
    // Higher frame rates need fewer bits per frame (frames are more alike).
    let fps_factor = 30.0 * (fps as f64 / 30.0).powf(0.6);
    ((w as f64 * h as f64 * fps_factor * bpp * codec_factor) as u32).clamp(1_000_000, 100_000_000)
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

/// Where one sound track of the clip comes from in the buffer.
#[derive(Clone)]
enum TrackSrc {
    /// Stored as it is (index of the stored track).
    Stored(u8),
    /// "Всё вместе": not stored; summed from the stored tracks with these weights.
    Mix(Vec<f32>),
}

fn track_src(spec: &crate::avi::ClipSpec, k: usize) -> TrackSrc {
    match &spec.mix_weights {
        Some(w) if k == 0 => TrackSrc::Mix(w.clone()),
        Some(_) => TrackSrc::Stored(k as u8 - 1),
        None => TrackSrc::Stored(k as u8),
    }
}

fn to_pcm(acc: &[f32]) -> Vec<u8> {
    acc.iter().flat_map(|v| (v.round().clamp(-32768.0, 32767.0) as i16).to_le_bytes()).collect()
}

/// The sound blocks (position, PCM) of one clip track in a buffer file.
fn track_blocks(path: &Path, src: &TrackSrc, out: &mut dyn FnMut(u64, Vec<u8>)) -> Result<(), String> {
    let audio = crate::avi::KIND_AUDIO;
    match src {
        TrackSrc::Stored(t) => crate::avi::for_each_record(path, |kd, pos, data| {
            if kd == audio + t {
                out(pos, data.to_vec());
            }
            Ok(())
        }),
        TrackSrc::Mix(w) => {
            // The stored tracks are written block by block at the same
            // positions, so blocks with equal positions are added together.
            let mut cur: Option<(u64, Vec<f32>)> = None;
            let r = crate::avi::for_each_record(path, |kd, pos, data| {
                let t = kd.wrapping_sub(audio) as usize;
                if kd < audio || t >= w.len() {
                    return Ok(());
                }
                if cur.as_ref().is_some_and(|c| c.0 != pos || c.1.len() != data.len() / 2) {
                    let (p, acc) = cur.take().unwrap();
                    out(p, to_pcm(&acc));
                }
                let acc = &mut cur.get_or_insert_with(|| (pos, vec![0.0; data.len() / 2])).1;
                for (i, s) in data.chunks_exact(2).enumerate() {
                    acc[i] += i16::from_le_bytes([s[0], s[1]]) as f32 * w[t];
                }
                Ok(())
            });
            if let Some((p, acc)) = cur {
                out(p, to_pcm(&acc));
            }
            r
        }
    }
}

/// Samples in one AAC frame.
const AAC_FRAME: u64 = 1024;
/// Extra frames a piece encodes before and after its own part, so the
/// encoder is settled at the joins (their output is thrown away).
const AAC_ROLL: u64 = 2;

/// One part of a sound track, encoded by its own AAC encoder: frames
/// `from`..`to` counted from the clip's first sound sample. The last part
/// runs to the end of the track.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Piece {
    from: u64,
    to: u64,
    last: bool,
}

/// Cut `frames` AAC frames into `n` pieces of nearly equal length.
fn pieces(frames: u64, n: usize) -> Vec<Piece> {
    let n = (n as u64).clamp(1, frames.max(1));
    let per = frames.div_ceil(n);
    (0..n)
        .map(|j| Piece { from: j * per, to: ((j + 1) * per).min(frames), last: j + 1 == n })
        .filter(|p| p.from < p.to || p.last)
        .collect()
}

/// How many pieces each of `tracks` sound tracks is cut into: enough to keep
/// the processor's cores busy, but no piece shorter than about 15 seconds.
fn piece_count(tracks: usize, samples: u64) -> usize {
    let cores = std::thread::available_parallelism().map_or(2, |n| n.get());
    let by_cores = (cores / tracks.max(1)).max(1);
    let by_len = (samples / (crate::audio::RATE as u64 * 15)).max(1) as usize;
    by_cores.min(by_len).min(8)
}

/// The segments a piece needs: those covering sample positions `lo`..`hi`,
/// plus one more on each side (sound may sit a little off the picture).
fn piece_segs<'a>(segs: &[&'a SegRef], fps: i64, lo: u64, hi: u64) -> Vec<&'a SegRef> {
    let rate = crate::audio::RATE as i64;
    let pos = |s: &SegRef| (s.base_slot as i64 * rate / fps) as u64;
    let first = segs.iter().rposition(|s| pos(s) <= lo).unwrap_or(0).saturating_sub(1);
    let last = segs.iter().position(|s| pos(s) >= hi).map_or(segs.len(), |i| (i + 2).min(segs.len()));
    segs[first..last.max(first)].to_vec()
}

/// Encode one piece of a sound track to AAC (the clip's sound starts at
/// buffer position `a0`; nothing at or after `limit` is used), sending its
/// frames, stamped with their clip times, as they come out.
fn encode_piece(aac: &Aac, segs: &[&SegRef], src: &TrackSrc, a0: u64, limit: u64, p: Piece, tx: &Sender<Result<Mf<IMFSample>, String>>) -> Result<(), String> {
    let frame = AAC_FRAME as i64 * HNS / crate::audio::RATE as i64;
    let pre = p.from.saturating_sub(AAC_ROLL);
    let stop = if p.last { limit } else { (a0 + (p.to + AAC_ROLL) * AAC_FRAME).min(limit) };
    let mut g = pre;
    let mut send = |s: IMFSample| -> Result<(), String> {
        let k = g;
        g += 1;
        if k < p.from || (!p.last && k >= p.to) {
            return Ok(());
        }
        unsafe {
            let _ = s.SetSampleTime(k as i64 * frame);
            let _ = s.SetSampleDuration(frame);
        }
        tx.send(Ok(Mf(s))).map_err(|_| "сохранение прервано".to_string())
    };
    let mut o = AudioOut::new(a0 + pre * AAC_FRAME);
    for s in segs {
        track_blocks(&s.audio, src, &mut |pos, data| o.q.push_back((pos, data)))?;
        o.until(stop, &mut |smp| unsafe { aac.input(&smp, &mut send) })?;
    }
    unsafe { aac.finish(&mut send) }
}

/// One sound track of the clip being written.
enum Audio {
    /// Already-encoded AAC frames coming from the encoder threads, one
    /// channel per piece of the track, taken in order.
    Enc { rx: VecDeque<AacFrames>, peek: Option<(i64, IMFSample)>, idx: u32 },
    /// PCM, encoded by the file writer itself (when the AAC encoder could not be made).
    Pcm { o: AudioOut, idx: u32 },
}

impl Audio {
    /// Write the sound up to `rel` (clip time) / `pos` (buffer sample position)
    /// through `write(stream, sample)`.
    unsafe fn until(&mut self, write: &mut dyn FnMut(u32, &IMFSample) -> Result<(), String>, rel: i64, pos: u64) -> Result<(), String> {
        match self {
            Audio::Pcm { o, idx } => {
                let idx = *idx;
                o.until(pos, &mut |s| write(idx, &s))
            }
            Audio::Enc { rx, peek, idx } => loop {
                if peek.is_none() {
                    let Some(r) = rx.front() else { return Ok(()) };
                    match r.recv() {
                        Ok(Ok(Mf(s))) => *peek = Some((s.GetSampleTime().unwrap_or(0), s)),
                        Ok(Err(x)) => return Err(x),
                        // This piece is done: go on with the next one.
                        Err(_) => {
                            rx.pop_front();
                            continue;
                        }
                    }
                }
                if peek.as_ref().is_some_and(|(t, _)| *t > rel) {
                    return Ok(());
                }
                let (_, s) = peek.take().unwrap();
                write(*idx, &s)?;
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

fn load_segment(s: &SegRef, base: i64, start: i64, end: i64, srcs: &[TrackSrc]) -> Result<Loaded, String> {
    let mut l = Loaded { video: Vec::new(), pcm: Vec::new() };
    for (k, src) in srcs.iter().enumerate() {
        track_blocks(&s.audio, src, &mut |pos, data| l.pcm.push((k, pos, data)))?;
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

/// AAC encoders for `ntracks` sound tracks of buffer positions `a0`..`a_end`:
/// each track is cut into pieces encoded at the same time by their own
/// encoders (one piece if that many encoders cannot be made).
fn aac_encoders(ntracks: usize, a0: u64, a_end: u64) -> Option<(Vec<Piece>, Vec<Vec<(Aac, IMFMediaType)>>)> {
    let samples = a_end.saturating_sub(a0);
    let make = |n: usize| -> Option<(Vec<Piece>, Vec<Vec<(Aac, IMFMediaType)>>)> {
        let plan = pieces(samples.div_ceil(AAC_FRAME), n);
        let encs = (0..ntracks).map(|_| plan.iter().map(|_| unsafe { Aac::new() }.ok()).collect::<Option<Vec<_>>>()).collect::<Option<Vec<_>>>()?;
        Some((plan, encs))
    };
    make(piece_count(ntracks, samples)).or_else(|| make(1))
}

/// Start the encoder threads of `aac_encoders` (inside scope `sc`); returns
/// for each track the channels its pieces send frames on, in order.
fn spawn_encoders<'s, 'e>(
    sc: &'s std::thread::Scope<'s, 'e>,
    plan: &[Piece],
    encs: Vec<Vec<Mf<Aac>>>,
    segs: &[&'e SegRef],
    srcs: &[TrackSrc],
    fps: i64,
    a0: u64,
    a_end: u64,
) -> Vec<VecDeque<AacFrames>> {
    let mut out = Vec::new();
    for (track, track_encs) in encs.into_iter().enumerate() {
        let mut rxs = VecDeque::new();
        for (&p, aac) in plan.iter().zip(track_encs) {
            let (tx, rx) = crossbeam_channel::unbounded();
            rxs.push_back(rx);
            let lo = a0 + p.from.saturating_sub(AAC_ROLL) * AAC_FRAME;
            let hi = if p.last { a_end } else { a0 + (p.to + AAC_ROLL) * AAC_FRAME };
            let segs = piece_segs(segs, fps, lo, hi);
            let src = srcs[track].clone();
            sc.spawn(move || {
                let aac = aac;
                com_init();
                if let Err(x) = encode_piece(&aac.0, &segs, &src, a0, a_end, p, &tx) {
                    let _ = tx.send(Err(x));
                }
            });
        }
        out.push(rxs);
    }
    out
}

/// Testing only: save the old way (one reader, sound encoded by the writer).
pub static SEQUENTIAL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Testing only: always save through Media Foundation.
pub static NO_COPY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Join buffer segments into one MP4 for the time range in `spec`.
///
/// The fast way copies the frames straight from the segment files (see
/// `mp4`); if a segment is not what it expects, the clip is made again
/// through Media Foundation.
pub fn save_mp4(segs: &[SegRef], spec: &crate::avi::ClipSpec, out: &Path) -> Result<crate::avi::ClipResult, String> {
    if !SEQUENTIAL.load(Ordering::Relaxed) && !NO_COPY.load(Ordering::Relaxed) {
        match save_copy(segs, spec, out) {
            Ok(r) => return Ok(r),
            Err(x) => {
                eprintln!("fast save failed ({x}), saving through Media Foundation");
                let _ = std::fs::remove_file(out);
            }
        }
    }
    save_mf(segs, spec, out)
}

/// The bytes of a Media Foundation sample.
unsafe fn sample_bytes(s: &IMFSample) -> Result<Vec<u8>, String> {
    let buf = s.ConvertToContiguousBuffer().map_err(e("AAC"))?;
    let mut p: *mut u8 = std::ptr::null_mut();
    let mut len = 0u32;
    buf.Lock(&mut p, None, Some(&mut len)).map_err(e("AAC"))?;
    let v = std::slice::from_raw_parts(p, len as usize).to_vec();
    buf.Unlock().map_err(e("AAC"))?;
    Ok(v)
}

/// Save by copying: segment files are read whole by a few threads, the
/// sound is encoded to AAC by the piece threads, and this thread writes
/// frames and sound in order with `mp4::Writer`.
fn save_copy(segs: &[SegRef], spec: &crate::avi::ClipSpec, out: &Path) -> Result<crate::avi::ClipResult, String> {
    startup()?;
    com_init();
    let prof = std::env::var_os("RB_PROF").is_some();
    let t_all = std::time::Instant::now();
    let fps = spec.fps as i64;
    let abs = |slot: u64| slot as i64 * HNS / fps;
    let want_start = spec.from_ms as i64 * 10_000;
    let want_end = spec.to_ms as i64 * 10_000;
    let segs: Vec<&SegRef> = segs.iter().filter(|s| s.mp4.exists()).collect();
    if segs.is_empty() {
        return Err("в буфере ещё нет кадров".into());
    }
    let hns = |seg: &crate::mp4::Segment, f: &crate::mp4::Frame| (f.time as i128 * HNS as i128 / seg.timescale as i128) as i64;

    // Start at the last key frame at or before the wanted start; the
    // segments read for this are kept for the writing.
    let mut first: Vec<crate::mp4::Segment> = Vec::new();
    let mut start: Option<i64> = None;
    let mut first_key: Option<i64> = None;
    for s in &segs {
        let seg = crate::mp4::read(&s.mp4)?;
        let base = abs(s.base_slot);
        for f in seg.frames.iter().filter(|f| f.key) {
            let t = base + hns(&seg, f);
            if t <= want_start {
                start = Some(t);
            } else if first_key.is_none() {
                first_key = Some(t);
            }
        }
        first.push(seg);
        if first_key.is_some() {
            break;
        }
    }
    let start = start.or(first_key).ok_or("в буфере ещё нет ключевых кадров")?;
    let (timescale, stsd) = (first[0].timescale, first[0].stsd.clone());
    let to_ts = |t: i64| ((t - start) as i128 * timescale as i128 / HNS as i128) as u64;

    let rate = crate::audio::RATE as i64;
    let a0 = (start * rate / HNS) as u64;
    let a_end = ((want_end + HNS / fps) * rate / HNS) as u64;
    let ntracks = spec.audio_tracks as usize;
    let (plan, encoders) = aac_encoders(ntracks, a0, a_end).ok_or("нет кодировщика AAC")?;
    let srcs: Vec<TrackSrc> = (0..ntracks).map(|k| track_src(spec, k)).collect();

    let mut w = crate::mp4::Writer::create(out).map_err(|x| format!("Файл MP4: {x}"))?;
    let v = w.add_video(stsd.clone(), timescale, (timescale as i64 / fps).max(1) as u32);
    let mut audio: Vec<Audio> = (0..ntracks)
        .map(|_| Audio::Enc { rx: VecDeque::new(), peek: None, idx: w.add_aac(crate::audio::RATE, 2) as u32 })
        .collect();
    let encs: Vec<Vec<Mf<Aac>>> = encoders.into_iter().map(|t| t.into_iter().map(|(a, _)| Mf(a)).collect()).collect();

    let mut frames = 0u64;
    let mut last_t = start;
    let mut t_wait = std::time::Duration::ZERO;
    let mut t_audio = std::time::Duration::ZERO;
    let n_first = first.len();
    let rest = segs.len() - n_first;
    let workers = std::thread::available_parallelism().map_or(2, |n| n.get()).clamp(2, 4).min(rest.max(1));
    let next = AtomicUsize::new(n_first);
    let slots: Vec<_> = (0..segs.len()).map(|_| crossbeam_channel::bounded::<Result<crate::mp4::Segment, String>>(1)).collect();
    let (ticket_tx, ticket_rx) = crossbeam_channel::unbounded::<()>();
    for _ in 0..workers + 1 {
        let _ = ticket_tx.send(());
    }
    let aac_frame = AAC_FRAME as f64 * HNS as f64 / rate as f64;
    std::thread::scope(|sc| -> Result<(), String> {
        for (a, rxs) in audio.iter_mut().zip(spawn_encoders(sc, &plan, encs, &segs, &srcs, fps, a0, a_end)) {
            if let Audio::Enc { rx, .. } = a {
                *rx = rxs;
            }
        }
        for _ in 0..workers {
            let (next, slots, segs, rx) = (&next, &slots, &segs, ticket_rx.clone());
            sc.spawn(move || {
                while rx.recv().is_ok() {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(s) = segs.get(i) else { break };
                    let _ = slots[i].0.send(crate::mp4::read(&s.mp4));
                }
            });
        }
        drop(ticket_rx);
        // Owned here: an early return drops it, which stops the readers.
        let ticket_tx = ticket_tx;
        let mut first = first.into_iter();
        let write_audio = |w: &mut crate::mp4::Writer, i: u32, s: &IMFSample| -> Result<(), String> {
            let k = (unsafe { s.GetSampleTime() }.unwrap_or(0) as f64 / aac_frame).round() as u64;
            let data = unsafe { sample_bytes(s) }?;
            w.write(i as usize, &data, k * AAC_FRAME, true).map_err(|x| format!("Запись клипа: {x}"))
        };
        for (i, s) in segs.iter().enumerate() {
            let seg = match first.next() {
                Some(seg) => seg,
                None => {
                    let tw = std::time::Instant::now();
                    let seg = slots[i].1.recv().map_err(|_| "сбой чтения буфера".to_string())??;
                    t_wait += tw.elapsed();
                    let _ = ticket_tx.send(());
                    seg
                }
            };
            if seg.timescale != timescale || seg.stsd != stsd {
                return Err("части буфера записаны с разными настройками".into());
            }
            let base = abs(s.base_slot);
            for f in &seg.frames {
                let t = base + hns(&seg, f);
                if t > want_end {
                    break;
                }
                if t < start {
                    continue;
                }
                w.write(v, &seg.data[f.off..f.off + f.size as usize], to_ts(t), f.key).map_err(|x| format!("Запись клипа: {x}"))?;
                frames += 1;
                last_t = last_t.max(t);
                // Keep the sound interleaved with the picture.
                let ta = std::time::Instant::now();
                for a in audio.iter_mut() {
                    unsafe { a.until(&mut |i, smp| write_audio(&mut w, i, smp), t - start, (t * rate / HNS) as u64)? };
                }
                t_audio += ta.elapsed();
            }
        }
        let end = last_t + HNS / fps;
        for a in audio.iter_mut() {
            unsafe { a.until(&mut |i, smp| write_audio(&mut w, i, smp), end - start, (end * rate / HNS) as u64)? };
        }
        Ok(())
    })?;
    drop(audio);
    if frames == 0 {
        return Err("в буфере нет кадров для клипа".into());
    }
    let tf = std::time::Instant::now();
    let bytes = w.finish().map_err(|x| format!("Запись клипа: {x}"))?;
    if prof {
        println!(
            "prof copy: total {:?}, waiting for readers {t_wait:?}, audio {t_audio:?}, finish {:?}, readers {workers}, aac threads {}",
            t_all.elapsed(),
            tf.elapsed(),
            ntracks * plan.len()
        );
    }
    let length_frames = ((last_t - start) * fps / HNS) as u64 + 1;
    Ok(crate::avi::ClipResult { frames: length_frames, bytes, truncated: false })
}

/// Join buffer segments into one MP4 through Media Foundation.
///
/// Everything runs at once: a few threads read the segments back from disk,
/// the sound is encoded to AAC by the piece threads, and this thread writes
/// the picture and the sound into the file in order. Nothing is re-encoded.
fn save_mf(segs: &[SegRef], spec: &crate::avi::ClipSpec, out: &Path) -> Result<crate::avi::ClipResult, String> {
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
    let encoders = if seq { None } else { aac_encoders(ntracks, a0, a_end) };

    unsafe {
        let sw = sink_writer(out, true, None).map_err(e("Файл MP4"))?;
        let vidx = sw.AddStream(&video_type).map_err(e("Видео"))?;
        sw.SetInputMediaType(vidx, &video_type, None).map_err(e("Видео"))?;
        // One AAC stream per sound track, in the clip's track order.
        let mut audio: Vec<Audio> = Vec::new();
        let mut encs: Vec<Vec<Mf<Aac>>> = Vec::new();
        let mut plan = Vec::new();
        match encoders {
            Some((p, list)) => {
                plan = p;
                for track_encs in list {
                    let ty = &track_encs[0].1;
                    let a = sw.AddStream(ty).map_err(e("AAC"))?;
                    sw.SetInputMediaType(a, ty, None).map_err(e("AAC"))?;
                    audio.push(Audio::Enc { rx: VecDeque::new(), peek: None, idx: a });
                    encs.push(track_encs.into_iter().map(|(aac, _)| Mf(aac)).collect());
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
        // Where each clip track comes from in the buffer ("всё вместе" may be
        // summed from the others). Readers only bring the sound along when
        // the writer encodes it.
        let srcs: Vec<TrackSrc> = (0..ntracks).map(|k| track_src(spec, k)).collect();
        let reader_srcs: Vec<TrackSrc> = if encs.is_empty() { srcs.clone() } else { Vec::new() };
        let reader_tracks = reader_srcs.len();
        sw.BeginWriting().map_err(e("Файл MP4"))?;

        let mut frames = 0u64;
        let mut last_t = start;
        let mut t_wait = std::time::Duration::ZERO;
        let mut t_audio = std::time::Duration::ZERO;
        let workers = if seq { 1 } else { std::thread::available_parallelism().map_or(2, |n| n.get()).clamp(2, 4).min(segs.len()) };
        let next = AtomicUsize::new(0);
        let slots: Vec<_> = (0..segs.len()).map(|_| crossbeam_channel::bounded::<Result<Loaded, String>>(1)).collect();
        let (ticket_tx, ticket_rx) = crossbeam_channel::unbounded::<()>();
        for _ in 0..workers + 1 {
            let _ = ticket_tx.send(());
        }
        std::thread::scope(|sc| -> Result<(), String> {
            for (a, rxs) in audio.iter_mut().zip(spawn_encoders(sc, &plan, encs, &segs, &srcs, fps, a0, a_end)) {
                if let Audio::Enc { rx, .. } = a {
                    *rx = rxs;
                }
            }
            // Readers stay a few segments ahead of the writing.
            for _ in 0..workers {
                let (next, slots, segs, rx, reader_srcs) = (&next, &slots, &segs, ticket_rx.clone(), &reader_srcs);
                sc.spawn(move || {
                    com_init();
                    while rx.recv().is_ok() {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(s) = segs.get(i) else { break };
                        let base = s.base_slot as i64 * HNS / fps;
                        let _ = slots[i].0.send(load_segment(s, base, start, want_end, reader_srcs));
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
                    let ta = std::time::Instant::now();
                    // Keep the sound interleaved with the picture.
                    for a in audio.iter_mut() {
                        a.until(&mut |i, s| sw.WriteSample(i, s).map_err(e("Звук")), t - start, (t * rate / HNS) as u64)?;
                    }
                    t_audio += ta.elapsed();
                }
            }
            let end = last_t + HNS / fps;
            for a in audio.iter_mut() {
                a.until(&mut |i, s| sw.WriteSample(i, s).map_err(e("Звук")), end - start, (end * rate / HNS) as u64)?;
            }
            Ok(())
        })?;
        drop(audio);
        let tf = std::time::Instant::now();
        sw.Finalize().map_err(e("Завершение MP4"))?;
        if prof {
            println!(
                "prof: total {:?}, waiting for readers {t_wait:?}, audio {t_audio:?}, finalize {:?}, readers {workers}, aac threads {}",
                t_all.elapsed(),
                tf.elapsed(),
                if reader_tracks == 0 { ntracks * plan.len() } else { 0 }
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

#[cfg(test)]
mod tests {
    use super::AudioOut;

    /// "Всё вместе" made at saving time is the weighted sum of the stored
    /// tracks, block by block, clamped to 16 bits.
    #[test]
    fn pieces_cover_every_frame_once() {
        for (frames, n) in [(10u64, 3usize), (9, 3), (2, 5), (0, 3), (5000, 4)] {
            let p = super::pieces(frames, n);
            assert!(p.last().unwrap().last);
            assert_eq!(p[0].from, 0);
            assert!(p.windows(2).all(|w| w[0].to == w[1].from && !w[0].last));
            assert_eq!(p.last().unwrap().to, frames);
        }
    }

    #[test]
    fn mix_is_summed_from_stored_tracks() {
        use super::{track_blocks, TrackSrc};
        use crate::avi::{write_record, KIND_AUDIO};
        let path = std::env::temp_dir().join(format!("rewind-mix-test-{}.buf", std::process::id()));
        let block = |v: i16| v.to_le_bytes().repeat(4); // 4 samples
        {
            let mut f = std::io::BufWriter::new(std::fs::File::create(&path).unwrap());
            for pos in [0u64, 4] {
                // game, chat, mic for each block, as the audio thread writes them
                write_record(&mut f, KIND_AUDIO, pos, &block(1000)).unwrap();
                write_record(&mut f, KIND_AUDIO + 1, pos, &block(200)).unwrap();
                write_record(&mut f, KIND_AUDIO + 2, pos, &block(30000)).unwrap();
            }
        }
        let mut got = Vec::new();
        track_blocks(&path, &TrackSrc::Mix(vec![1.0, 1.0, 1.0]), &mut |pos, d| got.push((pos, d))).unwrap();
        let mut no_chat = Vec::new();
        track_blocks(&path, &TrackSrc::Mix(vec![1.0, 0.0, 0.0]), &mut |pos, d| no_chat.push((pos, d))).unwrap();
        let mut chat = Vec::new();
        track_blocks(&path, &TrackSrc::Stored(1), &mut |pos, d| chat.push((pos, d))).unwrap();
        let _ = std::fs::remove_file(&path);
        // 1000 + 200 + 30000 = 31200 (fits); game alone = 1000; stored chat unchanged.
        assert_eq!(got, vec![(0, block(31200)), (4, block(31200))]);
        assert_eq!(no_chat, vec![(0, block(1000)), (4, block(1000))]);
        assert_eq!(chat, vec![(0, block(200)), (4, block(200))]);
        // Clamped when the sum doesn't fit.
        let mut loud = Vec::new();
        let p2 = std::env::temp_dir().join(format!("rewind-mix-test2-{}.buf", std::process::id()));
        {
            let mut f = std::io::BufWriter::new(std::fs::File::create(&p2).unwrap());
            write_record(&mut f, KIND_AUDIO, 0, &block(30000)).unwrap();
            write_record(&mut f, KIND_AUDIO + 1, 0, &block(30000)).unwrap();
        }
        track_blocks(&p2, &TrackSrc::Mix(vec![1.0, 1.0]), &mut |pos, d| loud.push((pos, d))).unwrap();
        let _ = std::fs::remove_file(&p2);
        assert_eq!(loud, vec![(0, block(i16::MAX))]);
    }

    /// Sound blocks with a gap: the gap is filled with silence and nothing
    /// goes past the limit.
    #[test]
    fn audio_gaps_become_silence() {
        super::startup().unwrap();
        let mut o = AudioOut::new(1000);
        o.q.push_back((1000, vec![1u8; 100 * 4]));
        o.q.push_back((1300, vec![2u8; 100 * 4]));
        let mut got: Vec<(i64, usize, u8)> = Vec::new();
        o.until(1350, &mut |s| {
            unsafe {
                let b = s.ConvertToContiguousBuffer().unwrap();
                let (mut p, mut n) = (std::ptr::null_mut(), 0u32);
                b.Lock(&mut p, None, Some(&mut n)).unwrap();
                got.push((s.GetSampleTime().unwrap(), n as usize / 4, *p));
                b.Unlock().unwrap();
            }
            Ok(())
        })
        .unwrap();
        let total: usize = got.iter().map(|g| g.1).sum();
        assert_eq!(total, 350, "{got:?}");
        assert_eq!(got.first().unwrap().2, 1);
        assert_eq!(got.last().unwrap().2, 2);
        assert!(got.iter().any(|g| g.2 == 0), "silence expected in the gap: {got:?}");
    }
}

/// All PCM (16-bit stereo, interleaved) of sound stream `stream` of a file.
pub fn decode_audio(p: &Path, stream: u32) -> Result<Vec<i16>, String> {
    startup()?;
    unsafe {
        let r = MFCreateSourceReaderFromURL(&HSTRING::from(p.as_os_str()), None).map_err(e("open"))?;
        let _ = r.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false);
        // Stream numbers count all streams; find the `stream`-th sound one.
        let mut idx = None;
        let mut k = 0;
        for i in 0..16u32 {
            let Ok(t) = r.GetNativeMediaType(i, 0) else { break };
            if t.GetGUID(&MF_MT_MAJOR_TYPE).ok() == Some(MFMediaType_Audio) {
                if k == stream {
                    idx = Some(i);
                    break;
                }
                k += 1;
            }
        }
        let idx = idx.ok_or("нет такой дорожки")?;
        r.SetStreamSelection(idx, true).map_err(e("select"))?;
        r.SetCurrentMediaType(idx, None, &pcm_type().map_err(e("t"))?).map_err(e("pcm"))?;
        let mut out = Vec::new();
        loop {
            let mut flags = 0u32;
            let mut s: Option<IMFSample> = None;
            r.ReadSample(idx, 0, None, Some(&mut flags), None, Some(&mut s)).map_err(e("read"))?;
            if let Some(s) = s {
                let b = sample_bytes(&s)?;
                out.extend(b.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])));
            }
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                break;
            }
        }
        Ok(out)
    }
}

/// Testing: encode a tone in one piece and in several, decode both and
/// report how far apart they are, at the joins and elsewhere.
pub fn aac_join_test(dir: &Path) -> Result<String, String> {
    startup()?;
    com_init();
    std::fs::create_dir_all(dir).map_err(|x| x.to_string())?;
    let rate = crate::audio::RATE as u64;
    let fps = 60i64;
    // Three 10-second buffer files of 10 ms blocks: 440 Hz plus a quieter 3 kHz.
    let mut segs = Vec::new();
    for i in 0..3u64 {
        let path = dir.join(format!("seg{i}.buf"));
        let mut f = std::io::BufWriter::new(std::fs::File::create(&path).map_err(|x| x.to_string())?);
        for b in 0..1000u64 {
            let pos = i * 10 * rate + b * rate / 100;
            let data: Vec<u8> = (0..rate / 100)
                .flat_map(|j| {
                    let t = (pos + j) as f64 / rate as f64;
                    let v = ((t * 440.0 * std::f64::consts::TAU).sin() * 8000.0 + (t * 3000.0 * std::f64::consts::TAU).sin() * 3000.0) as i16;
                    let b = v.to_le_bytes();
                    [b[0], b[1], b[0], b[1]]
                })
                .collect();
            crate::avi::write_record(&mut f, crate::avi::KIND_AUDIO, pos, &data).map_err(|x| x.to_string())?;
        }
        drop(f);
        segs.push(SegRef { mp4: PathBuf::new(), audio: path, base_slot: i * 600 });
    }
    let refs: Vec<&SegRef> = segs.iter().collect();
    let (a0, a_end) = (rate / 2, 29 * rate);
    let src = TrackSrc::Stored(0);
    let frames = (a_end - a0).div_ceil(AAC_FRAME);
    let mut decoded = Vec::new();
    let mut joins = Vec::new();
    for n in [1usize, 3] {
        let plan = pieces(frames, n);
        joins = plan.iter().skip(1).map(|p| p.from * AAC_FRAME).collect::<Vec<_>>();
        let out = dir.join(format!("pieces{n}.mp4"));
        let mut w = crate::mp4::Writer::create(&out).map_err(|x| x.to_string())?;
        let t = w.add_aac(crate::audio::RATE, 2);
        for p in plan {
            let (aac, _) = unsafe { Aac::new() }.map_err(e("AAC"))?;
            let (tx, rx) = crossbeam_channel::unbounded();
            let ps = piece_segs(&refs, fps, a0 + p.from.saturating_sub(AAC_ROLL) * AAC_FRAME, if p.last { a_end } else { a0 + (p.to + AAC_ROLL) * AAC_FRAME });
            encode_piece(&aac, &ps, &src, a0, a_end, p, &tx)?;
            drop(tx);
            for s in rx.iter() {
                let s = s?.0;
                let k = (unsafe { s.GetSampleTime() }.unwrap_or(0) as f64 / (AAC_FRAME as f64 * HNS as f64 / rate as f64)).round() as u64;
                w.write(t, &unsafe { sample_bytes(&s) }?, k * AAC_FRAME, true).map_err(|x| x.to_string())?;
            }
        }
        w.finish().map_err(|x| x.to_string())?;
        decoded.push(decode_audio(&out, 0)?);
    }
    let (a, b) = (&decoded[0], &decoded[1]);
    let n = a.len().min(b.len()) / 2;
    let diff = |i: usize| (a[i * 2] as i32 - b[i * 2] as i32).abs();
    let near = |i: usize| joins.iter().any(|&j| (i as i64 - j as i64).abs() < 4096);
    let max_join = (0..n).filter(|&i| near(i)).map(diff).max().unwrap_or(0);
    let max_else = (0..n).filter(|&i| !near(i)).map(diff).max().unwrap_or(0);
    let peak = a.iter().map(|v| v.unsigned_abs()).max().unwrap_or(0);
    Ok(format!(
        "samples {} / {} (want {}), joins at {:?}, peak {peak}, max diff near joins {max_join}, elsewhere {max_else}",
        a.len() / 2,
        b.len() / 2,
        a_end - a0,
        joins
    ))
}

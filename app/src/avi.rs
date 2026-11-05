//! Buffer segment records and the AVI (Motion-JPEG + PCM) writer that turns
//! a range of the buffer into one playable file. Saving is a plain copy, so
//! even 20 minutes are written in seconds.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub const KIND_VIDEO: u8 = 0;
pub const KIND_AUDIO: u8 = 1;
pub const REC_HEADER: usize = 13;

pub fn write_record(w: &mut impl Write, kind: u8, pos: u64, data: &[u8]) -> io::Result<()> {
    let mut h = [0u8; REC_HEADER];
    h[0] = kind;
    h[1..9].copy_from_slice(&pos.to_le_bytes());
    h[9..13].copy_from_slice(&(data.len() as u32).to_le_bytes());
    w.write_all(&h)?;
    w.write_all(data)
}

struct Reader {
    r: BufReader<File>,
}

impl Reader {
    fn open(p: &Path) -> io::Result<Self> {
        Ok(Self { r: BufReader::with_capacity(1 << 20, File::open(p)?) })
    }
    /// Next record header: (kind, pos, len). None at the end (or a torn tail).
    fn header(&mut self) -> Option<(u8, u64, usize)> {
        let mut h = [0u8; REC_HEADER];
        self.r.read_exact(&mut h).ok()?;
        let pos = u64::from_le_bytes(h[1..9].try_into().unwrap());
        let len = u32::from_le_bytes(h[9..13].try_into().unwrap()) as usize;
        Some((h[0], pos, len))
    }
    fn skip(&mut self, len: usize) -> bool {
        self.r.seek_relative(len as i64).is_ok()
    }
    fn data(&mut self, len: usize) -> Option<Vec<u8>> {
        let mut v = vec![0u8; len];
        self.r.read_exact(&mut v).ok()?;
        Some(v)
    }
}

/// Call `f(kind, pos, data)` for every record in a buffer file.
pub fn for_each_record(
    p: &Path,
    mut f: impl FnMut(u8, u64, &[u8]) -> Result<(), String>,
) -> Result<(), String> {
    let Ok(mut r) = Reader::open(p) else { return Ok(()) };
    while let Some((kind, pos, len)) = r.header() {
        let Some(d) = r.data(len) else { break };
        f(kind, pos, &d)?;
    }
    Ok(())
}

pub struct ClipSpec {
    pub from_ms: u64,
    pub to_ms: u64,
    pub fps: u32,
    pub width: u32,
    pub height: u32,
    /// Number of sound tracks in the clip (AVI uses only the first).
    pub audio_tracks: u8,
    /// Some(weights): the first clip track ("всё вместе") is not in the
    /// buffer but summed from the stored tracks with these weights; clip
    /// track k+1 is stored track k. None: clip tracks are stored as they are.
    pub mix_weights: Option<Vec<f32>>,
}

pub struct ClipResult {
    pub frames: u64,
    pub bytes: u64,
    pub truncated: bool,
}

const MAX_MOVI: u64 = 3_900_000_000;
const AUDIO_BLOCK: u32 = 4; // 16-bit stereo
const AUDIO_RATE: u64 = crate::audio::RATE as u64;

fn chunk(id: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(8 + data.len() + 1);
    v.extend_from_slice(id);
    v.extend_from_slice(&(data.len() as u32).to_le_bytes());
    v.extend_from_slice(data);
    if data.len() % 2 == 1 {
        v.push(0);
    }
    v
}

fn list(kind: &[u8; 4], parts: &[Vec<u8>]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(kind);
    for p in parts {
        body.extend_from_slice(p);
    }
    chunk(b"LIST", &body)
}

struct Le(Vec<u8>);
impl Le {
    fn u32(mut self, v: u32) -> Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn u16(mut self, v: u16) -> Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn cc(mut self, v: &[u8; 4]) -> Self {
        self.0.extend_from_slice(v);
        self
    }
}

struct Totals {
    frames: u32,
    audio_blocks: u32,
    max_chunk: u32,
    movi_size: u32,
    idx_len: u32,
}

fn header(spec: &ClipSpec, t: &Totals) -> Vec<u8> {
    let streams = if spec.audio_tracks > 0 { 2 } else { 1 };
    let avih = Le(Vec::new())
        .u32(1_000_000 / spec.fps)
        .u32(0)
        .u32(0)
        .u32(0x10 | 0x100)
        .u32(t.frames)
        .u32(0)
        .u32(streams)
        .u32(t.max_chunk)
        .u32(spec.width)
        .u32(spec.height)
        .u32(0)
        .u32(0)
        .u32(0)
        .u32(0)
        .0;
    let vstrh = Le(Vec::new())
        .cc(b"vids")
        .cc(b"MJPG")
        .u32(0)
        .u16(0)
        .u16(0)
        .u32(0)
        .u32(1)
        .u32(spec.fps)
        .u32(0)
        .u32(t.frames)
        .u32(t.max_chunk)
        .u32(u32::MAX)
        .u32(0)
        .u16(0)
        .u16(0)
        .u16(spec.width as u16)
        .u16(spec.height as u16)
        .0;
    let vstrf = Le(Vec::new())
        .u32(40)
        .u32(spec.width)
        .u32(spec.height)
        .u16(1)
        .u16(24)
        .cc(b"MJPG")
        .u32(spec.width * spec.height * 3)
        .u32(0)
        .u32(0)
        .u32(0)
        .u32(0)
        .0;
    let mut hdrl = vec![
        chunk(b"avih", &avih),
        list(b"strl", &[chunk(b"strh", &vstrh), chunk(b"strf", &vstrf)]),
    ];
    if spec.audio_tracks > 0 {
        let astrh = Le(Vec::new())
            .cc(b"auds")
            .u32(0)
            .u32(0)
            .u16(0)
            .u16(0)
            .u32(0)
            .u32(AUDIO_BLOCK)
            .u32(AUDIO_BLOCK * AUDIO_RATE as u32)
            .u32(0)
            .u32(t.audio_blocks)
            .u32(AUDIO_BLOCK * AUDIO_RATE as u32 / 10)
            .u32(u32::MAX)
            .u32(AUDIO_BLOCK)
            .u16(0)
            .u16(0)
            .u16(0)
            .u16(0)
            .0;
        let astrf = Le(Vec::new())
            .u16(1)
            .u16(2)
            .u32(AUDIO_RATE as u32)
            .u32(AUDIO_RATE as u32 * AUDIO_BLOCK)
            .u16(AUDIO_BLOCK as u16)
            .u16(16)
            .u16(0)
            .0;
        hdrl.push(list(b"strl", &[chunk(b"strh", &astrh), chunk(b"strf", &astrf)]));
    }
    let hdrl = list(b"hdrl", &hdrl);
    let riff_size = 4 + hdrl.len() as u32 + 8 + t.movi_size + t.idx_len;
    let mut out = Vec::new();
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&riff_size.to_le_bytes());
    out.extend_from_slice(b"AVI ");
    out.extend_from_slice(&hdrl);
    out.extend_from_slice(b"LIST");
    out.extend_from_slice(&t.movi_size.to_le_bytes());
    out.extend_from_slice(b"movi");
    out
}

struct MoviWriter {
    w: BufWriter<File>,
    offset: u64, // bytes written after the 'movi' fourcc, +4
    index: Vec<([u8; 4], u32, u32)>,
    max_chunk: u32,
    full: bool,
}

impl MoviWriter {
    fn put(&mut self, id: &[u8; 4], data: &[u8]) -> io::Result<()> {
        if self.full {
            return Ok(());
        }
        if self.offset + data.len() as u64 + 8 > MAX_MOVI {
            self.full = true;
            return Ok(());
        }
        self.w.write_all(id)?;
        self.w.write_all(&(data.len() as u32).to_le_bytes())?;
        self.w.write_all(data)?;
        if data.len() % 2 == 1 {
            self.w.write_all(&[0])?;
        }
        self.index.push((*id, self.offset as u32, data.len() as u32));
        self.offset += 8 + data.len() as u64 + (data.len() % 2) as u64;
        self.max_chunk = self.max_chunk.max(data.len() as u32);
        Ok(())
    }
}

/// Write the part of the buffer between `from_ms` and `to_ms` to `out`.
pub fn assemble(segments: &[PathBuf], spec: &ClipSpec, out: &Path) -> io::Result<ClipResult> {
    let fps = spec.fps as u64;
    let want_start = spec.from_ms * fps / 1000;
    let want_end = spec.to_ms * fps / 1000;

    // Pass 1: find where the picture actually starts and ends.
    let mut has_before = false;
    let mut first_in: Option<u64> = None;
    let mut last_slot: Option<u64> = None;
    for p in segments {
        let Ok(mut r) = Reader::open(p) else { continue };
        while let Some((kind, pos, len)) = r.header() {
            if kind == KIND_VIDEO {
                if pos <= want_start {
                    has_before = true;
                } else if pos <= want_end {
                    first_in = Some(first_in.map_or(pos, |f: u64| f.min(pos)));
                }
                if pos <= want_end {
                    last_slot = Some(last_slot.map_or(pos, |l: u64| l.max(pos)));
                }
            }
            if !r.skip(len) {
                break;
            }
        }
    }
    let start_slot = if has_before {
        want_start
    } else {
        match first_in {
            Some(f) => f,
            None => return Err(io::Error::new(io::ErrorKind::NotFound, "в буфере ещё нет кадров")),
        }
    };
    let end_slot = last_slot.unwrap_or(start_slot).max(start_slot);
    let a_start = start_slot * AUDIO_RATE / fps;
    let a_end = (end_slot + 1) * AUDIO_RATE / fps;

    // Pass 2: write.
    let file = File::create(out)?;
    let mut t = Totals { frames: 0, audio_blocks: 0, max_chunk: 0, movi_size: 4, idx_len: 0 };
    let hdr = header(spec, &t);
    let mut mw = MoviWriter {
        w: BufWriter::with_capacity(1 << 20, file),
        offset: 4,
        index: Vec::new(),
        max_chunk: 0,
        full: false,
    };
    mw.w.write_all(&hdr)?;

    let mut pending: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
    let mut last_frame: Option<(u64, Vec<u8>)> = None; // newest frame before start
    let mut prev: Option<Vec<u8>> = None;
    let mut next_slot = start_slot;
    let mut a_next = a_start;
    let mut frames: u64 = 0;

    let emit = |mw: &mut MoviWriter,
                    slot: u64,
                    data: Vec<u8>,
                    prev: &mut Option<Vec<u8>>,
                    next_slot: &mut u64,
                    frames: &mut u64|
     -> io::Result<()> {
        if slot < *next_slot || slot > end_slot {
            return Ok(());
        }
        while *next_slot < slot {
            let fill = prev.as_ref().unwrap_or(&data).clone();
            mw.put(b"00dc", &fill)?;
            *next_slot += 1;
            *frames += 1;
        }
        mw.put(b"00dc", &data)?;
        *prev = Some(data);
        *next_slot = slot + 1;
        *frames += 1;
        Ok(())
    };

    for p in segments {
        let Ok(mut r) = Reader::open(p) else { continue };
        while let Some((kind, pos, len)) = r.header() {
            match kind {
                KIND_VIDEO if pos < start_slot => {
                    let newer = last_frame.as_ref().map_or(true, |(s, _)| pos >= *s);
                    if newer {
                        let Some(d) = r.data(len) else { break };
                        last_frame = Some((pos, d));
                    } else if !r.skip(len) {
                        break;
                    }
                }
                KIND_VIDEO if pos <= end_slot => {
                    let Some(d) = r.data(len) else { break };
                    if prev.is_none() && pending.is_empty() {
                        if let Some((_, lf)) = last_frame.take() {
                            if pos > start_slot {
                                emit(&mut mw, start_slot, lf, &mut prev, &mut next_slot, &mut frames)?;
                            }
                        }
                    }
                    pending.insert(pos, d);
                    while pending.len() > 8 {
                        let (s, d) = pending.pop_first().unwrap();
                        emit(&mut mw, s, d, &mut prev, &mut next_slot, &mut frames)?;
                    }
                }
                KIND_AUDIO if spec.audio_tracks > 0 => {
                    let Some(d) = r.data(len) else { break };
                    let n = (d.len() / 4) as u64;
                    let (s, e) = (pos.max(a_next), (pos + n).min(a_end));
                    if e > s {
                        if s > a_next {
                            // Gap in the sound: fill with silence.
                            let mut gap = s - a_next;
                            while gap > 0 {
                                let k = gap.min(AUDIO_RATE / 10);
                                mw.put(b"01wb", &vec![0u8; (k * 4) as usize])?;
                                gap -= k;
                            }
                        }
                        let off = ((s - pos) * 4) as usize;
                        let end = ((e - pos) * 4) as usize;
                        mw.put(b"01wb", &d[off..end])?;
                        a_next = e;
                    }
                }
                _ => {
                    if !r.skip(len) {
                        break;
                    }
                }
            }
        }
    }
    if prev.is_none() && pending.is_empty() {
        if let Some((_, lf)) = last_frame.take() {
            emit(&mut mw, start_slot, lf, &mut prev, &mut next_slot, &mut frames)?;
        }
    }
    while let Some((s, d)) = pending.pop_first() {
        emit(&mut mw, s, d, &mut prev, &mut next_slot, &mut frames)?;
    }

    // Index.
    let audio_bytes: u64 = mw
        .index
        .iter()
        .filter(|(id, _, _)| id == b"01wb")
        .map(|(_, _, l)| *l as u64)
        .sum();
    let mut idx = Vec::with_capacity(mw.index.len() * 16);
    for (id, off, len) in &mw.index {
        idx.extend_from_slice(id);
        idx.extend_from_slice(&0x10u32.to_le_bytes());
        idx.extend_from_slice(&off.to_le_bytes());
        idx.extend_from_slice(&len.to_le_bytes());
    }
    mw.w.write_all(b"idx1")?;
    mw.w.write_all(&(idx.len() as u32).to_le_bytes())?;
    mw.w.write_all(&idx)?;

    t.frames = frames as u32;
    t.audio_blocks = (audio_bytes / 4) as u32;
    t.max_chunk = mw.max_chunk;
    t.movi_size = mw.offset as u32;
    t.idx_len = 8 + idx.len() as u32;
    let truncated = mw.full;
    let mut f = mw.w.into_inner().map_err(|e| e.into_error())?;
    f.seek(SeekFrom::Start(0))?;
    f.write_all(&header(spec, &t))?;
    let bytes = f.seek(SeekFrom::End(0))?;
    f.sync_all()?;
    Ok(ClipResult { frames, bytes, truncated })
}

/// Duration of an AVI written by this program (reads only the main header).
pub fn avi_duration(p: &Path) -> Option<f64> {
    let mut f = File::open(p).ok()?;
    let mut h = [0u8; 56];
    f.read_exact(&mut h).ok()?;
    if &h[0..4] != b"RIFF" || &h[24..28] != b"avih" {
        return None;
    }
    let us = u32::from_le_bytes(h[32..36].try_into().ok()?) as f64;
    let frames = u32::from_le_bytes(h[48..52].try_into().ok()?) as f64;
    Some(us * frames / 1e6)
}

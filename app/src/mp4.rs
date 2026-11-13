//! A small MP4 reader and writer for saving clips by copying.
//!
//! The buffer segments are MP4 files that Rewind wrote itself (one video
//! track, no B-frames). Saving a clip only has to copy their compressed
//! frames into one new file and add the sound, so this reads a whole segment
//! in one go and writes the clip directly: far faster than passing every
//! frame through Media Foundation. Anything unexpected is reported as an
//! error and the caller falls back to the Media Foundation way.

use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::Path;

/// One compressed video frame inside a segment file.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Frame {
    pub off: usize,
    pub size: u32,
    /// Time in the segment's timescale.
    pub time: u64,
    pub key: bool,
}

/// A buffer segment read into memory: its frames and how to describe them.
pub struct Segment {
    pub data: Vec<u8>,
    pub timescale: u32,
    /// The sample description box body (codec setup), copied into the clip.
    pub stsd: Vec<u8>,
    pub frames: Vec<Frame>,
}

fn be32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn be64(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_be_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

/// The child boxes of `b`: (type, body).
fn boxes(b: &[u8]) -> Vec<([u8; 4], &[u8])> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at + 8 <= b.len() {
        let Some(size) = be32(b, at) else { break };
        let typ: [u8; 4] = b[at + 4..at + 8].try_into().unwrap();
        let (size, hdr) = match size {
            1 => match be64(b, at + 8) {
                Some(s) => (s as usize, 16),
                None => break,
            },
            0 => (b.len() - at, 8),
            s => (s as usize, 8),
        };
        if size < hdr || at + size > b.len() {
            break;
        }
        out.push((typ, &b[at + hdr..at + size]));
        at += size;
    }
    out
}

fn child<'a>(b: &'a [u8], typ: &[u8; 4]) -> Option<&'a [u8]> {
    boxes(b).into_iter().find(|(t, _)| t == typ).map(|(_, d)| d)
}

/// Full-box tables start with version/flags and an entry count.
fn entries(b: &[u8]) -> Option<(u32, &[u8])> {
    Some((be32(b, 4)?, b.get(8..)?))
}

fn bad(what: &str) -> String {
    format!("MP4: {what}")
}

/// Parse a segment Rewind wrote: one video track, frames with their times.
pub fn parse(data: Vec<u8>) -> Result<Segment, String> {
    let moov = child(&data, b"moov").ok_or_else(|| bad("нет moov"))?;
    let trak = boxes(moov)
        .into_iter()
        .filter(|(t, _)| t == b"trak")
        .map(|(_, d)| d)
        .find(|t| {
            child(t, b"mdia").and_then(|m| child(m, b"hdlr")).is_some_and(|h| h.get(8..12) == Some(b"vide"))
        })
        .ok_or_else(|| bad("нет видео"))?;
    let mdia = child(trak, b"mdia").ok_or_else(|| bad("нет mdia"))?;
    let mdhd = child(mdia, b"mdhd").ok_or_else(|| bad("нет mdhd"))?;
    let timescale = match mdhd.first() {
        Some(0) => be32(mdhd, 12),
        Some(1) => be32(mdhd, 20),
        _ => None,
    }
    .filter(|&t| t > 0)
    .ok_or_else(|| bad("mdhd"))?;
    let stbl = child(mdia, b"minf").and_then(|m| child(m, b"stbl")).ok_or_else(|| bad("нет stbl"))?;
    let stsd = child(stbl, b"stsd").ok_or_else(|| bad("нет stsd"))?.to_vec();

    // Frames reordered (B-frames) would need their own times: not copied here.
    if let Some(ctts) = child(stbl, b"ctts") {
        let (n, t) = entries(ctts).ok_or_else(|| bad("ctts"))?;
        if (0..n as usize).any(|i| be32(t, i * 8 + 4).unwrap_or(1) != 0) {
            return Err(bad("переставленные кадры"));
        }
    }

    // Sizes.
    let stsz = child(stbl, b"stsz").ok_or_else(|| bad("нет stsz"))?;
    let fixed = be32(stsz, 4).ok_or_else(|| bad("stsz"))?;
    let count = be32(stsz, 8).ok_or_else(|| bad("stsz"))? as usize;
    let sizes: Vec<u32> = if fixed != 0 {
        vec![fixed; count]
    } else {
        (0..count).map(|i| be32(stsz, 12 + i * 4)).collect::<Option<_>>().ok_or_else(|| bad("stsz"))?
    };

    // Times.
    let (n, t) = entries(child(stbl, b"stts").ok_or_else(|| bad("нет stts"))?).ok_or_else(|| bad("stts"))?;
    let mut times = Vec::with_capacity(count);
    let mut now = 0u64;
    for i in 0..n as usize {
        let (k, d) = (be32(t, i * 8).ok_or_else(|| bad("stts"))?, be32(t, i * 8 + 4).ok_or_else(|| bad("stts"))?);
        for _ in 0..k {
            times.push(now);
            now += d as u64;
        }
    }

    // Where each frame is: chunk offsets plus frames per chunk.
    let offsets: Vec<u64> = if let Some(b) = child(stbl, b"stco") {
        let (n, t) = entries(b).ok_or_else(|| bad("stco"))?;
        (0..n as usize).map(|i| be32(t, i * 4).map(u64::from)).collect::<Option<_>>().ok_or_else(|| bad("stco"))?
    } else {
        let (n, t) = entries(child(stbl, b"co64").ok_or_else(|| bad("нет stco"))?).ok_or_else(|| bad("co64"))?;
        (0..n as usize).map(|i| be64(t, i * 8)).collect::<Option<_>>().ok_or_else(|| bad("co64"))?
    };
    let (n, t) = entries(child(stbl, b"stsc").ok_or_else(|| bad("нет stsc"))?).ok_or_else(|| bad("stsc"))?;
    let stsc: Vec<(u32, u32)> = (0..n as usize)
        .map(|i| Some((be32(t, i * 12)?, be32(t, i * 12 + 4)?)))
        .collect::<Option<_>>()
        .ok_or_else(|| bad("stsc"))?;
    let mut offs = Vec::with_capacity(count);
    for (c, &base) in offsets.iter().enumerate() {
        let chunk = c as u32 + 1;
        let per = stsc.iter().rev().find(|(first, _)| *first <= chunk).map_or(0, |e| e.1);
        let mut at = base;
        for _ in 0..per {
            let Some(&size) = sizes.get(offs.len()) else { break };
            offs.push(at);
            at += size as u64;
        }
    }

    // Key frames (none listed: every frame is one).
    let keys: Option<Vec<u32>> = match child(stbl, b"stss") {
        Some(b) => {
            let (n, t) = entries(b).ok_or_else(|| bad("stss"))?;
            Some((0..n as usize).map(|i| be32(t, i * 4)).collect::<Option<_>>().ok_or_else(|| bad("stss"))?)
        }
        None => None,
    };

    if offs.len() != count || times.len() < count {
        return Err(bad("таблицы кадров не сходятся"));
    }
    let mut frames = Vec::with_capacity(count);
    for i in 0..count {
        let (off, size) = (offs[i] as usize, sizes[i]);
        if off + size as usize > data.len() {
            return Err(bad("кадр за концом файла"));
        }
        let key = keys.as_ref().map_or(true, |k| k.binary_search(&(i as u32 + 1)).is_ok());
        frames.push(Frame { off, size, time: times[i], key });
    }
    Ok(Segment { data, timescale, stsd, frames })
}

pub fn read(path: &Path) -> Result<Segment, String> {
    parse(std::fs::read(path).map_err(|e| format!("Чтение буфера: {e}"))?)
}

// ---------------------------------------------------------------- writing

enum Kind {
    Video { width: u16, height: u16 },
    Audio,
}

struct Track {
    kind: Kind,
    timescale: u32,
    stsd: Vec<u8>,
    sizes: Vec<u32>,
    times: Vec<u64>,
    /// Duration of the last sample (nothing follows it to measure against).
    last_dur: u32,
    keys: Vec<u32>,
    /// Runs of samples stored one after another: (samples, file offset).
    chunks: Vec<(u32, u64)>,
}

impl Track {
    fn durations(&self) -> Vec<u32> {
        let mut d: Vec<u32> = self.times.windows(2).map(|w| w[1].saturating_sub(w[0]).clamp(1, u32::MAX as u64) as u32).collect();
        if !self.times.is_empty() {
            d.push(self.last_dur.max(1));
        }
        d
    }
}

/// Writes an MP4 file: the media data first, the index (moov) at the end.
pub struct Writer {
    f: BufWriter<File>,
    pos: u64,
    mdat_at: u64,
    tracks: Vec<Track>,
    last: Option<usize>,
}

/// AAC-LC AudioSpecificConfig for `rate` Hz and `channels` channels.
fn aac_config(rate: u32, channels: u8) -> [u8; 2] {
    const RATES: [u32; 13] = [96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350];
    let idx = RATES.iter().position(|&r| r == rate).unwrap_or(3) as u8;
    let object = 2u8; // AAC-LC
    [(object << 3) | (idx >> 1), ((idx & 1) << 7) | (channels << 3)]
}

fn put_box(out: &mut Vec<u8>, typ: &[u8; 4], body: &[u8]) {
    out.extend_from_slice(&(body.len() as u32 + 8).to_be_bytes());
    out.extend_from_slice(typ);
    out.extend_from_slice(body);
}

fn full(version: u8, flags: u32) -> Vec<u8> {
    let mut v = vec![version];
    v.extend_from_slice(&flags.to_be_bytes()[1..]);
    v
}

fn descriptor(tag: u8, body: &[u8]) -> Vec<u8> {
    // Four-byte length form: fine for any size.
    let n = body.len() as u32;
    let mut v = vec![tag, 0x80 | (n >> 21) as u8 & 0x7f, 0x80 | (n >> 14) as u8 & 0x7f, 0x80 | (n >> 7) as u8 & 0x7f, n as u8 & 0x7f];
    v.extend_from_slice(body);
    v
}

/// The sample description for an AAC track.
fn mp4a_stsd(rate: u32, channels: u8, bitrate: u32) -> Vec<u8> {
    let mut dec = vec![0x40, (0x05 << 2) | 1, 0, 0x30, 0]; // AAC, audio stream, 12 KB buffer
    dec.extend_from_slice(&bitrate.to_be_bytes());
    dec.extend_from_slice(&bitrate.to_be_bytes());
    dec.extend(descriptor(5, &aac_config(rate, channels)));
    let mut es = vec![0, 0, 0]; // ES_ID, flags
    es.extend(descriptor(4, &dec));
    es.extend(descriptor(6, &[2]));
    let mut esds = full(0, 0);
    esds.extend(descriptor(3, &es));

    let mut entry = vec![0u8; 6];
    entry.extend_from_slice(&1u16.to_be_bytes()); // data reference
    entry.extend_from_slice(&[0; 8]);
    entry.extend_from_slice(&(channels as u16).to_be_bytes());
    entry.extend_from_slice(&16u16.to_be_bytes());
    entry.extend_from_slice(&[0; 4]);
    entry.extend_from_slice(&(rate << 16).to_be_bytes());
    put_box(&mut entry, b"esds", &esds);

    let mut stsd = full(0, 0);
    stsd.extend_from_slice(&1u32.to_be_bytes());
    put_box(&mut stsd, b"mp4a", &entry);
    stsd
}

/// Picture size from a video sample description (VisualSampleEntry).
fn visual_size(stsd: &[u8]) -> (u16, u16) {
    // stsd: version/flags(4) count(4), entry: size(4) type(4) reserved(6)
    // data ref(2) predefined/reserved(16) width(2) height(2).
    let at = 8 + 8 + 6 + 2 + 16;
    let get = |i: usize| stsd.get(i..i + 2).map_or(0, |b| u16::from_be_bytes([b[0], b[1]]));
    (get(at), get(at + 2))
}

impl Writer {
    pub fn create(path: &Path) -> std::io::Result<Writer> {
        let mut f = BufWriter::with_capacity(4 << 20, File::create(path)?);
        let mut head = Vec::new();
        put_box(&mut head, b"ftyp", b"isom\0\0\x02\0isomiso2mp41");
        let mdat_at = head.len() as u64;
        // mdat with a 64-bit size, filled in at the end.
        head.extend_from_slice(&1u32.to_be_bytes());
        head.extend_from_slice(b"mdat");
        head.extend_from_slice(&0u64.to_be_bytes());
        f.write_all(&head)?;
        Ok(Writer { f, pos: head.len() as u64, mdat_at, tracks: Vec::new(), last: None })
    }

    /// Add the video track; `stsd` comes from a segment, `frame` is the
    /// duration of one frame (for the last one).
    pub fn add_video(&mut self, stsd: Vec<u8>, timescale: u32, frame: u32) -> usize {
        let (width, height) = visual_size(&stsd);
        self.add(Track { kind: Kind::Video { width, height }, timescale, stsd, sizes: vec![], times: vec![], last_dur: frame, keys: vec![], chunks: vec![] })
    }

    /// Add an AAC track (`rate` Hz, `channels`); its stsd is made at the end.
    pub fn add_aac(&mut self, rate: u32, channels: u8) -> usize {
        let stsd = vec![channels];
        self.add(Track { kind: Kind::Audio, timescale: rate, stsd, sizes: vec![], times: vec![], last_dur: 1024, keys: vec![], chunks: vec![] })
    }

    fn add(&mut self, t: Track) -> usize {
        self.tracks.push(t);
        self.tracks.len() - 1
    }

    /// Append one sample of `track` at `time` (in the track's timescale).
    pub fn write(&mut self, track: usize, data: &[u8], time: u64, key: bool) -> std::io::Result<()> {
        self.f.write_all(data)?;
        let t = &mut self.tracks[track];
        // Times only go forward.
        let time = match t.times.last() {
            Some(&p) if time <= p => p + 1,
            _ => time,
        };
        t.sizes.push(data.len() as u32);
        t.times.push(time);
        if key {
            t.keys.push(t.sizes.len() as u32);
        }
        match (self.last == Some(track), t.chunks.last_mut()) {
            (true, Some(c)) => c.0 += 1,
            _ => t.chunks.push((1, self.pos)),
        }
        self.last = Some(track);
        self.pos += data.len() as u64;
        Ok(())
    }

    /// Write the index and close the file. Returns the file size.
    pub fn finish(mut self) -> std::io::Result<u64> {
        let mdat_size = self.pos - self.mdat_at;
        let moov = self.moov();
        self.f.write_all(&moov)?;
        self.f.seek(SeekFrom::Start(self.mdat_at + 8))?;
        self.f.write_all(&mdat_size.to_be_bytes())?;
        self.f.flush()?;
        Ok(self.pos + moov.len() as u64)
    }

    fn moov(&self) -> Vec<u8> {
        const MOVIE_SCALE: u64 = 1000;
        let matrix: [u32; 9] = [0x10000, 0, 0, 0, 0x10000, 0, 0, 0, 0x4000_0000];
        let durs: Vec<Vec<u32>> = self.tracks.iter().map(Track::durations).collect();
        let lens: Vec<u64> = durs.iter().map(|d| d.iter().map(|&x| x as u64).sum()).collect();
        let movie_len = |i: usize| lens[i] * MOVIE_SCALE / self.tracks[i].timescale as u64;
        let longest = (0..self.tracks.len()).map(movie_len).max().unwrap_or(0);

        let mut moov = Vec::new();
        let mut mvhd = full(1, 0);
        mvhd.extend_from_slice(&[0; 16]); // created, modified
        mvhd.extend_from_slice(&(MOVIE_SCALE as u32).to_be_bytes());
        mvhd.extend_from_slice(&longest.to_be_bytes());
        mvhd.extend_from_slice(&0x10000u32.to_be_bytes()); // rate 1.0
        mvhd.extend_from_slice(&0x0100u16.to_be_bytes()); // volume 1.0
        mvhd.extend_from_slice(&[0; 10]);
        matrix.iter().for_each(|m| mvhd.extend_from_slice(&m.to_be_bytes()));
        mvhd.extend_from_slice(&[0; 24]);
        mvhd.extend_from_slice(&(self.tracks.len() as u32 + 1).to_be_bytes());
        put_box(&mut moov, b"mvhd", &mvhd);

        for (i, t) in self.tracks.iter().enumerate() {
            let id = i as u32 + 1;
            let video = matches!(t.kind, Kind::Video { .. });
            let mut trak = Vec::new();

            let mut tkhd = full(1, 3); // enabled, in movie
            tkhd.extend_from_slice(&[0; 16]);
            tkhd.extend_from_slice(&id.to_be_bytes());
            tkhd.extend_from_slice(&[0; 4]);
            tkhd.extend_from_slice(&movie_len(i).to_be_bytes());
            tkhd.extend_from_slice(&[0; 8]);
            tkhd.extend_from_slice(&0u16.to_be_bytes()); // layer
            tkhd.extend_from_slice(&(if video { 0u16 } else { 1 }).to_be_bytes()); // alternate group
            tkhd.extend_from_slice(&(if video { 0u16 } else { 0x0100 }).to_be_bytes());
            tkhd.extend_from_slice(&[0; 2]);
            matrix.iter().for_each(|m| tkhd.extend_from_slice(&m.to_be_bytes()));
            let (w, h) = match t.kind {
                Kind::Video { width, height } => (width as u32, height as u32),
                Kind::Audio => (0, 0),
            };
            tkhd.extend_from_slice(&(w << 16).to_be_bytes());
            tkhd.extend_from_slice(&(h << 16).to_be_bytes());
            put_box(&mut trak, b"tkhd", &tkhd);

            let mut mdia = Vec::new();
            let mut mdhd = full(1, 0);
            mdhd.extend_from_slice(&[0; 16]);
            mdhd.extend_from_slice(&t.timescale.to_be_bytes());
            mdhd.extend_from_slice(&lens[i].to_be_bytes());
            mdhd.extend_from_slice(&0x55c4u16.to_be_bytes()); // "und"
            mdhd.extend_from_slice(&[0; 2]);
            put_box(&mut mdia, b"mdhd", &mdhd);

            let mut hdlr = full(0, 0);
            hdlr.extend_from_slice(&[0; 4]);
            hdlr.extend_from_slice(if video { b"vide" } else { b"soun" });
            hdlr.extend_from_slice(&[0; 12]);
            hdlr.extend_from_slice(if video { b"VideoHandler\0" } else { b"SoundHandler\0" });
            put_box(&mut mdia, b"hdlr", &hdlr);

            let mut minf = Vec::new();
            if video {
                let mut vmhd = full(0, 1);
                vmhd.extend_from_slice(&[0; 8]);
                put_box(&mut minf, b"vmhd", &vmhd);
            } else {
                let mut smhd = full(0, 0);
                smhd.extend_from_slice(&[0; 4]);
                put_box(&mut minf, b"smhd", &smhd);
            }
            let mut dref = full(0, 0);
            dref.extend_from_slice(&1u32.to_be_bytes());
            put_box(&mut dref, b"url ", &full(0, 1));
            let mut dinf = Vec::new();
            put_box(&mut dinf, b"dref", &dref);
            put_box(&mut minf, b"dinf", &dinf);

            let mut stbl = Vec::new();
            let stsd = match t.kind {
                Kind::Video { .. } => t.stsd.clone(),
                Kind::Audio => {
                    let secs = (lens[i] / t.timescale.max(1) as u64).max(1);
                    let bytes: u64 = t.sizes.iter().map(|&s| s as u64).sum();
                    mp4a_stsd(t.timescale, t.stsd[0], (bytes * 8 / secs) as u32)
                }
            };
            put_box(&mut stbl, b"stsd", &stsd);

            let mut runs: Vec<(u32, u32)> = Vec::new();
            for &d in &durs[i] {
                match runs.last_mut() {
                    Some(r) if r.1 == d => r.0 += 1,
                    _ => runs.push((1, d)),
                }
            }
            let mut stts = full(0, 0);
            stts.extend_from_slice(&(runs.len() as u32).to_be_bytes());
            runs.iter().for_each(|(n, d)| {
                stts.extend_from_slice(&n.to_be_bytes());
                stts.extend_from_slice(&d.to_be_bytes());
            });
            put_box(&mut stbl, b"stts", &stts);

            if video && t.keys.len() != t.sizes.len() {
                let mut stss = full(0, 0);
                stss.extend_from_slice(&(t.keys.len() as u32).to_be_bytes());
                t.keys.iter().for_each(|k| stss.extend_from_slice(&k.to_be_bytes()));
                put_box(&mut stbl, b"stss", &stss);
            }

            let mut stsc_rows: Vec<(u32, u32)> = Vec::new();
            for (c, &(n, _)) in t.chunks.iter().enumerate() {
                if stsc_rows.last().map(|r| r.1) != Some(n) {
                    stsc_rows.push((c as u32 + 1, n));
                }
            }
            let mut stsc = full(0, 0);
            stsc.extend_from_slice(&(stsc_rows.len() as u32).to_be_bytes());
            stsc_rows.iter().for_each(|(first, n)| {
                stsc.extend_from_slice(&first.to_be_bytes());
                stsc.extend_from_slice(&n.to_be_bytes());
                stsc.extend_from_slice(&1u32.to_be_bytes());
            });
            put_box(&mut stbl, b"stsc", &stsc);

            let mut stsz = full(0, 0);
            stsz.extend_from_slice(&0u32.to_be_bytes());
            stsz.extend_from_slice(&(t.sizes.len() as u32).to_be_bytes());
            t.sizes.iter().for_each(|s| stsz.extend_from_slice(&s.to_be_bytes()));
            put_box(&mut stbl, b"stsz", &stsz);

            let mut co = full(0, 0);
            co.extend_from_slice(&(t.chunks.len() as u32).to_be_bytes());
            if t.chunks.iter().any(|c| c.1 > u32::MAX as u64) {
                t.chunks.iter().for_each(|c| co.extend_from_slice(&c.1.to_be_bytes()));
                put_box(&mut stbl, b"co64", &co);
            } else {
                t.chunks.iter().for_each(|c| co.extend_from_slice(&(c.1 as u32).to_be_bytes()));
                put_box(&mut stbl, b"stco", &co);
            }

            put_box(&mut minf, b"stbl", &stbl);
            put_box(&mut mdia, b"minf", &minf);
            put_box(&mut trak, b"mdia", &mdia);
            put_box(&mut moov, b"trak", &trak);
        }
        let mut out = Vec::new();
        put_box(&mut out, b"moov", &moov);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aac_config_for_48k_stereo() {
        assert_eq!(aac_config(48000, 2), [0x11, 0x90]);
        assert_eq!(aac_config(44100, 2), [0x12, 0x10]);
    }

    /// What the writer makes, the reader reads back: frames, times, keys.
    #[test]
    fn written_file_reads_back() {
        let dir = std::env::temp_dir().join(format!("rewind-mp4-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.mp4");
        // A minimal visual sample entry: 1280x720.
        let mut entry = vec![0u8; 6 + 2 + 16];
        entry.extend_from_slice(&1280u16.to_be_bytes());
        entry.extend_from_slice(&720u16.to_be_bytes());
        entry.extend_from_slice(&[0; 50]);
        let mut stsd = full(0, 0);
        stsd.extend_from_slice(&1u32.to_be_bytes());
        put_box(&mut stsd, b"hvc1", &entry);

        let mut w = Writer::create(&path).unwrap();
        let v = w.add_video(stsd.clone(), 60000, 1000);
        let a = w.add_aac(48000, 2);
        let frames: Vec<(Vec<u8>, u64, bool)> = (0..5u8).map(|i| (vec![i; 10 + i as usize], i as u64 * 1000 + if i > 2 { 500 } else { 0 }, i % 3 == 0)).collect();
        for (k, (d, t, key)) in frames.iter().enumerate() {
            w.write(v, d, *t, *key).unwrap();
            w.write(a, &[0xAA; 7], k as u64 * 1024, true).unwrap();
            w.write(a, &[0xBB; 6], k as u64 * 1024 + 512, true).unwrap();
        }
        w.finish().unwrap();

        let seg = read(&path).unwrap();
        assert_eq!(seg.timescale, 60000);
        assert_eq!(seg.stsd, stsd);
        assert_eq!(visual_size(&seg.stsd), (1280, 720));
        assert_eq!(seg.frames.len(), 5);
        for (f, (d, t, key)) in seg.frames.iter().zip(&frames) {
            assert_eq!(&seg.data[f.off..f.off + f.size as usize], &d[..]);
            assert_eq!((f.time, f.key), (*t, *key));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

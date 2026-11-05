//! Short sounds: a chime when a clip is saved, a low tone when it failed.
//! Heard even over games in exclusive fullscreen, where the corner notice
//! can't be seen. Made in code (no sound files).
//!
//! The chime must not end up in the recording: the game track already leaves
//! out this program's own sound, except when it leaves out a voice-chat app
//! instead; for that case `muting_game` tells the audio thread to drop the
//! game sound for the moment the chime plays.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use windows::core::PCWSTR;
use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_MEMORY, SND_NODEFAULT};

const RATE: u32 = 44_100;
/// How long the game track stays silent after a chime starts.
const MUTE_FOR: Duration = Duration::from_millis(700);

fn start() -> Instant {
    static T: OnceLock<Instant> = OnceLock::new();
    *T.get_or_init(Instant::now)
}

/// When the last chime started (ms since start(), 0 = never).
static PLAYED_AT: AtomicU64 = AtomicU64::new(0);

/// True while a chime may be playing.
pub fn muting_game() -> bool {
    let at = PLAYED_AT.load(Ordering::Relaxed);
    at != 0 && start().elapsed().as_millis() as u64 <= at + MUTE_FOR.as_millis() as u64
}

/// Two soft rising notes.
pub fn saved() {
    static WAV: OnceLock<Vec<u8>> = OnceLock::new();
    play(WAV.get_or_init(|| wav(&[(880.0, 0.09), (1318.5, 0.16)])));
}

/// One low note.
pub fn failed() {
    static WAV: OnceLock<Vec<u8>> = OnceLock::new();
    play(WAV.get_or_init(|| wav(&[(330.0, 0.22)])));
}

fn play(wav: &'static [u8]) {
    PLAYED_AT.store(start().elapsed().as_millis() as u64 + 1, Ordering::Relaxed);
    unsafe {
        // SND_MEMORY: the "name" is a pointer to the whole .wav in memory.
        let _ = PlaySoundW(PCWSTR(wav.as_ptr() as *const u16), None, SND_MEMORY | SND_ASYNC | SND_NODEFAULT);
    }
}

/// A mono 16-bit .wav of sine notes (frequency Hz, seconds), each with a
/// quick fade in and a longer fade out so they don't click.
fn wav(notes: &[(f32, f32)]) -> Vec<u8> {
    let mut pcm: Vec<i16> = Vec::new();
    for &(f, len) in notes {
        let n = (len * RATE as f32) as usize;
        for i in 0..n {
            let t = i as f32 / RATE as f32;
            let attack = (t / 0.008).min(1.0);
            let release = ((len - t) / (len * 0.7)).clamp(0.0, 1.0);
            let env = attack * release * release;
            // A touch of the octave makes it sound like a bell, not a beep.
            let s = (std::f32::consts::TAU * f * t).sin() * 0.8 + (std::f32::consts::TAU * f * 2.0 * t).sin() * 0.2;
            pcm.push((s * env * 0.35 * i16::MAX as f32) as i16);
        }
    }
    let data_len = (pcm.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&RATE.to_le_bytes());
    out.extend_from_slice(&(RATE * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in pcm {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn wav_header_matches_data() {
        let w = super::wav(&[(440.0, 0.1)]);
        assert_eq!(&w[..4], b"RIFF");
        assert_eq!(&w[8..12], b"WAVE");
        let data_len = u32::from_le_bytes(w[40..44].try_into().unwrap()) as usize;
        assert_eq!(w.len(), 44 + data_len);
        assert_eq!(data_len, 4410 * 2);
    }
}

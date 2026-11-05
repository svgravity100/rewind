//! Sound capture into separate tracks:
//!   • game / computer sound — everything except the voice-chat app,
//!   • voice chat (Discord, TeamSpeak, Telegram…) — only that app's sound,
//!   • microphone.
//! Per-app capture uses Windows "process loopback" (Windows 10 2004+ / 11).
//! All sources are mixed on a wall clock (48 kHz stereo 16-bit) so silence
//! and device hiccups never shift sync. When there is more than one track,
//! track 1 is a mix of everything so the clip sounds complete in any player.

use crate::recorder::{Shared, WriterMsg};
use crossbeam_channel::Sender;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const RATE: u32 = 48_000;
const LATENCY_S: f64 = 0.10;

/// Known voice-chat programs: (name shown to the person, process names).
pub const CHAT_APPS: &[(&str, &[&str])] = &[
    ("Discord", &["discord.exe", "discordptb.exe", "discordcanary.exe"]),
    ("TeamSpeak", &["ts3client_win64.exe", "ts3client_win32.exe", "teamspeak.exe"]),
    ("Telegram", &["telegram.exe"]),
    ("Skype", &["skype.exe"]),
    ("Zoom", &["zoom.exe"]),
    ("Teams", &["ms-teams.exe", "teams.exe"]),
    ("Mumble", &["mumble.exe"]),
    ("WhatsApp", &["whatsapp.exe", "whatsapp.root.exe"]),
    ("Viber", &["viber.exe"]),
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Track {
    Mix,
    Game,
    Chat,
    Mic,
}

/// Which tracks a clip gets, in file order.
pub fn tracks(game: bool, chat: bool, mic: bool) -> Vec<Track> {
    let parts: Vec<Track> = [(game, Track::Game), (chat, Track::Chat), (mic, Track::Mic)]
        .into_iter()
        .filter(|(on, _)| *on)
        .map(|(_, t)| t)
        .collect();
    if parts.len() > 1 {
        std::iter::once(Track::Mix).chain(parts).collect()
    } else {
        parts
    }
}

type Queue = Arc<Mutex<VecDeque<f32>>>;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Target {
    /// Everything except this process tree.
    Exclude(u32),
    /// Only this process tree.
    Include(u32),
    /// Classic loopback of the default output (fallback).
    Render,
    Mic,
}

struct Source {
    q: Queue,
    stop: Arc<AtomicBool>,
    failed: Arc<Mutex<Option<String>>>,
    target: Target,
}

impl Drop for Source {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn open_client(target: Target) -> Result<wasapi::AudioClient, wasapi::WasapiError> {
    use wasapi::*;
    let mut client = match target {
        Target::Exclude(pid) => AudioClient::new_application_loopback_client(pid, false)?,
        Target::Include(pid) => AudioClient::new_application_loopback_client(pid, true)?,
        Target::Render => DeviceEnumerator::new()?.get_default_device(&Direction::Render)?.get_iaudioclient()?,
        Target::Mic => DeviceEnumerator::new()?.get_default_device(&Direction::Capture)?.get_iaudioclient()?,
    };
    let fmt = WaveFormat::new(32, 32, &SampleType::Float, RATE as usize, 2, None);
    let mode = StreamMode::EventsShared { autoconvert: true, buffer_duration_hns: 0 };
    client.initialize_client(&fmt, &Direction::Capture, &mode)?;
    Ok(client)
}

fn spawn_source(target: Target) -> Source {
    let q: Queue = Arc::default();
    let stop = Arc::new(AtomicBool::new(false));
    let failed: Arc<Mutex<Option<String>>> = Arc::default();
    let (q2, s2, f2) = (q.clone(), stop.clone(), failed.clone());
    std::thread::spawn(move || {
        let _ = wasapi::initialize_mta();
        let run = || -> Result<(), wasapi::WasapiError> {
            let client = open_client(target)?;
            let event = client.set_get_eventhandle()?;
            let capture = client.get_audiocaptureclient()?;
            client.start_stream()?;
            let mut raw: VecDeque<u8> = VecDeque::new();
            while !s2.load(Ordering::Relaxed) {
                let _ = event.wait_for_event(100);
                loop {
                    match capture.get_next_packet_size()? {
                        Some(n) if n > 0 => {
                            capture.read_from_device_to_deque(&mut raw)?;
                        }
                        _ => break,
                    }
                }
                if raw.len() >= 4 {
                    let n = raw.len() / 4 * 4;
                    let bytes: Vec<u8> = raw.drain(..n).collect();
                    let mut g = q2.lock().unwrap();
                    for c in bytes.chunks_exact(4) {
                        g.push_back(f32::from_le_bytes([c[0], c[1], c[2], c[3]]));
                    }
                }
            }
            let _ = client.stop_stream();
            Ok(())
        };
        if let Err(e) = run() {
            *f2.lock().unwrap() = Some(e.to_string());
        }
    });
    Source { q, stop, failed, target }
}

/// Root processes of running chat apps (in CHAT_APPS order), with their names.
pub fn find_chat_roots(enabled: &[String]) -> Vec<(u32, &'static str)> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::*;
    let mut procs: Vec<(u32, u32, String)> = Vec::new();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return Vec::new() };
        let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        if Process32FirstW(snap, &mut e).is_ok() {
            loop {
                let len = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
                let name = String::from_utf16_lossy(&e.szExeFile[..len]).to_lowercase();
                procs.push((e.th32ProcessID, e.th32ParentProcessID, name));
                if Process32NextW(snap, &mut e).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
    let by_pid: HashMap<u32, &str> = procs.iter().map(|(p, _, n)| (*p, n.as_str())).collect();
    let mut out = Vec::new();
    for (app, exes) in CHAT_APPS {
        if !enabled.iter().any(|a| a == app) {
            continue;
        }
        for (pid, ppid, name) in &procs {
            if exes.contains(&name.as_str()) && by_pid.get(ppid).map_or(true, |pn| pn != name) {
                out.push((*pid, *app));
            }
        }
    }
    out
}

pub struct AudioConfig {
    pub game: bool,
    pub chat: bool,
    pub mic: bool,
    pub chat_apps: Vec<String>,
    /// AVI fallback: one mixed track only.
    pub mix_only: bool,
}

fn take(q: &Queue, n: usize, out: &mut [f32], peak: &mut f32) {
    let mut g = q.lock().unwrap();
    let k = (n * 2).min(g.len());
    for (i, s) in g.drain(..k).enumerate() {
        out[i] += s;
        *peak = peak.max(s.abs());
    }
    // Keep the queue short so latency can't creep up.
    if g.len() > (RATE as usize / 4) * 2 {
        let extra = g.len() - (RATE as usize / 20) * 2;
        g.drain(..extra);
    }
}

fn to_i16(samples: &[f32]) -> Vec<u8> {
    let mut b = Vec::with_capacity(samples.len() * 2);
    for s in samples {
        b.extend_from_slice(&((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
    }
    b
}

pub fn run(cfg: AudioConfig, stop: Arc<AtomicBool>, tx: Sender<WriterMsg>, t0: Instant, shared: Arc<Shared>) {
    let order = if cfg.mix_only { vec![Track::Mix] } else { tracks(cfg.game, cfg.chat, cfg.mic) };
    let own_pid = std::process::id();
    let mut game: Option<Source> = None;
    let mut game_fallback = false;
    let mut chats: HashMap<u32, (Source, &'static str)> = HashMap::new();
    let mut mic: Option<Source> = cfg.mic.then(|| spawn_source(Target::Mic));
    let mut last_scan = Instant::now() - Duration::from_secs(60);
    let mut emitted: Option<u64> = None;

    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(20));

        // Every 2 s: look for chat apps and (re)open sources as needed.
        if last_scan.elapsed() >= Duration::from_secs(2) {
            last_scan = Instant::now();
            let roots = if cfg.chat { find_chat_roots(&cfg.chat_apps) } else { Vec::new() };
            if cfg.chat {
                chats.retain(|pid, (s, _)| roots.iter().any(|r| r.0 == *pid) && s.failed.lock().unwrap().is_none());
                for (pid, name) in &roots {
                    chats.entry(*pid).or_insert_with(|| (spawn_source(Target::Include(*pid)), *name));
                }
                let mut names: Vec<&str> = chats.values().map(|c| c.1).collect();
                names.sort();
                names.dedup();
                *shared.chat_found.lock().unwrap() = names.iter().map(|s| s.to_string()).collect();
            }
            if cfg.game {
                let want = if game_fallback {
                    Target::Render
                } else {
                    Target::Exclude(roots.first().map_or(own_pid, |r| r.0))
                };
                let broken = game.as_ref().is_some_and(|g| g.failed.lock().unwrap().is_some());
                if broken && !game_fallback {
                    // Per-app capture not available: record the whole output.
                    game_fallback = true;
                    game = Some(spawn_source(Target::Render));
                } else if game.as_ref().map_or(true, |g| g.target != want) || broken {
                    game = Some(spawn_source(want));
                }
            }
            if mic.as_ref().is_some_and(|m| m.failed.lock().unwrap().is_some()) {
                mic = Some(spawn_source(Target::Mic));
            }
            let mut notes = Vec::new();
            if game_fallback {
                notes.push("Запись звука отдельных программ недоступна — дорожка игры включает и чат".to_string());
            }
            if let Some(e) = mic.as_ref().and_then(|m| m.failed.lock().unwrap().clone()) {
                notes.push(format!("Микрофон: {e}"));
            }
            *shared.audio_note.lock().unwrap() = if notes.is_empty() { None } else { Some(notes.join("; ")) };
        }

        let target = ((t0.elapsed().as_secs_f64() - LATENCY_S).max(0.0) * RATE as f64) as u64;
        let start = *emitted.get_or_insert(target);
        if target <= start {
            continue;
        }
        let n = (target - start) as usize;
        let mut g = vec![0f32; n * 2];
        let mut c = vec![0f32; n * 2];
        let mut m = vec![0f32; n * 2];
        let mut peaks = [0f32; 3];
        if let Some(s) = &game {
            take(&s.q, n, &mut g, &mut peaks[0]);
        }
        // Our save chime is in the game sound when the game track leaves out a
        // chat app instead of this program: drop the game sound while it plays.
        if crate::sound::muting_game() && (game_fallback || !chats.is_empty()) {
            g.iter_mut().for_each(|v| *v = 0.0);
        }
        for (s, _) in chats.values() {
            take(&s.q, n, &mut c, &mut peaks[1]);
        }
        if let Some(s) = &mic {
            take(&s.q, n, &mut m, &mut peaks[2]);
        }
        shared.level_sys.store(peaks[0].to_bits(), Ordering::Relaxed);
        shared.level_chat.store(peaks[1].to_bits(), Ordering::Relaxed);
        shared.level_mic.store(peaks[2].to_bits(), Ordering::Relaxed);
        for (i, t) in order.iter().enumerate() {
            let data = match t {
                Track::Game => to_i16(&g),
                Track::Chat => to_i16(&c),
                Track::Mic => to_i16(&m),
                Track::Mix => {
                    // In fallback the game track already contains the chat.
                    let cw = if game_fallback { 0.0 } else { 1.0 };
                    let mix: Vec<f32> = (0..n * 2).map(|k| g[k] + c[k] * cw + m[k]).collect();
                    to_i16(&mix)
                }
            };
            if tx.send(WriterMsg::Audio { track: i as u8, pos: start, data }).is_err() {
                return;
            }
        }
        emitted = Some(target);
    }
}

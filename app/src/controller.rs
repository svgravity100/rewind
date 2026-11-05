//! Shared control surface used by both the window and the global hotkeys
//! and the tray (both work while the console window is hidden).

use crate::capture::{self, MonitorInfo};
use crate::recorder::{AppEvent, Pipeline, PipelineConfig};
use crate::settings::{Hotkey, Mods, Settings};
use crate::util;
use crossbeam_channel::Sender;
use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Action {
    SaveReplay,
    ToggleReplay,
    ToggleCrosshair,
}

/// Set from the tray or the console close handler; the UI loop exits on it.
pub static QUIT: AtomicBool = AtomicBool::new(false);

pub struct Controller {
    pub settings: Mutex<Settings>,
    pub pipeline: Mutex<Option<Pipeline>>,
    pub monitors: Vec<MonitorInfo>,
    pub events: Sender<AppEvent>,
    session: AtomicU64,
    pub restart_note: Mutex<Option<String>>,
}

impl Controller {
    pub fn new(settings: Settings, events: Sender<AppEvent>) -> Self {
        // Leftovers from a crash (default place and the chosen one).
        let _ = std::fs::remove_dir_all(util::buffer_root(&None));
        let _ = std::fs::remove_dir_all(util::buffer_root(&settings.buffer_dir));
        Self {
            settings: Mutex::new(settings),
            pipeline: Mutex::new(None),
            monitors: capture::list_monitors(),
            events,
            session: AtomicU64::new(0),
            restart_note: Mutex::new(None),
        }
    }

    fn config(&self, s: &Settings) -> Option<PipelineConfig> {
        let m = self.monitors.get(s.monitor).or(self.monitors.first())?.clone();
        let (out_w, out_h) = s.resolution.output_size(m.width, m.height);
        let n = self.session.fetch_add(1, Ordering::Relaxed);
        Some(PipelineConfig {
            monitor: m,
            fps: s.fps,
            out_w,
            out_h,
            quality: s.quality.jpeg(),
            bitrate: crate::mf::bitrate(out_w, out_h, s.fps, s.quality, s.codec),
            codec: s.codec,
            format: s.format,
            game_audio: s.system_audio,
            chat_track: s.chat_track,
            chat_apps: s.chat_apps.clone(),
            mic: s.microphone,
            temp_dir: util::buffer_root(&s.buffer_dir).join(format!("{}-{n}", std::process::id())),
            key: s.capture_key(),
        })
    }

    /// Start capturing without touching the saved settings.
    pub fn start_now(&self) {
        let mut p = self.pipeline.lock().unwrap();
        self.start_locked(&mut p);
    }

    fn start_locked(&self, p: &mut Option<Pipeline>) {
        if p.is_some() {
            return;
        }
        let s = self.settings.lock().unwrap().clone();
        match self.config(&s) {
            Some(cfg) => {
                let pl = Pipeline::start(cfg, s.buffer_minutes as u64 * 60, self.events.clone());
                // The pointer is always part of the recording.
                pl.shared.show_cursor.store(true, Ordering::Relaxed);
                *p = Some(pl);
            }
            None => {
                let _ = self.events.send(AppEvent::Error("Не найден ни один экран для записи".into()));
            }
        }
    }

    pub fn set_replay(&self, on: bool) {
        {
            let mut s = self.settings.lock().unwrap();
            s.replay_enabled = on;
            s.save();
        }
        let mut p = self.pipeline.lock().unwrap();
        if on {
            self.start_locked(&mut p);
        } else if let Some(x) = p.take() {
            x.stop();
        }
    }

    pub fn save_replay(&self, secs: Option<u64>) {
        let s = self.settings.lock().unwrap().clone();
        let p = self.pipeline.lock().unwrap();
        let Some(p) = p.as_ref() else {
            crate::notify::post("Буфер выключен", "включите его, чтобы сохранять клипы", false);
            if s.sound_on_save {
                crate::sound::failed();
            }
            let _ = self.events.send(AppEvent::Error("Буфер выключен — включите его, чтобы сохранять моменты".into()));
            return;
        };
        let secs = secs.unwrap_or(s.buffer_minutes as u64 * 60);
        // Right away, at the key press: the notice and the sound. The notice
        // changes to "Клип сохранён" when the file is ready.
        let len = if secs % 60 == 0 { format!("{} мин", secs / 60) } else { format!("{secs} с") };
        crate::notify::post("Сохраняю клип…", format!("последние {len}"), true);
        if s.sound_on_save {
            crate::sound::saved();
        }
        let out = util::unique_path(&s.output_dir, &format!("Rewind {}", util::stamp()), p.format.ext());
        p.save_last(secs, out, "Клип".into());
    }

    /// Apply edited settings: update the live buffer length, and restart the
    /// capture when something it depends on changed.
    pub fn apply(&self, new: Settings) {
        {
            let mut s = self.settings.lock().unwrap();
            if *s == new {
                return;
            }
            *s = new.clone();
            s.save();
        }
        let mut p = self.pipeline.lock().unwrap();
        if let Some(pl) = p.as_ref() {
            pl.shared.buffer_secs.store(new.buffer_minutes as u64 * 60, Ordering::Relaxed);
            pl.shared.show_cursor.store(true, Ordering::Relaxed);
            if pl.key != new.capture_key() {
                if let Some(x) = p.take() {
                    x.stop();
                }
                self.start_locked(&mut p);
                *self.restart_note.lock().unwrap() = Some("Настройки применены — буфер начат заново".into());
            }
        }
    }

    pub fn run(&self, a: Action) {
        match a {
            Action::SaveReplay => self.save_replay(None),
            Action::ToggleReplay => {
                let on = self.settings.lock().unwrap().replay_enabled;
                self.set_replay(!on);
            }
            Action::ToggleCrosshair => {
                let mut s = self.settings.lock().unwrap();
                s.crosshair_enabled = !s.crosshair_enabled;
                s.save();
            }
        }
    }

    pub fn shutdown(&self) {
        if let Some(p) = self.pipeline.lock().unwrap().take() {
            p.stop();
        }
    }
}

pub fn to_hotkey(h: Hotkey) -> HotKey {
    let mods = match h.mods {
        Mods::Alt => Modifiers::ALT,
        Mods::CtrlAlt => Modifiers::CONTROL | Modifiers::ALT,
        Mods::CtrlShift => Modifiers::CONTROL | Modifiers::SHIFT,
        Mods::AltShift => Modifiers::ALT | Modifiers::SHIFT,
    };
    let code = match h.key {
        1 => Code::F1,
        2 => Code::F2,
        3 => Code::F3,
        4 => Code::F4,
        5 => Code::F5,
        6 => Code::F6,
        7 => Code::F7,
        8 => Code::F8,
        9 => Code::F9,
        10 => Code::F10,
        11 => Code::F11,
        _ => Code::F12,
    };
    HotKey::new(Some(mods), code)
}

/// Global hotkeys. Must be created on a thread that pumps window messages.
pub struct Hotkeys {
    manager: Option<GlobalHotKeyManager>,
    registered: Vec<HotKey>,
    map: Arc<Mutex<HashMap<u32, Action>>>,
    pub failed: HashMap<Action, String>,
    current: Vec<(Action, Hotkey)>,
}

impl Hotkeys {
    pub fn new(ctrl: Arc<Controller>) -> Self {
        let map: Arc<Mutex<HashMap<u32, Action>>> = Arc::default();
        let manager = GlobalHotKeyManager::new().ok();
        if manager.is_some() {
            let m = map.clone();
            std::thread::spawn(move || {
                while let Ok(ev) = GlobalHotKeyEvent::receiver().recv() {
                    if ev.state != HotKeyState::Pressed {
                        continue;
                    }
                    let a = m.lock().unwrap().get(&ev.id).copied();
                    if let Some(a) = a {
                        ctrl.run(a);
                    }
                }
            });
        }
        Self { manager, registered: Vec::new(), map, failed: HashMap::new(), current: Vec::new() }
    }

    pub fn sync(&mut self, s: &Settings) {
        let want = vec![
            (Action::SaveReplay, s.hk_save),
            (Action::ToggleReplay, s.hk_toggle),
            (Action::ToggleCrosshair, s.hk_crosshair),
        ];
        if want == self.current {
            return;
        }
        self.current = want.clone();
        let Some(m) = &self.manager else {
            for (a, _) in &want {
                self.failed.insert(*a, "горячие клавиши недоступны".into());
            }
            return;
        };
        for hk in self.registered.drain(..) {
            let _ = m.unregister(hk);
        }
        self.failed.clear();
        let mut map = self.map.lock().unwrap();
        map.clear();
        for (a, h) in want {
            let hk = to_hotkey(h);
            if map.contains_key(&hk.id()) {
                self.failed.insert(a, "уже назначено другому действию".into());
                continue;
            }
            match m.register(hk) {
                Ok(()) => {
                    self.registered.push(hk);
                    map.insert(hk.id(), a);
                }
                Err(_) => {
                    self.failed.insert(a, "занято другой программой".into());
                }
            }
        }
    }
}

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum Resolution {
    Native,
    P1080,
    P720,
    P480,
}

impl Resolution {
    pub const ALL: [Resolution; 4] = [Self::Native, Self::P1080, Self::P720, Self::P480];
    pub fn label(self) -> &'static str {
        match self {
            Self::Native => "Как у экрана",
            Self::P1080 => "1080p",
            Self::P720 => "720p",
            Self::P480 => "480p",
        }
    }
    fn max_h(self) -> Option<u32> {
        match self {
            Self::Native => None,
            Self::P1080 => Some(1080),
            Self::P720 => Some(720),
            Self::P480 => Some(480),
        }
    }
    /// Output size for a screen of `w`×`h` (never upscales, always even).
    pub fn output_size(self, w: u32, h: u32) -> (u32, u32) {
        let (mut ow, mut oh) = (w, h);
        if let Some(mh) = self.max_h() {
            if h > mh {
                oh = mh;
                ow = (w as u64 * mh as u64 / h as u64) as u32;
            }
        }
        (ow.max(2) & !1, oh.max(2) & !1)
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum Quality {
    Low,
    Medium,
    High,
}

impl Quality {
    pub const ALL: [Quality; 3] = [Self::Low, Self::Medium, Self::High];
    pub fn label(self) -> &'static str {
        match self {
            Self::Low => "Низкое (меньше места)",
            Self::Medium => "Среднее",
            Self::High => "Высокое",
        }
    }
    pub fn jpeg(self) -> u8 {
        match self {
            Self::Low => 45,
            Self::Medium => 65,
            Self::High => 85,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum Format {
    Mp4,
    Avi,
}

impl Format {
    pub fn ext(self) -> &'static str {
        match self {
            Self::Mp4 => "mp4",
            Self::Avi => "avi",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Mods {
    Alt,
    CtrlAlt,
    CtrlShift,
    AltShift,
}

impl Mods {
    pub const ALL: [Mods; 4] = [Self::Alt, Self::CtrlAlt, Self::CtrlShift, Self::AltShift];
    pub fn label(self) -> &'static str {
        match self {
            Self::Alt => "Alt",
            Self::CtrlAlt => "Ctrl+Alt",
            Self::CtrlShift => "Ctrl+Shift",
            Self::AltShift => "Alt+Shift",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub struct Hotkey {
    pub mods: Mods,
    /// Function key number, 1..=12.
    pub key: u8,
}

impl Hotkey {
    pub fn label(self) -> String {
        format!("{}+F{}", self.mods.label(), self.key)
    }
}

/// Shape of the aiming crosshair drawn over the screen.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum CrosshairKind {
    Cross,
    Dot,
    Circle,
    T,
    CrossDot,
}

impl CrosshairKind {
    pub const ALL: [CrosshairKind; 5] = [Self::Cross, Self::Dot, Self::Circle, Self::T, Self::CrossDot];
    pub fn label(self) -> &'static str {
        match self {
            Self::Cross => "Перекрестие",
            Self::Dot => "Точка",
            Self::Circle => "Кружок",
            Self::T => "«Т» (как в шутерах)",
            Self::CrossDot => "Перекрестие с точкой",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct Settings {
    pub replay_enabled: bool,
    pub buffer_minutes: u32,
    pub fps: u32,
    pub resolution: Resolution,
    pub quality: Quality,
    /// Always MP4 now; AVI is only used automatically when MP4 can't work.
    pub format: Format,
    pub monitor: usize,
    /// Game / computer sound (without the voice-chat apps when `chat_track` is on).
    pub system_audio: bool,
    /// Voice-chat apps on their own track.
    pub chat_track: bool,
    /// Which chat apps count (names from `audio::CHAT_APPS`).
    pub chat_apps: Vec<String>,
    pub microphone: bool,
    /// The mouse pointer is always recorded now (no setting).
    /// Aiming crosshair, an overlay over the screen (excluded from the recording).
    pub crosshair_enabled: bool,
    pub crosshair_kind: CrosshairKind,
    /// Arm length in screen pixels, 4..=200.
    pub crosshair_size: u32,
    /// Line thickness in screen pixels, 1..=10.
    pub crosshair_thickness: u32,
    /// RGB, 0..=255 each (kept as bytes so it can be saved as JSON).
    pub crosshair_color: [u8; 3],
    pub output_dir: PathBuf,
    pub hk_save: Hotkey,
    pub hk_toggle: Hotkey,
    pub hk_crosshair: Hotkey,
    /// Missing in old files → 0, which triggers the migration in `load`.
    #[serde(default)]
    pub version: u32,
}

pub const FPS_MIN: u32 = 1;
pub const FPS_MAX: u32 = 240;
/// Buffer length range, minutes (the buffer lives on disk: ~19 GB per hour at "high").
pub const MINUTES_MIN: u32 = 1;
pub const MINUTES_MAX: u32 = 60;

impl Default for Settings {
    fn default() -> Self {
        Self {
            replay_enabled: true,
            buffer_minutes: 20,
            fps: 60,
            resolution: Resolution::P1080,
            quality: Quality::Medium,
            format: Format::Mp4,
            monitor: 0,
            system_audio: true,
            chat_track: true,
            chat_apps: crate::audio::CHAT_APPS.iter().map(|a| a.0.to_string()).collect(),
            microphone: false,
            crosshair_enabled: false,
            crosshair_kind: CrosshairKind::Cross,
            crosshair_size: 40,
            crosshair_thickness: 2,
            crosshair_color: [0x54, 0xE8, 0x7A],
            output_dir: crate::util::default_output_dir(),
            hk_save: Hotkey { mods: Mods::CtrlAlt, key: 10 },
            hk_toggle: Hotkey { mods: Mods::CtrlShift, key: 10 },
            hk_crosshair: Hotkey { mods: Mods::CtrlShift, key: 9 },
            version: 2,
        }
    }
}

impl Settings {
    pub fn load() -> Self {
        let p = crate::util::settings_path();
        std::fs::read_to_string(p)
            .ok()
            .and_then(|s| serde_json::from_str::<Settings>(&s).ok())
            .map(|mut s| {
                s.buffer_minutes = s.buffer_minutes.clamp(MINUTES_MIN, MINUTES_MAX);
                if s.version < 2 {
                    // Old defaults (Alt+F-keys) clash with the NVIDIA overlay.
                    let d = Settings::default();
                    s.hk_save = d.hk_save;
                    s.hk_toggle = d.hk_toggle;
                    s.version = 2;
                }
                // Old files had only 30/60/120; any typed value 1..=240 is fine now.
                s.fps = s.fps.clamp(FPS_MIN, FPS_MAX);
                s.crosshair_size = s.crosshair_size.clamp(4, 200);
                s.crosshair_thickness = s.crosshair_thickness.clamp(1, 10);
                s.format = Format::Mp4;
                s.move_old_clip_folder();
                s
            })
            .unwrap_or_default()
    }

    /// The program used to be called «Повтор» and kept clips in Videos\Повтор.
    /// If that default folder is still in use, rename it to Videos\Rewind
    /// (same disk, so the clips are not copied) and point the setting there.
    fn move_old_clip_folder(&mut self) {
        let old = crate::util::old_output_dir();
        if self.output_dir != old {
            return;
        }
        let new = crate::util::default_output_dir();
        let moved = if !old.exists() {
            true
        } else if !new.exists() {
            std::fs::rename(&old, &new).is_ok()
        } else {
            false // both exist: leave the clips where they are
        };
        if moved {
            self.output_dir = new;
            self.save();
        }
    }

    pub fn save(&self) {
        let p = crate::util::settings_path();
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        if let Ok(s) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(p, s);
        }
    }

    /// Everything that requires restarting the capture when changed.
    pub fn capture_key(&self) -> String {
        format!(
            "{}|{:?}|{:?}|{:?}|{}|{}|{}|{:?}|{}",
            self.fps,
            self.resolution,
            self.quality,
            self.format,
            self.monitor,
            self.system_audio,
            self.chat_track,
            self.chat_apps,
            self.microphone
        )
    }
}

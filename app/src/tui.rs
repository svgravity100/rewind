//! Terminal UI. Three pages: main (status + clips), settings, crosshair.
//! Keyboard only; the buffer, hotkeys, tray and overlay run on other threads.

use crate::capture::MonitorInfo;
use crate::controller::{Action, Controller, QUIT};
use crate::recorder::AppEvent;
use crate::settings::{CrosshairKind, Hotkey, Mods, Quality, Resolution, Settings, FPS_MAX, FPS_MIN, MINUTES_MAX, MINUTES_MIN};
use crate::shell::Failed;
use crate::{audio, avi, mf, tray, util};
use crossbeam_channel::Receiver;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

// Green theme: near-black green background, light green text, green frames.
const ACCENT: Color = Color::Rgb(0x76, 0xb9, 0x00);
const BG: Color = Color::Rgb(0x07, 0x0c, 0x04);
const TEXT: Color = Color::Rgb(0xc8, 0xe6, 0xa8);
const LINE: Color = Color::Rgb(0x3f, 0x66, 0x00);
const MUTED: Color = Color::Rgb(0x7e, 0x9c, 0x66);
const OK: Color = Color::Rgb(0x76, 0xb9, 0x00);
const REC: Color = Color::Rgb(0xff, 0x4d, 0x5e);
const WARN: Color = Color::Rgb(0xff, 0xb4, 0x3d);
const SEL_BG: Color = Color::Rgb(0x2b, 0x42, 0x00);

/// Whether the console takes 24-bit colours (set at start-up).
pub static TRUECOLOR: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

const fn rgb3(c: Color) -> (u8, u8, u8) {
    match c {
        Color::Rgb(r, g, b) => (r, g, b),
        _ => (0, 0, 0),
    }
}

/// Area inside a one-cell border.
fn inner(r: Rect) -> Rect {
    Rect::new(r.x + 1, r.y + 1, r.width.saturating_sub(2), r.height.saturating_sub(2))
}

/// Console colour-table slots repainted to the theme in our own console
/// (slot = Windows console index; the comment is the ratatui colour using it).
pub const PALETTE: [(usize, (u8, u8, u8)); 14] = [
    (0, rgb3(BG)),      // Black
    (2, rgb3(LINE)),    // Green
    (7, rgb3(MUTED)),   // Gray
    (8, rgb3(SEL_BG)),  // DarkGray
    (10, rgb3(ACCENT)), // LightGreen
    (12, rgb3(REC)),    // LightRed
    (14, rgb3(WARN)),   // LightYellow
    (15, rgb3(TEXT)),   // White
    // The crosshair colours, so the colour swatch shows the real colour.
    (3, rgb(COLORS[0].1)),  // Cyan: green
    (4, rgb(COLORS[1].1)),  // Red: red
    (11, rgb(COLORS[2].1)), // LightCyan: light blue
    (6, rgb(COLORS[3].1)),  // Yellow: yellow
    (9, rgb(COLORS[4].1)),  // LightBlue: white
    (5, rgb(COLORS[5].1)),  // Magenta: pink
];

const fn rgb(c: [u8; 3]) -> (u8, u8, u8) {
    (c[0], c[1], c[2])
}

/// Nearest standard console colour (Windows "Campbell" palette).
fn to_16(c: Color) -> Color {
    // Crosshair colours go to the slots repainted for them in `PALETTE`.
    const SLOTS: [Color; 6] = [Color::Cyan, Color::Red, Color::LightCyan, Color::Yellow, Color::LightBlue, Color::Magenta];
    if let Color::Rgb(r, g, b) = c {
        if let Some(i) = COLORS.iter().position(|x| x.1 == [r, g, b]) {
            return SLOTS[i];
        }
    }
    // Theme colours go to the slots `PALETTE` repaints in our own console.
    match c {
        ACCENT => return Color::LightGreen,
        TEXT => return Color::White,
        LINE => return Color::Green,
        MUTED => return Color::Gray,
        SEL_BG => return Color::DarkGray,
        BG => return Color::Black,
        REC => return Color::LightRed,
        WARN => return Color::LightYellow,
        _ => {}
    }
    let Color::Rgb(r, g, b) = c else { return c };
    const PAL: [(Color, (i32, i32, i32)); 16] = [
        (Color::Black, (12, 12, 12)),
        (Color::Red, (197, 15, 31)),
        (Color::Green, (19, 161, 14)),
        (Color::Yellow, (193, 156, 0)),
        (Color::Blue, (0, 55, 218)),
        (Color::Magenta, (136, 23, 152)),
        (Color::Cyan, (58, 150, 221)),
        (Color::Gray, (204, 204, 204)),
        (Color::DarkGray, (118, 118, 118)),
        (Color::LightRed, (231, 72, 86)),
        (Color::LightGreen, (22, 198, 12)),
        (Color::LightYellow, (249, 241, 165)),
        (Color::LightBlue, (59, 120, 255)),
        (Color::LightMagenta, (180, 0, 158)),
        (Color::LightCyan, (97, 214, 214)),
        (Color::White, (242, 242, 242)),
    ];
    let (r, g, b) = (r as i32, g as i32, b as i32);
    PAL.iter().min_by_key(|(_, (pr, pg, pb))| (r - pr).pow(2) + (g - pg).pow(2) + (b - pb).pow(2)).map_or(c, |p| p.0)
}

const COLORS: [(&str, [u8; 3]); 6] = [
    ("Зелёный", [0x54, 0xE8, 0x7A]),
    ("Красный", [0xFF, 0x3B, 0x3B]),
    ("Голубой", [0x3B, 0xD5, 0xFF]),
    ("Жёлтый", [0xFF, 0xE0, 0x3B]),
    ("Белый", [0xFF, 0xFF, 0xFF]),
    ("Розовый", [0xFF, 0x4D, 0xD2]),
];

#[derive(Clone, Copy, PartialEq)]
enum Page {
    Main,
    Settings,
    Voice,
    Crosshair,
}

/// One editable row on the settings / crosshair pages.
#[derive(Clone, Copy, PartialEq)]
enum F {
    Replay,
    Length,
    Monitor,
    Fps,
    Resolution,
    Quality,
    Game,
    Chat,
    ChatApp(usize),
    Mic,
    Folder,
    HkSave,
    HkToggle,
    XEnabled,
    XKind,
    XSize,
    XThick,
    XColor,
    HkCross,
    Autostart,
    CheckUpdates,
    Update,
    SaveSound,
    /// A group heading inside a list (not selectable).
    Group(&'static str),
}

struct Clip {
    path: PathBuf,
    name: String,
    bytes: u64,
    secs: Option<f64>,
}

struct Toast {
    text: String,
    color: Color,
    at: Instant,
}

pub struct Tui {
    ctrl: Arc<Controller>,
    events: Receiver<AppEvent>,
    failed: Failed,
    console: isize,
    page: Page,
    sel: [usize; 4],
    clips: Vec<Clip>,
    clips_dir: PathBuf,
    clip_state: ListState,
    toast: Option<Toast>,
    /// Digits typed on the FPS or buffer-length row, applied with Enter.
    num_input: Option<(F, String)>,
    /// Clickable places from the last frame (tabs, status lines).
    hits: Vec<(Rect, MHit)>,
    /// The list on screen: inner area, first visible row, and for setting
    /// rows the plain text of each row's value (to find ‹ › under the mouse).
    list_area: Rect,
    list_offset: usize,
    row_values: Vec<String>,
    /// For double-click on a clip.
    last_click: Option<(Instant, usize)>,
}

/// Mouse targets outside the lists.
#[derive(Clone, Copy)]
enum MHit {
    Tab(Page),
    ToggleBuffer,
    ToggleCross,
}

/// Width of the selection marker and of the label column in setting rows.
const MARK_W: u16 = 2;
const LABEL_W: u16 = 24;

impl Tui {
    pub fn new(ctrl: Arc<Controller>, events: Receiver<AppEvent>, failed: Failed, console: isize) -> Self {
        let mut t = Self {
            ctrl,
            events,
            failed,
            console,
            page: Page::Main,
            sel: [0; 4],
            clips: Vec::new(),
            clips_dir: PathBuf::new(),
            clip_state: ListState::default(),
            toast: None,
            num_input: None,
            hits: Vec::new(),
            list_area: Rect::default(),
            list_offset: 0,
            row_values: Vec::new(),
            last_click: None,
        };
        t.scan_clips();
        t
    }

    pub fn run(mut self) -> std::io::Result<()> {
        // The theme is the interface (selection, tabs, states), so colours stay
        // on even when NO_COLOR is set in the environment.
        crossterm::style::force_color_output(true);
        let mut term = ratatui::init();
        let _ = crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture);
        let res = (|| -> std::io::Result<()> {
            while !QUIT.load(Ordering::Relaxed) {
                self.handle_app_events();
                term.draw(|f| self.draw(f))?;
                if event::poll(Duration::from_millis(250))? {
                    match event::read()? {
                        Event::Key(k) if k.kind == KeyEventKind::Press => self.key(k),
                        Event::Mouse(m) => self.mouse(m),
                        // Repaint everything; diffs alone can leave stale cells.
                        Event::Resize(..) => term.clear()?,
                        _ => {}
                    }
                }
            }
            Ok(())
        })();
        let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture);
        ratatui::restore();
        res
    }

    /// Draw every page into a text buffer (for checking the layout without a console).
    pub fn snapshot(mut self, w: u16, h: u16) -> String {
        use ratatui::backend::TestBackend;
        let mut out = String::new();
        for page in [Page::Main, Page::Settings, Page::Voice, Page::Crosshair] {
            self.page = page;
            let mut term = ratatui::Terminal::new(TestBackend::new(w, h)).unwrap();
            term.draw(|f| self.draw(f)).unwrap();
            let buf = term.backend().buffer().clone();
            for y in 0..h {
                let mut line = String::new();
                for x in 0..w {
                    line.push_str(buf[(x, y)].symbol());
                }
                out.push_str(line.trim_end());
                out.push('\n');
            }
            out.push_str(&"=".repeat(w as usize));
            out.push('\n');
        }
        out
    }

    fn settings(&self) -> Settings {
        self.ctrl.settings.lock().unwrap().clone()
    }

    fn running(&self) -> bool {
        self.ctrl.pipeline.lock().unwrap().is_some()
    }

    fn say(&mut self, text: impl Into<String>, color: Color) {
        self.toast = Some(Toast { text: text.into(), color, at: Instant::now() });
    }

    fn handle_app_events(&mut self) {
        while let Ok(ev) = self.events.try_recv() {
            match ev {
                AppEvent::Saved { path, secs, bytes, what, truncated } => {
                    let mut text = if what == "Запись" { "Запись сохранена".to_string() } else { format!("{what} сохранён") };
                    if what != "Снимок" {
                        text.push_str(&format!(" · {}", util::fmt_clock((secs * 1000.0) as u64)));
                    }
                    text.push_str(&format!(" · {}", util::fmt_bytes(bytes)));
                    if truncated {
                        text.push_str(" (обрезан: файл больше 4 ГБ)");
                    }
                    // Notice in the corner of the screen, seen over the game.
                    let title = if what == "Запись" { "Запись сохранена".to_string() } else { format!("{what} сохранён") };
                    let mut detail = Vec::new();
                    if what != "Снимок" {
                        detail.push(util::fmt_clock((secs * 1000.0) as u64));
                    }
                    detail.push(util::fmt_bytes(bytes));
                    if let Some(dir) = path.parent().and_then(|d| d.file_name()) {
                        detail.push(dir.to_string_lossy().into_owned());
                    }
                    crate::notify::post(title, detail.join(" · "), true);
                    if self.settings().sound_on_save {
                        crate::sound::saved();
                    }
                    self.say(text, OK);
                    self.scan_clips();
                }
                AppEvent::Error(e) => {
                    if e.starts_with("Не удалось сохранить") {
                        crate::notify::post("Не удалось сохранить", e.trim_start_matches("Не удалось сохранить").trim_start_matches(':').trim(), false);
                        if self.settings().sound_on_save {
                            crate::sound::failed();
                        }
                    }
                    self.say(e, REC)
                }
            }
        }
        let note = self.ctrl.restart_note.lock().unwrap().take();
        if let Some(n) = note {
            self.say(n, ACCENT);
        }
        if self.toast.as_ref().is_some_and(|t| t.at.elapsed() > Duration::from_secs(8)) {
            self.toast = None;
        }
        let dir = self.settings().output_dir;
        if dir != self.clips_dir {
            self.scan_clips();
        }
    }

    fn scan_clips(&mut self) {
        let dir = self.settings().output_dir;
        let mut v: Vec<(std::time::SystemTime, Clip)> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                let p = e.path();
                let ext = p.extension().and_then(|x| x.to_str()).unwrap_or("").to_ascii_lowercase();
                if ext != "avi" && ext != "mp4" && ext != "png" {
                    continue;
                }
                let Ok(md) = e.metadata() else { continue };
                v.push((
                    md.modified().unwrap_or(std::time::UNIX_EPOCH),
                    Clip {
                        name: p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
                        bytes: md.len(),
                        secs: match ext.as_str() {
                            "avi" => avi::avi_duration(&p),
                            "mp4" => mf::mp4_duration(&p),
                            _ => None,
                        },
                        path: p,
                    },
                ));
            }
        }
        v.sort_by(|a, b| b.0.cmp(&a.0));
        self.clips = v.into_iter().take(60).map(|x| x.1).collect();
        self.clips_dir = dir;
        let n = self.clips.len();
        match self.clip_state.selected() {
            _ if n == 0 => self.clip_state.select(None),
            Some(i) if i >= n => self.clip_state.select(Some(n - 1)),
            None => self.clip_state.select(Some(0)),
            _ => {}
        }
    }

    // ------------------------------------------------------------------ keys

    fn key(&mut self, k: KeyEvent) {
        let shift = k.modifiers.contains(KeyModifiers::SHIFT);
        if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
            QUIT.store(true, Ordering::SeqCst);
            return;
        }
        // Typing a number on the FPS or buffer-length row.
        if let Some((field, buf)) = self.num_input.as_mut() {
            match k.code {
                KeyCode::Char(c) if c.is_ascii_digit() && buf.len() < 3 => buf.push(c),
                KeyCode::Backspace => {
                    buf.pop();
                }
                KeyCode::Enter => {
                    let (field, typed) = (*field, buf.parse::<u32>());
                    if let Ok(v) = typed {
                        let mut s = self.settings();
                        if field == F::Length {
                            s.buffer_minutes = v.clamp(MINUTES_MIN, MINUTES_MAX);
                        } else {
                            s.fps = v.clamp(FPS_MIN, FPS_MAX);
                        }
                        self.ctrl.apply(s);
                    }
                    self.num_input = None;
                }
                KeyCode::Esc => self.num_input = None,
                _ => {}
            }
            return;
        }
        match k.code {
            KeyCode::Char('1') => self.page = Page::Main,
            KeyCode::Char('2') => self.page = Page::Settings,
            KeyCode::Char('3') => self.page = Page::Voice,
            KeyCode::Char('4') => self.page = Page::Crosshair,
            KeyCode::Tab => {
                self.page = match self.page {
                    Page::Main => Page::Settings,
                    Page::Settings => Page::Voice,
                    Page::Voice => Page::Crosshair,
                    Page::Crosshair => Page::Main,
                }
            }
            KeyCode::Char('q') | KeyCode::Char('й') => QUIT.store(true, Ordering::SeqCst),
            KeyCode::Char('u') | KeyCode::Char('г') => {
                if matches!(crate::update::state(), crate::update::State::Available(_)) {
                    crate::update::install();
                }
            }
            KeyCode::Esc => {
                if self.console != 0 {
                    tray::hide(self.console);
                }
            }
            _ => match self.page {
                Page::Main => self.key_main(k),
                _ => self.key_fields(k, shift),
            },
        }
    }

    fn key_main(&mut self, k: KeyEvent) {
        let n = self.clips.len();
        if n == 0 {
            return;
        }
        let i = self.clip_state.selected().unwrap_or(0);
        match k.code {
            KeyCode::Up => self.clip_state.select(Some(i.saturating_sub(1))),
            KeyCode::Down => self.clip_state.select(Some((i + 1).min(n - 1))),
            KeyCode::Home => self.clip_state.select(Some(0)),
            KeyCode::End => self.clip_state.select(Some(n - 1)),
            KeyCode::Enter => util::open_path(&self.clips[i].path),
            KeyCode::Char('f') | KeyCode::Char('а') => util::reveal(&self.clips[i].path),
            KeyCode::Char('o') | KeyCode::Char('щ') => {
                let dir = self.settings().output_dir;
                let _ = std::fs::create_dir_all(&dir);
                util::open_path(&dir);
            }
            _ => {}
        }
    }

    fn page_idx(&self) -> usize {
        self.page as usize
    }

    fn key_fields(&mut self, k: KeyEvent, shift: bool) {
        let fields = fields(self.page);
        let pi = self.page_idx();
        let i = self.sel[pi].min(fields.len().saturating_sub(1));
        let f = fields[i];
        match k.code {
            KeyCode::Up => self.sel[pi] = step(&fields, i, -1),
            KeyCode::Down => self.sel[pi] = step(&fields, i, 1),
            KeyCode::Left | KeyCode::Right => {
                let dir = if k.code == KeyCode::Left { -1 } else { 1 };
                self.change(f, dir, shift);
            }
            KeyCode::Enter | KeyCode::Char(' ') => self.activate(f),
            KeyCode::Char(c) if c.is_ascii_digit() && matches!(f, F::Fps | F::Length) => self.num_input = Some((f, c.to_string())),
            _ => {}
        }
    }

    /// ←/→ on a row.
    fn change(&mut self, f: F, dir: i32, big: bool) {
        let mut s = self.settings();
        let step = if big { 10 } else { 1 };
        match f {
            F::Length => {
                let step = if big { 5 } else { 1 };
                s.buffer_minutes = (s.buffer_minutes as i32 + dir * step).clamp(MINUTES_MIN as i32, MINUTES_MAX as i32) as u32;
            }
            F::Monitor => {
                let n = self.ctrl.monitors.len().max(1);
                s.monitor = cycle(s.monitor.min(n - 1), n, dir);
            }
            F::Fps => s.fps = (s.fps as i32 + dir * step).clamp(FPS_MIN as i32, FPS_MAX as i32) as u32,
            F::Resolution => {
                let i = Resolution::ALL.iter().position(|&r| r == s.resolution).unwrap_or(0);
                s.resolution = Resolution::ALL[cycle(i, Resolution::ALL.len(), dir)];
            }
            F::Quality => {
                let i = Quality::ALL.iter().position(|&q| q == s.quality).unwrap_or(0);
                s.quality = Quality::ALL[cycle(i, Quality::ALL.len(), dir)];
            }
            F::XKind => {
                let i = CrosshairKind::ALL.iter().position(|&q| q == s.crosshair_kind).unwrap_or(0);
                s.crosshair_kind = CrosshairKind::ALL[cycle(i, CrosshairKind::ALL.len(), dir)];
            }
            F::XSize => s.crosshair_size = (s.crosshair_size as i32 + dir * step).clamp(4, 200) as u32,
            F::XThick => s.crosshair_thickness = (s.crosshair_thickness as i32 + dir).clamp(1, 10) as u32,
            F::XColor => {
                let i = COLORS.iter().position(|c| c.1 == s.crosshair_color);
                let next = match i {
                    Some(i) => cycle(i, COLORS.len(), dir),
                    None => 0,
                };
                s.crosshair_color = COLORS[next].1;
            }
            F::HkSave => s.hk_save.key = fkey(s.hk_save.key, dir),
            F::HkToggle => s.hk_toggle.key = fkey(s.hk_toggle.key, dir),
            F::HkCross => s.hk_crosshair.key = fkey(s.hk_crosshair.key, dir),
            // Switches flip with arrows too.
            _ => return self.activate(f),
        }
        self.ctrl.apply(s);
    }

    /// Enter / Space on a row.
    fn activate(&mut self, f: F) {
        let mut s = self.settings();
        match f {
            F::Replay => {
                self.ctrl.run(Action::ToggleReplay);
                return;
            }
            F::Game => s.system_audio = !s.system_audio,
            F::Chat => s.chat_track = !s.chat_track,
            F::ChatApp(i) => {
                let name = audio::CHAT_APPS[i].0;
                if s.chat_apps.iter().any(|a| a == name) {
                    s.chat_apps.retain(|a| a != name);
                } else {
                    s.chat_apps.push(name.to_string());
                }
            }
            F::Mic => s.microphone = !s.microphone,
            F::CheckUpdates => s.check_updates = !s.check_updates,
            F::SaveSound => s.sound_on_save = !s.sound_on_save,
            F::Update => {
                match crate::update::state() {
                    crate::update::State::Available(_) => crate::update::install(),
                    crate::update::State::Downloading(_) | crate::update::State::Installing | crate::update::State::Checking => {}
                    _ => crate::update::check_now(false),
                }
                return;
            }
            F::Autostart => {
                let on = !crate::autostart::enabled();
                match crate::autostart::set(on) {
                    Ok(()) => self.say(if on { "Автозапуск включён: Rewind будет стартовать с Windows в трее" } else { "Автозапуск выключен" }, OK),
                    Err(e) => self.say(e, REC),
                }
                return;
            }
            F::XEnabled => s.crosshair_enabled = !s.crosshair_enabled,
            F::Folder => {
                // The folder dialog wants its own (STA) thread.
                let start = s.output_dir.clone();
                let picked = std::thread::spawn(move || rfd::FileDialog::new().set_directory(&start).pick_folder())
                    .join()
                    .ok()
                    .flatten();
                match picked {
                    Some(d) => s.output_dir = d,
                    None => return,
                }
            }
            F::HkSave => s.hk_save.mods = next_mods(s.hk_save.mods),
            F::HkToggle => s.hk_toggle.mods = next_mods(s.hk_toggle.mods),
            F::HkCross => s.hk_crosshair.mods = next_mods(s.hk_crosshair.mods),
            F::Fps | F::Length => {
                self.num_input = Some((f, String::new()));
                return;
            }
            _ => return self.change(f, 1, false),
        }
        self.ctrl.apply(s);
    }

    // ------------------------------------------------------------------ draw

    fn draw(&mut self, f: &mut Frame) {
        let s = self.settings();
        let [head, _, body, toast, foot] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(8),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(f.area());
        // Paint the whole screen in the theme first; widgets draw over it.
        f.render_widget(Block::new().style(Style::new().bg(BG).fg(TEXT)), f.area());
        self.hits.clear();
        let running = self.running();

        // Header: the name, then the tabs.
        let mut spans = vec![Span::styled(" REWIND", Style::new().fg(ACCENT).bold()), Span::raw("    ")];
        let mut x = head.x + 11;
        for (i, (p, name)) in [(Page::Main, "Главная"), (Page::Settings, "Настройки"), (Page::Voice, "Голос"), (Page::Crosshair, "Прицел")]
            .into_iter()
            .enumerate()
        {
            let t = format!(" {} {name} ", i + 1);
            let w = t.chars().count() as u16;
            let st = if p == self.page { Style::new().fg(Color::Black).bg(ACCENT).bold() } else { Style::new().fg(MUTED) };
            spans.push(Span::styled(t, st));
            spans.push(Span::raw(" "));
            self.hits.push((Rect::new(x, head.y, w, 1), MHit::Tab(p)));
            x += w + 1;
        }
        f.render_widget(Paragraph::new(Line::from(spans)), head);
        // A newer version: shown on the right of the header on every page.
        let badge = match crate::update::state() {
            crate::update::State::Available(r) => Some(format!("↑ есть Rewind {} — нажмите U ", r.version)),
            crate::update::State::Downloading(p) => Some(format!("↑ обновление… {p}% ")),
            crate::update::State::Installing => Some("↑ устанавливаю обновление… ".to_string()),
            _ => None,
        };
        if let Some(b) = badge {
            let w = (b.chars().count() as u16).min(head.width);
            f.render_widget(
                Paragraph::new(Span::styled(b, Style::new().fg(Color::Black).bg(ACCENT).bold())),
                Rect::new(head.x + head.width - w, head.y, w, 1),
            );
        }

        match self.page {
            Page::Main => self.draw_main(f, body, &s, running),
            _ => self.draw_fields(f, body, &s),
        }

        if let Some(t) = &self.toast {
            f.render_widget(Paragraph::new(Span::styled(format!(" {}", t.text), Style::new().fg(t.color))), toast);
        }

        let keys = match self.page {
            Page::Main => "2×клик — открыть · правый клик — в папке · Esc — в трей · Q — выход",
            _ if self.num_input.is_some() => "впишите число · Enter — применить · Esc — отмена",
            _ => "↑↓ · ←→ или клик по ‹ › · Enter — переключить · Esc — в трей",
        };
        f.render_widget(Paragraph::new(Span::styled(format!(" {keys}"), Style::new().fg(MUTED))), foot);

        // Console without 24-bit colour: use the nearest of the 16 standard
        // ones, so green stays green instead of dropping to no colour at all.
        if !TRUECOLOR.load(Ordering::Relaxed) {
            for cell in f.buffer_mut().content.iter_mut() {
                cell.fg = to_16(cell.fg);
                cell.bg = to_16(cell.bg);
            }
        }
    }

    fn draw_main(&mut self, f: &mut Frame, area: Rect, s: &Settings, running: bool) {
        let [status, buttons, clips] = Layout::vertical([Constraint::Length(3), Constraint::Length(3), Constraint::Min(4)]).areas(area);
        let frame = |c: Color| Block::bordered().border_style(Style::new().fg(c));

        // Status strip: recording or not on the left, what is recorded on the right.
        let (left, right) = {
            let p = self.ctrl.pipeline.lock().unwrap();
            match p.as_ref() {
                Some(p) => {
                    let sh = &p.shared;
                    let mut left = vec![
                        Span::styled(" ● ЗАПИСЬ", Style::new().fg(REC).bold()),
                        Span::styled(format!("   буфер {} мин", s.buffer_minutes), Style::new().fg(TEXT)),
                    ];
                    if sh.saving.load(Ordering::Relaxed) > 0 {
                        left.push(Span::styled("   сохраняю…", Style::new().fg(ACCENT).bold()));
                    }
                    let err = sh.error.lock().unwrap().clone();
                    let note = sh.format_note.lock().unwrap().clone().or_else(|| sh.audio_note.lock().unwrap().clone());
                    let right = if let Some(e) = err {
                        Line::styled(format!("Ошибка: {e} "), Style::new().fg(REC))
                    } else if let Some(n) = note {
                        Line::styled(format!("Внимание: {n} "), Style::new().fg(WARN))
                    } else {
                        let total = p.shared.buffer_secs.load(Ordering::Relaxed) * mf::bitrate(p.out_w, p.out_h, p.fps, s.quality) as u64 / 8;
                        Line::styled(
                            format!("{}×{} · {} к/с · {} · ≈ {} ", p.out_w, p.out_h, p.fps, s.quality.label(), util::fmt_bytes(total)),
                            Style::new().fg(MUTED),
                        )
                    };
                    (Line::from(left), right)
                }
                None => (
                    Line::from(vec![
                        Span::styled(" ○ БУФЕР ВЫКЛЮЧЕН", Style::new().fg(MUTED).bold()),
                        Span::styled("   нажмите «Буфер», чтобы включить", Style::new().fg(MUTED)),
                    ]),
                    Line::default(),
                ),
            }
        };
        let si = frame(LINE).inner(status);
        f.render_widget(frame(LINE), status);
        f.render_widget(Paragraph::new(left), si);
        f.render_widget(Paragraph::new(right).alignment(ratatui::layout::Alignment::Right), si);

        // Two switches and the save shortcut, as boxes side by side.
        let [b1, b2, b3] = Layout::horizontal([Constraint::Ratio(1, 3); 3]).areas(buttons);
        let switch = |name: &str, on: bool, w: u16| {
            let state = if on { Span::styled("● вкл", Style::new().fg(OK)) } else { Span::styled("○ выкл", Style::new().fg(MUTED)) };
            let pad = (w as usize).saturating_sub(name.chars().count() + state.width() + 4);
            Line::from(vec![Span::styled(format!(" {name}"), Style::new().fg(TEXT).bold()), Span::raw(" ".repeat(pad + 2)), state])
        };
        for (r, name, on, hit) in [(b1, "Буфер", running, MHit::ToggleBuffer), (b2, "Прицел", s.crosshair_enabled, MHit::ToggleCross)] {
            let b = frame(if on { ACCENT } else { LINE });
            let w = b.inner(r).width;
            f.render_widget(Paragraph::new(switch(name, on, w)).block(b), r);
            self.hits.push((r, hit));
        }
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" Сохранить ", Style::new().fg(MUTED)),
                Span::styled(s.hk_save.label(), Style::new().fg(ACCENT).bold()),
            ]))
            .block(frame(LINE)),
            b3,
        );

        // Clips as a table.
        let block = Block::bordered()
            .title(Line::from(vec![Span::styled(" Клипы", Style::new().fg(TEXT)), Span::styled(format!(" · {} ", self.clips.len()), Style::new().fg(MUTED))]))
            .border_style(Style::new().fg(LINE));
        let ci = block.inner(clips);
        f.render_widget(block, clips);
        let [header, list_r] = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(ci);
        f.render_widget(
            Paragraph::new(Span::styled(
                format!("  {:<10}{:<8}{:<10}{:<10}{}", "ДАТА", "ВРЕМЯ", "ТИП", "ДЛИНА", "РАЗМЕР"),
                Style::new().fg(MUTED),
            )),
            header,
        );
        let today = util::stamp();
        let today = &today[..10];
        let items: Vec<ListItem> = if self.clips.is_empty() {
            vec![ListItem::new(Line::styled("Пока пусто — сохранённые повторы появятся здесь", Style::new().fg(MUTED)))]
        } else {
            self.clips.iter().map(|c| ListItem::new(clip_line(c, today))).collect()
        };
        let list = List::new(items)
            .highlight_style(Style::new().bg(SEL_BG).fg(ACCENT).add_modifier(Modifier::BOLD))
            .highlight_symbol("▌ ");
        f.render_stateful_widget(list, list_r, &mut self.clip_state);
        self.list_area = list_r;
        self.list_offset = self.clip_state.offset();
        self.row_values.clear();
    }

    fn draw_fields(&mut self, f: &mut Frame, area: Rect, s: &Settings) {
        let fields = fields(self.page);
        let pi = self.page_idx();
        let mut i = self.sel[pi].min(fields.len().saturating_sub(1));
        if matches!(fields.get(i), Some(F::Group(_))) {
            i = step(&fields, i, 1);
        }
        self.sel[pi] = i;
        let failed = self.failed.lock().unwrap().clone();
        let mut values = Vec::new();
        let items: Vec<ListItem> = fields
            .iter()
            .map(|&fl| {
                if let F::Group(name) = fl {
                    values.push(String::new());
                    return ListItem::new(Line::styled(name, Style::new().fg(ACCENT).bold()));
                }
                let (label, value) = self.row(fl, s, &failed);
                values.push(value.iter().map(|sp| sp.content.as_ref()).collect::<String>());
                let mut spans = vec![Span::styled(format!(" {label:<w$}", w = LABEL_W as usize - 1), Style::new().fg(MUTED))];
                spans.extend(value);
                ListItem::new(Line::from(spans))
            })
            .collect();
        self.row_values = values;
        let mut state = ListState::default().with_selected(Some(self.sel[pi]));
        let title = match self.page {
            Page::Settings => " Настройки ",
            Page::Voice => " Голосовой чат — своя дорожка в клипе ",
            _ => " Прицел — поверх экрана, в запись не попадает ",
        };
        // Under the list: the voice page's status box, or a hint for the chosen row.
        let selected = fields.get(self.sel[pi]).copied();
        let (area, info) = if self.page == Page::Voice {
            let [a, b] = Layout::vertical([Constraint::Min(5), Constraint::Length(6)]).areas(area);
            (a, Some(b))
        } else {
            let [a, b] = Layout::vertical([Constraint::Min(5), Constraint::Length(3)]).areas(area);
            if let Some(fl) = selected {
                let (label, _) = self.row(fl, s, &failed);
                f.render_widget(
                    Paragraph::new(vec![
                        Line::from(vec![Span::styled(format!("{label}. "), Style::new().fg(ACCENT).bold()), Span::raw(hint(fl))]),
                    ])
                    .wrap(Wrap { trim: true })
                    .block(Block::bordered().title(" Подсказка ").border_style(Style::new().fg(LINE)).title_style(Style::new().fg(MUTED))),
                    b,
                );
            }
            (a, None)
        };
        let list = List::new(items)
            .block(Block::bordered().title(title).border_style(Style::new().fg(LINE)).title_style(Style::new().fg(ACCENT).bold()))
            // Marker + bold only: a row background would hide the green
            // pill of the chosen value (e.g. buffer length).
            .highlight_style(Style::new().add_modifier(Modifier::BOLD))
            .highlight_symbol("▌ ");
        f.render_stateful_widget(list, area, &mut state);
        self.list_area = inner(area);
        self.list_offset = state.offset();
        if let Some(info) = info {
            self.draw_voice_info(f, info, s);
        }
    }

    /// Which chats are on their own track right now, and what that means.
    fn draw_voice_info(&self, f: &mut Frame, area: Rect, s: &Settings) {
        let found = self.ctrl.pipeline.lock().unwrap().as_ref().map(|p| p.shared.chat_found.lock().unwrap().clone());
        let now = match (s.chat_track, found) {
            (false, _) => Line::styled("Выключено: голос из чатов идёт вместе со звуком игры", Style::new().fg(MUTED)),
            (true, None) => Line::styled("Буфер выключен — включите его на главной", Style::new().fg(MUTED)),
            (true, Some(v)) if v.is_empty() => Line::styled("Сейчас ни один из выбранных чатов не запущен", Style::new().fg(MUTED)),
            (true, Some(v)) => Line::from(vec![Span::styled("Сейчас на своей дорожке: ", Style::new().fg(MUTED)), Span::styled(v.join(", "), Style::new().fg(OK).bold())]),
        };
        let lines = vec![
            now,
            Line::styled("Звук отмеченных программ пишется отдельно и не попадает в дорожку игры.", Style::new().fg(MUTED)),
            Line::styled("Дорожки в клипе: 1 — всё вместе, 2 — игра, 3 — голосовой чат, 4 — микрофон.", Style::new().fg(MUTED)),
        ];
        f.render_widget(
            Paragraph::new(lines).wrap(Wrap { trim: true }).block(Block::bordered().title(" Сейчас ").border_style(Style::new().fg(LINE)).title_style(Style::new().fg(ACCENT).bold())),
            area,
        );
    }

    // ------------------------------------------------------------------ mouse

    fn mouse(&mut self, m: MouseEvent) {
        let (x, y) = (m.column, m.row);
        let shift = m.modifiers.contains(KeyModifiers::SHIFT);
        let inside = |r: Rect| x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height;
        let in_list = inside(self.list_area);
        let idx = self.list_offset + y.saturating_sub(self.list_area.y) as usize;
        match m.kind {
            MouseEventKind::ScrollDown => self.scroll(1),
            MouseEventKind::ScrollUp => self.scroll(-1),
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(h) = self.hits.iter().find(|(r, _)| inside(*r)).map(|(_, h)| *h) {
                    match h {
                        MHit::Tab(p) => self.page = p,
                        MHit::ToggleBuffer => self.ctrl.run(Action::ToggleReplay),
                        MHit::ToggleCross => self.ctrl.run(Action::ToggleCrosshair),
                    }
                    return;
                }
                if !in_list {
                    return;
                }
                if self.page == Page::Main {
                    if idx >= self.clips.len() {
                        return;
                    }
                    let double = self.last_click.is_some_and(|(t, i)| i == idx && t.elapsed() < Duration::from_millis(450));
                    self.clip_state.select(Some(idx));
                    if double {
                        util::open_path(&self.clips[idx].path);
                        self.last_click = None;
                    } else {
                        self.last_click = Some((Instant::now(), idx));
                    }
                } else {
                    self.click_field(idx, x - self.list_area.x, shift);
                }
            }
            MouseEventKind::Down(MouseButton::Right) if self.page == Page::Main && in_list => {
                if let Some(c) = self.clips.get(idx) {
                    self.clip_state.select(Some(idx));
                    util::reveal(&c.path);
                }
            }
            _ => {}
        }
    }

    /// Wheel: move the selection in the list on screen.
    fn scroll(&mut self, d: i32) {
        if self.page == Page::Main {
            let n = self.clips.len();
            if n > 0 {
                let i = self.clip_state.selected().unwrap_or(0) as i32 + d;
                self.clip_state.select(Some(i.clamp(0, n as i32 - 1) as usize));
            }
        } else {
            let fields = fields(self.page);
            let pi = self.page_idx();
            self.sel[pi] = step(&fields, self.sel[pi], d);
        }
    }

    /// A click on a settings row at column `col` (from the list's left edge).
    fn click_field(&mut self, idx: usize, col: u16, shift: bool) {
        let fields = fields(self.page);
        let Some(&f) = fields.get(idx) else { return };
        if matches!(f, F::Group(_)) {
            return;
        }
        self.sel[self.page_idx()] = idx;
        self.num_input = None;
        let switch = matches!(f, F::SaveSound | F::CheckUpdates | F::Update | F::Autostart | F::Replay | F::Game | F::Chat | F::ChatApp(_) | F::Mic | F::XEnabled);
        let rel = col as i32 - (MARK_W + LABEL_W) as i32;
        if rel < 0 {
            // On the label: switches flip, other rows just get selected.
            if switch {
                self.activate(f);
            }
            return;
        }
        let rel = rel as usize;
        let val: Vec<char> = self.row_values.get(idx).map(|v| v.chars().collect()).unwrap_or_default();
        // Arrows get a cell of slack on each side.
        let near = |c: char| [rel.saturating_sub(1), rel, rel + 1].iter().any(|&i| val.get(i) == Some(&c));
        if near('‹') {
            self.change(f, -1, shift);
        } else if near('›') {
            self.change(f, 1, shift);
        } else if rel < val.len() {
            self.activate(f);
        }
    }

    /// Label and value of a row.
    fn row(&self, fl: F, s: &Settings, failed: &std::collections::HashMap<Action, String>) -> (String, Vec<Span<'static>>) {
        let on = |b: bool| {
            if b {
                Span::styled("● вкл", Style::new().fg(OK))
            } else {
                Span::styled("○ выкл", Style::new().fg(MUTED))
            }
        };
        let plain = |t: String| vec![Span::raw(t)];
        let hk = |h: Hotkey, a: Action| {
            let mut v = vec![Span::raw(format!("‹ {} ›", h.label()))];
            if let Some(e) = failed.get(&a) {
                v.push(Span::styled(format!("  {e}"), Style::new().fg(WARN)));
            }
            v
        };
        match fl {
            F::Replay => ("Буфер повтора".into(), vec![on(s.replay_enabled && self.running())]),
            F::Length => ("Длина буфера".into(), {
                match &self.num_input {
                    Some((F::Length, b)) => vec![
                        Span::styled(format!("{b}_"), Style::new().fg(Color::Black).bg(ACCENT)),
                        Span::styled(format!(" мин  (от {MINUTES_MIN} до {MINUTES_MAX})"), Style::new().fg(MUTED)),
                    ],
                    _ => vec![
                        Span::raw(format!("‹ {} мин ›", s.buffer_minutes)),
                        Span::styled("  можно вписать цифрами", Style::new().fg(MUTED)),
                    ],
                }
            }),
            F::Monitor => ("Экран".into(), plain(monitor_label(&self.ctrl.monitors, s.monitor))),
            F::Fps => ("Кадров в секунду".into(), {
                match &self.num_input {
                    Some((F::Fps, b)) => vec![Span::styled(format!("{b}_"), Style::new().fg(Color::Black).bg(ACCENT))],
                    _ => {
                        let mut v = vec![Span::raw(format!("‹ {} ›", s.fps))];
                        let hz = self.ctrl.monitors.get(s.monitor).map_or(0, |m| m.hz);
                        if hz > 0 && s.fps > hz {
                            v.push(Span::styled(format!("  экран {hz} Гц — кадров будет не больше"), Style::new().fg(WARN)));
                        }
                        v
                    }
                }
            }),
            F::Resolution => ("Разрешение".into(), plain(format!("‹ {} ›", s.resolution.label()))),
            F::Quality => ("Качество".into(), {
                let mut v = vec![Span::raw(format!("‹ {} ›", s.quality.label()))];
                if let Some(m) = self.ctrl.monitors.get(s.monitor) {
                    let (w, h) = s.resolution.output_size(m.width, m.height);
                    let total = mf::bitrate(w, h, s.fps, s.quality) as f64 / 8.0 * s.buffer_minutes as f64 * 60.0;
                    v.push(Span::styled(format!("  ≈ {} на {} мин", util::fmt_bytes(total as u64), s.buffer_minutes), Style::new().fg(MUTED)));
                }
                v
            }),
            F::Game => ("Звук игры и ПК".into(), vec![on(s.system_audio)]),
            F::Chat => ("Отдельная дорожка".into(), vec![on(s.chat_track)]),
            F::ChatApp(i) => {
                let name = audio::CHAT_APPS[i].0;
                (name.into(), vec![on(s.chat_apps.iter().any(|a| a == name))])
            }
            F::Mic => ("Микрофон".into(), vec![on(s.microphone)]),
            F::Folder => ("Папка".into(), plain(s.output_dir.display().to_string())),
            F::HkSave => ("Сохранить клип".into(), hk(s.hk_save, Action::SaveReplay)),
            F::HkToggle => ("Буфер вкл/выкл".into(), hk(s.hk_toggle, Action::ToggleReplay)),
            F::XEnabled => ("Прицел".into(), vec![on(s.crosshair_enabled)]),
            F::XKind => ("Вид".into(), plain(format!("‹ {} ›", s.crosshair_kind.label()))),
            F::XSize => ("Размер".into(), plain(format!("‹ {} px ›", s.crosshair_size))),
            F::XThick => ("Толщина".into(), plain(format!("‹ {} px ›", s.crosshair_thickness))),
            F::XColor => ("Цвет".into(), {
                let [r, g, b] = s.crosshair_color;
                let name = COLORS.iter().find(|c| c.1 == s.crosshair_color).map_or("свой".to_string(), |c| c.0.to_string());
                vec![Span::styled("■■ ", Style::new().fg(Color::Rgb(r, g, b))), Span::raw(format!("‹ {name} ›"))]
            }),
            F::HkCross => ("Прицел вкл/выкл".into(), hk(s.hk_crosshair, Action::ToggleCrosshair)),
            F::Autostart => ("Запуск с Windows".into(), vec![on(crate::autostart::enabled())]),
            F::CheckUpdates => ("Проверять обновления".into(), vec![on(s.check_updates)]),
            F::SaveSound => ("Звук при сохранении".into(), vec![on(s.sound_on_save)]),
            F::Update => ("Обновление".into(), update_value()),
            F::Group(name) => (name.into(), Vec::new()),
        }
    }
}

fn fields(page: Page) -> Vec<F> {
    use F::*;
    match page {
        Page::Main => Vec::new(),
        Page::Settings => vec![
            Group("ЗАПИСЬ"),
            Replay,
            Length,
            Monitor,
            Fps,
            Resolution,
            Quality,
            Group("ЗВУК"),
            Game,
            Mic,
            SaveSound,
            Group("ПАПКА И КЛАВИШИ"),
            Folder,
            HkSave,
            HkToggle,
            Group("СИСТЕМА"),
            Autostart,
            CheckUpdates,
            Update,
        ],
        Page::Voice => [Chat, Group("ПРОГРАММЫ")].into_iter().chain((0..audio::CHAT_APPS.len()).map(ChatApp)).collect(),
        Page::Crosshair => vec![XEnabled, Group("ВИД"), XKind, XSize, XThick, XColor, Group("КЛАВИША"), HkCross],
    }
}

/// The "Обновление" row: current version and what the updater is doing.
fn update_value() -> Vec<Span<'static>> {
    use crate::update::State;
    let cur = crate::update::current();
    match crate::update::state() {
        State::Idle => vec![Span::raw(format!("v{cur}")), Span::styled("  Enter — проверить", Style::new().fg(MUTED))],
        State::Checking => vec![Span::raw(format!("v{cur}")), Span::styled("  проверяю…", Style::new().fg(MUTED))],
        State::UpToDate => vec![Span::raw(format!("v{cur}")), Span::styled("  последняя версия", Style::new().fg(OK))],
        State::Available(r) => vec![
            Span::styled(format!("есть v{}", r.version), Style::new().fg(ACCENT).bold()),
            Span::styled("  Enter или U — обновить", Style::new().fg(MUTED)),
        ],
        State::Downloading(p) => vec![Span::styled(format!("скачиваю… {p}%"), Style::new().fg(ACCENT))],
        State::Installing => vec![Span::styled("устанавливаю — Rewind перезапустится", Style::new().fg(ACCENT))],
        State::Failed(e) => vec![Span::styled(format!("не вышло: {e}"), Style::new().fg(WARN)), Span::styled("  Enter — ещё раз", Style::new().fg(MUTED))],
    }
}

/// One-line explanation of a settings row, shown under the list.
fn hint(f: F) -> &'static str {
    match f {
        F::Replay => "Пишет последние минуты экрана, пока включён. Выключите, если не нужен.",
        F::Length => "Сколько последних минут попадёт в клип. Буфер лежит на диске: чем длиннее, тем больше места.",
        F::Monitor => "Какой экран записывать.",
        F::Fps => "Кадров в секунду в клипе. Больше — плавнее, но тяжелее для диска и процессора.",
        F::Resolution => "Размер картинки в клипе. Меньше — файлы легче.",
        F::Quality => "Чем выше, тем чётче картинка и больше файл.",
        F::Game => "Звук игры и всего компьютера.",
        F::Mic => "Ваш микрофон, на своей дорожке в клипе.",
        F::Folder => "Куда сохраняются клипы. Enter или щелчок — выбрать другую папку.",
        F::HkSave => "Сохранить повтор из любой игры. ←→ — клавиша F, Enter — Ctrl/Alt/Shift.",
        F::HkToggle => "Включить или выключить буфер из любой игры.",
        F::XEnabled => "Прицел поверх экрана. В запись не попадает.",
        F::XKind => "Форма прицела.",
        F::XSize | F::XThick => "В пикселях. Shift + ←→ — шаг больше.",
        F::XColor => "Цвет прицела.",
        F::HkCross => "Включить или выключить прицел из любой игры.",
        F::Autostart => "Запускать Rewind при входе в Windows, сразу свёрнутым в трей.",
        F::SaveSound => "Короткий сигнал после сохранения — слышно и в полноэкранной игре. В запись не попадает.",
        F::CheckUpdates => "Раз в несколько часов проверять, вышла ли новая версия, и сообщать об этом.",
        F::Update => "Enter — проверить сейчас или установить новую версию; Rewind перезапустится обновлённым.",
        F::Chat | F::ChatApp(_) | F::Group(_) => "",
    }
}

/// The next selectable row from `i` in direction `d` (headings are skipped);
/// stays put at the ends.
fn step(fields: &[F], i: usize, d: i32) -> usize {
    let mut j = i as i32;
    loop {
        j += d.signum();
        match fields.get(j.max(0) as usize) {
            _ if j < 0 => return i,
            None => return i,
            Some(F::Group(_)) => continue,
            Some(_) => return j as usize,
        }
    }
}

/// A clip row: "сегодня 23:53   Клип   20:00    4.98 ГБ".
fn clip_line(c: &Clip, today: &str) -> Line<'static> {
    let dur = c.secs.map(|x| util::fmt_clock((x * 1000.0) as u64)).unwrap_or_default();
    let size = util::fmt_bytes(c.bytes);
    // Names look like "Rewind 2026-11-04 23-53-37".
    let mut parts = c.name.splitn(3, ' ');
    let (kind, date, time) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    // "Rewind …" now, "Повтор …" before the rename: both are clips.
    let kind = if kind == "Rewind" || kind == "Повтор" { "Клип" } else { kind };
    let (day, hm) = match (date.get(5..7), date.get(8..10), time.get(..5)) {
        (Some(mo), Some(d), Some(hm)) if date.len() == 10 => {
            (if date == today { "сегодня".to_string() } else { format!("{d}.{mo}") }, hm.replace('-', ":"))
        }
        _ => return Line::from(vec![Span::raw(c.name.clone()), Span::styled(format!("   {dur}  {size}"), Style::new().fg(MUTED))]),
    };
    // Same columns as the header in draw_main.
    let dur = if dur.is_empty() { "—".to_string() } else { dur };
    Line::from(vec![
        Span::raw(format!("{day:<10}{hm:<8}")),
        Span::styled(format!("{kind:<10}"), Style::new().fg(MUTED)),
        Span::raw(format!("{dur:<10}")),
        Span::styled(size, Style::new().fg(MUTED)),
    ])
}

fn monitor_label(m: &[MonitorInfo], i: usize) -> String {
    m.get(i).map(|x| format!("‹ {} ›", x.label(i))).unwrap_or_else(|| "не найден".into())
}

fn cycle(i: usize, n: usize, dir: i32) -> usize {
    ((i as i32 + dir).rem_euclid(n as i32)) as usize
}

fn fkey(k: u8, dir: i32) -> u8 {
    ((k as i32 - 1 + dir).rem_euclid(12) + 1) as u8
}

fn next_mods(m: Mods) -> Mods {
    let i = Mods::ALL.iter().position(|&x| x == m).unwrap_or(0);
    Mods::ALL[(i + 1) % Mods::ALL.len()]
}

#[cfg(test)]
mod tests {
    use super::{clip_line, step, Clip, F};
    use std::path::PathBuf;

    fn text(c: &Clip, today: &str) -> String {
        clip_line(c, today).spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn clip_rows_show_date_time_and_kind() {
        let c = Clip { path: PathBuf::new(), name: "Rewind 2026-11-05 17-48-03".into(), bytes: 970_000_000, secs: Some(313.0) };
        let t = text(&c, "2026-11-05");
        assert!(t.starts_with("сегодня   17:48"), "{t}");
        assert!(t.contains("Клип") && t.contains("05:13") && t.contains("970 МБ"), "{t}");
        // Clips from before the rename are clips too.
        let old = Clip { name: "Повтор 2026-11-04 23-53-37".into(), ..c };
        let t = text(&old, "2026-11-05");
        assert!(t.starts_with("04.11") && t.contains("Клип"), "{t}");
    }

    #[test]
    fn selection_skips_group_headings() {
        let f = [F::Group("A"), F::Replay, F::Group("B"), F::Mic];
        assert_eq!(step(&f, 1, 1), 3);
        assert_eq!(step(&f, 3, -1), 1);
        assert_eq!(step(&f, 1, -1), 1); // nothing selectable above
        assert_eq!(step(&f, 3, 1), 3);
    }
}

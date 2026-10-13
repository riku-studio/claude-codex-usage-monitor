use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, AtomicU8, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use windows::core::PCWSTR;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleW};
use windows::Win32::System::Registry::*;
use windows::Win32::System::RemoteDesktop::{
    WTSRegisterSessionNotification, WTSUnRegisterSessionNotification, NOTIFY_FOR_THIS_SESSION,
};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Accessibility::HWINEVENTHOOK;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows::Win32::UI::Shell::ExtractIconExW;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::character::{self, CharacterKind};
use crate::diagnose;
use crate::localization::{self, LanguageId, Strings};
use crate::models::AppUsageData;
use crate::native_interop::{
    self, Color, TIMER_COUNTDOWN, TIMER_POLL, TIMER_RESET_POLL, TIMER_UPDATE_CHECK, WM_APP_TRAY,
    WM_APP_USAGE_UPDATED,
};
use crate::poller;
use crate::quota_layout::{DisplayLimits, Provider, Quota};
use crate::theme;
use crate::tray_icon;
use crate::updater::{self, InstallChannel, ReleaseDescriptor, UpdateCheckResult};

/// Wrapper to make HWND sendable across threads (safe for PostMessage usage)
#[derive(Clone, Copy)]
struct SendHwnd(isize);

unsafe impl Send for SendHwnd {}

impl SendHwnd {
    fn from_hwnd(hwnd: HWND) -> Self {
        Self(hwnd.0 as isize)
    }
    fn to_hwnd(self) -> HWND {
        HWND(self.0 as *mut _)
    }
}

/// Shared application state
struct AppState {
    hwnd: SendHwnd,
    taskbar_hwnd: Option<HWND>,
    tray_notify_hwnd: Option<HWND>,
    win_event_hook: Option<HWINEVENTHOOK>,
    overlay_event_hooks: Vec<HWINEVENTHOOK>,
    is_dark: bool,
    language_override: Option<LanguageId>,
    language: LanguageId,
    install_channel: InstallChannel,

    session_percent: f64,
    session_text: String,
    weekly_percent: f64,
    weekly_text: String,
    codex_session_percent: f64,
    codex_session_text: String,
    codex_weekly_percent: f64,
    codex_weekly_text: String,
    show_claude_code: bool,
    show_codex: bool,
    display_limits: DisplayLimits,

    data: Option<AppUsageData>,

    poll_interval_ms: u32,
    retry_count: u32,
    force_notify_auth_error: bool,
    auth_error_paused_polling: bool,
    auth_watch_mode: poller::CredentialWatchMode,
    auth_watch_snapshot: poller::CredentialWatchSnapshot,
    last_poll_ok: bool,
    update_status: UpdateStatus,
    last_update_check_unix: Option<u64>,

    tray_offset: i32,
    dragging: bool,
    drag_start_mouse_x: i32,
    drag_start_offset: i32,

    widget_visible: bool,
    shutting_down: bool,
}

#[derive(Clone, Debug)]
enum UpdateStatus {
    Idle,
    Checking,
    Applying,
    UpToDate,
    Available(ReleaseDescriptor),
}

const RETRY_BASE_MS: u32 = 30_000; // 30 seconds

const POLL_1_MIN: u32 = 60_000;
const POLL_5_MIN: u32 = 300_000;
const POLL_15_MIN: u32 = 900_000;
const POLL_1_HOUR: u32 = 3_600_000;

// Menu item IDs for update frequency
const IDM_FREQ_1MIN: u16 = 10;
const IDM_FREQ_5MIN: u16 = 11;
const IDM_FREQ_15MIN: u16 = 12;
const IDM_FREQ_1HOUR: u16 = 13;
const IDM_START_WITH_WINDOWS: u16 = 20;
const IDM_RESET_POSITION: u16 = 30;
const IDM_VERSION_ACTION: u16 = 31;
const IDM_LANG_SYSTEM: u16 = 40;
const IDM_LANG_ENGLISH: u16 = 41;
const IDM_LANG_DUTCH: u16 = 42;
const IDM_LANG_SPANISH: u16 = 43;
const IDM_LANG_FRENCH: u16 = 44;
const IDM_LANG_GERMAN: u16 = 45;
const IDM_LANG_JAPANESE: u16 = 46;
const IDM_LANG_KOREAN: u16 = 47;
const IDM_LANG_TRADITIONAL_CHINESE: u16 = 48;
const IDM_MODEL_CLAUDE_CODE: u16 = 60;
const IDM_MODEL_CODEX: u16 = 61;
const IDM_CLAUDE_5H: u16 = 62;
const IDM_CLAUDE_7D: u16 = 63;
const IDM_CODEX_5H: u16 = 64;
const IDM_CODEX_7D: u16 = 65;

const IDM_BAR_THEME_SEGMENTED: u16 = 70;
const IDM_BAR_THEME_FLAT: u16 = 71;
const IDM_BAR_THEME_GRADIENT: u16 = 72;
const IDM_BAR_THEME_PIXEL: u16 = 73;

const IDM_CHAR_SHOW: u16 = 80;
const IDM_CHAR_CAT: u16 = 81;
const IDM_CHAR_DOG: u16 = 82;
const IDM_CHAR_BOTH: u16 = 83;
const IDM_CHAR_GIRL: u16 = 89;
const IDM_CAT_COLOR_0: u16 = 84;
const IDM_CAT_COLOR_1: u16 = 85;
const IDM_DOG_COLOR_0: u16 = 86;
const IDM_DOG_COLOR_1: u16 = 87;

const IDM_SEG_4: u16 = 90;
const IDM_SEG_6: u16 = 91;
const IDM_SEG_8: u16 = 92;
const IDM_SEG_10: u16 = 93;
const IDM_TOGGLE_LABELS: u16 = 94;
const IDM_TOGGLE_PERCENT: u16 = 95;
const IDM_TOGGLE_TIMER: u16 = 96;
const IDM_TOGGLE_DETAILED: u16 = 97;
const IDM_PACE_TOGGLE: u16 = 98;

const TIMER_RECOVERY: usize = 5;
const TIMER_OVERLAY_SETTLE: usize = 6;
const WM_APP_OVERLAY_REFRESH: u32 = WM_APP + 4;
static OVERLAY_REFRESH_PENDING: AtomicBool = AtomicBool::new(false);
static RECOVERY_WINDOW: AtomicIsize = AtomicIsize::new(0);
static RECREATE_WIDGET: AtomicBool = AtomicBool::new(false);
static TASKBAR_CREATED_MSG: AtomicU32 = AtomicU32::new(0);

const DIVIDER_HIT_ZONE: i32 = 13; // LEFT_DIVIDER_W + DIVIDER_RIGHT_MARGIN

const WM_DPICHANGED_MSG: u32 = 0x02E0;
const WM_APP_UPDATE_CHECK_COMPLETE: u32 = WM_APP + 2;
const TRAY_ICON_UPDATE_REPOSITION_SUPPRESS_MS: u64 = 750;

static SUPPRESS_TRAY_REPOSITION_UNTIL: Mutex<Option<Instant>> = Mutex::new(None);

/// Current system DPI (96 = 100% scaling, 144 = 150%, 192 = 200%, etc.)
static CURRENT_DPI: AtomicU32 = AtomicU32::new(96);

/// Selected usage-bar visual theme, stored lock-free so the paint path can read
/// it without taking the state lock. Kept in sync with the persisted setting.
static CURRENT_BAR_THEME: AtomicU8 = AtomicU8::new(0);

/// Visual style of the usage bar. `Segmented` is the original default and keeps
/// the brand accent colors; the other themes color-code by usage threshold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BarTheme {
    Segmented,
    Flat,
    Gradient,
    Pixel,
}

impl BarTheme {
    const ALL: [BarTheme; 4] = [
        BarTheme::Segmented,
        BarTheme::Flat,
        BarTheme::Gradient,
        BarTheme::Pixel,
    ];

    fn from_u8(value: u8) -> Self {
        match value {
            1 => BarTheme::Flat,
            2 => BarTheme::Gradient,
            3 => BarTheme::Pixel,
            _ => BarTheme::Segmented,
        }
    }

    fn to_u8(self) -> u8 {
        match self {
            BarTheme::Segmented => 0,
            BarTheme::Flat => 1,
            BarTheme::Gradient => 2,
            BarTheme::Pixel => 3,
        }
    }

    fn code(self) -> &'static str {
        match self {
            BarTheme::Segmented => "segmented",
            BarTheme::Flat => "flat",
            BarTheme::Gradient => "gradient",
            BarTheme::Pixel => "pixel",
        }
    }

    fn from_code(code: &str) -> Option<Self> {
        match code {
            "segmented" => Some(BarTheme::Segmented),
            "flat" => Some(BarTheme::Flat),
            "gradient" => Some(BarTheme::Gradient),
            "pixel" => Some(BarTheme::Pixel),
            _ => None,
        }
    }

    fn label(self, strings: Strings) -> &'static str {
        match self {
            BarTheme::Segmented => strings.bar_theme_segmented,
            BarTheme::Flat => strings.bar_theme_flat,
            BarTheme::Gradient => strings.bar_theme_gradient,
            BarTheme::Pixel => strings.bar_theme_pixel,
        }
    }

    /// Themes other than the default color-code the fill by usage threshold.
    fn uses_threshold_color(self) -> bool {
        !matches!(self, BarTheme::Segmented)
    }
}

fn current_bar_theme() -> BarTheme {
    BarTheme::from_u8(CURRENT_BAR_THEME.load(Ordering::Relaxed))
}

/// Configurable compact-UI state, kept lock-free so the paint and width-calc
/// paths can read it without the state lock.
static CURRENT_SEGMENT_COUNT: AtomicU8 = AtomicU8::new(10);
static SHOW_LABELS: AtomicBool = AtomicBool::new(true);
static SHOW_PERCENTAGES: AtomicBool = AtomicBool::new(true);
static SHOW_RESET_TIMER: AtomicBool = AtomicBool::new(true);
static SHOW_DETAILED_REMAINING: AtomicBool = AtomicBool::new(false);
/// Pace indicator style: 0 = Off, 1 = Tick, 2 = Solid.
static PACE_STYLE: AtomicU8 = AtomicU8::new(0);

fn current_segment_count() -> i32 {
    match CURRENT_SEGMENT_COUNT.load(Ordering::Relaxed) {
        4 => 4,
        6 => 6,
        8 => 8,
        _ => 10,
    }
}

fn show_labels() -> bool {
    SHOW_LABELS.load(Ordering::Relaxed)
}

fn show_percentages() -> bool {
    SHOW_PERCENTAGES.load(Ordering::Relaxed)
}

fn show_reset_timer() -> bool {
    SHOW_RESET_TIMER.load(Ordering::Relaxed)
}

fn show_detailed_remaining() -> bool {
    SHOW_DETAILED_REMAINING.load(Ordering::Relaxed)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PaceStyle {
    Off,
    Tick,
}

impl PaceStyle {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => PaceStyle::Tick,
            _ => PaceStyle::Off,
        }
    }

    fn to_u8(self) -> u8 {
        match self {
            PaceStyle::Off => 0,
            PaceStyle::Tick => 1,
        }
    }

    fn code(self) -> &'static str {
        match self {
            PaceStyle::Off => "Off",
            PaceStyle::Tick => "Tick",
        }
    }

    fn from_code(code: &str) -> Self {
        match code {
            "Tick" => PaceStyle::Tick,
            _ => PaceStyle::Off,
        }
    }
}

fn pace_style() -> PaceStyle {
    PaceStyle::from_u8(PACE_STYLE.load(Ordering::Relaxed))
}

const PACE_SESSION_WINDOW_SECS: f64 = 5.0 * 3600.0;
const PACE_WEEKLY_WINDOW_SECS: f64 = 7.0 * 86400.0;

/// Where usage *should* be (0.0..1.0) if spread evenly across the window, i.e.
/// elapsed_time / window_length. None when pace is off or no reset time.
fn pace_expected(resets_at: Option<SystemTime>, window_secs: f64) -> Option<f64> {
    if pace_style() == PaceStyle::Off {
        return None;
    }
    let reset = resets_at?;
    let remaining = reset.duration_since(SystemTime::now()).ok()?.as_secs() as f64;
    let elapsed = window_secs - remaining;
    Some((elapsed / window_secs).clamp(0.0, 1.0))
}

/// Logical width of the text column, sized to what's actually shown so a single
/// element (e.g. just "62%") doesn't leave a wide blank gap before the right
/// edge. Wider when the detailed remaining time is on. Returns 0 when nothing
/// is shown.
fn text_column_width_logical() -> i32 {
    let detailed = show_detailed_remaining();
    match (show_percentages(), show_reset_timer()) {
        (true, true) => {
            if detailed {
                100 // "100% · 23h 59m" (incl. CJK suffixes)
            } else {
                TEXT_WIDTH
            }
        }
        (true, false) => 36, // "100%"
        (false, true) => {
            if detailed {
                72 // "23h 59m" / "23時間59分"
            } else {
                50
            }
        }
        (false, false) => 0,
    }
}

/// Scale a base pixel value (designed at 96 DPI) to the current DPI.
fn sc(px: i32) -> i32 {
    let dpi = CURRENT_DPI.load(Ordering::Relaxed);
    (px as f64 * dpi as f64 / 96.0).round() as i32
}

/// Re-query the monitor DPI for our window and update the cached value.
/// Uses GetDpiForWindow which returns the live DPI (unlike GetDpiForSystem
/// which is cached at process startup and never changes).
fn refresh_dpi() {
    let hwnd = {
        let state = lock_state();
        state.as_ref().map(|s| s.hwnd.to_hwnd())
    };
    if let Some(hwnd) = hwnd {
        let dpi = unsafe { GetDpiForWindow(hwnd) };
        if dpi > 0 {
            CURRENT_DPI.store(dpi, Ordering::Relaxed);
        }
    }
}

fn load_embedded_app_icons() -> (HICON, HICON) {
    unsafe {
        let mut exe_buf = [0u16; 260];
        let len = GetModuleFileNameW(None, &mut exe_buf) as usize;
        if len == 0 {
            return (HICON::default(), HICON::default());
        }

        let mut large_icon = HICON::default();
        let mut small_icon = HICON::default();
        let extracted = ExtractIconExW(
            PCWSTR::from_raw(exe_buf.as_ptr()),
            0,
            Some(&mut large_icon),
            Some(&mut small_icon),
            1,
        );

        if extracted == 0 {
            (HICON::default(), HICON::default())
        } else {
            (large_icon, small_icon)
        }
    }
}

unsafe impl Send for AppState {}

static STATE: Mutex<Option<AppState>> = Mutex::new(None);

/// Lock STATE safely, recovering from poisoned mutex
fn lock_state() -> MutexGuard<'static, Option<AppState>> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

fn settings_path() -> PathBuf {
    let appdata = std::env::var("APPDATA").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(appdata)
        .join("ClaudeCodexUsageMonitor")
        .join("settings.json")
}

#[derive(Debug, Serialize, Deserialize)]
struct SettingsFile {
    #[serde(default)]
    tray_offset: i32,
    #[serde(default = "default_poll_interval")]
    poll_interval_ms: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    language: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_update_check_unix: Option<u64>,
    #[serde(default = "default_widget_visible")]
    widget_visible: bool,
    #[serde(default = "default_show_claude_code")]
    show_claude_code: bool,
    #[serde(default = "default_show_codex")]
    show_codex: bool,
    #[serde(flatten)]
    display_limits: DisplayLimits,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    bar_theme: Option<String>,
    #[serde(default = "default_characters_enabled")]
    characters_enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    character_kind: Option<String>,
    #[serde(default)]
    cat_variant: u8,
    #[serde(default)]
    dog_variant: u8,
    #[serde(default = "default_segment_count")]
    segment_count: u8,
    #[serde(default = "default_true")]
    show_labels: bool,
    #[serde(default = "default_true")]
    show_percentages: bool,
    #[serde(default = "default_true")]
    show_reset_timer: bool,
    #[serde(default)]
    show_detailed_remaining: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pace_indicator_style: Option<String>,
}

impl Default for SettingsFile {
    fn default() -> Self {
        Self {
            tray_offset: 0,
            poll_interval_ms: default_poll_interval(),
            language: None,
            last_update_check_unix: None,
            widget_visible: true,
            show_claude_code: true,
            show_codex: false,
            display_limits: DisplayLimits::default(),
            bar_theme: None,
            characters_enabled: true,
            character_kind: None,
            cat_variant: 0,
            dog_variant: 0,
            segment_count: 10,
            show_labels: true,
            show_percentages: true,
            show_reset_timer: true,
            show_detailed_remaining: false,
            pace_indicator_style: None,
        }
    }
}

fn default_poll_interval() -> u32 {
    POLL_15_MIN
}

fn default_widget_visible() -> bool {
    true
}

fn default_show_claude_code() -> bool {
    true
}

fn default_show_codex() -> bool {
    false
}

fn default_characters_enabled() -> bool {
    true
}

fn default_segment_count() -> u8 {
    10
}

fn default_true() -> bool {
    true
}

fn load_settings() -> SettingsFile {
    let path = settings_path();
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return SettingsFile::default(),
    };
    // `#[serde(default)]` covers missing fields, but a malformed file (wrong
    // type on one field, manual edit error) would otherwise silently reset every
    // saved setting. Log it and keep a .bak copy so the reset is diagnosable and
    // the user can recover their old settings.
    let mut settings: SettingsFile = match serde_json::from_str(&content) {
        Ok(s) => s,
        Err(error) => {
            diagnose::log_error("settings parse failed; reverting to defaults", &error);
            let backup = path.with_extension("json.bak");
            let _ = std::fs::copy(&path, &backup);
            SettingsFile::default()
        }
    };
    if !settings.show_claude_code && !settings.show_codex {
        settings.show_claude_code = true;
    }
    settings.display_limits.normalize();
    settings
}

fn save_settings(settings: &SettingsFile) {
    let path = settings_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(settings) {
        let _ = std::fs::write(path, json);
    }
}

fn save_state_settings() {
    let state = lock_state();
    if let Some(s) = state.as_ref() {
        save_settings(&SettingsFile {
            tray_offset: s.tray_offset,
            poll_interval_ms: s.poll_interval_ms,
            language: s
                .language_override
                .map(|language| language.code().to_string()),
            last_update_check_unix: s.last_update_check_unix,
            widget_visible: s.widget_visible,
            show_claude_code: s.show_claude_code,
            show_codex: s.show_codex,
            display_limits: s.display_limits,
            bar_theme: Some(current_bar_theme().code().to_string()),
            characters_enabled: character::is_enabled(),
            character_kind: Some(character::current_kind().code().to_string()),
            cat_variant: character::cat_variant(),
            dog_variant: character::dog_variant(),
            segment_count: current_segment_count() as u8,
            show_labels: show_labels(),
            show_percentages: show_percentages(),
            show_reset_timer: show_reset_timer(),
            show_detailed_remaining: show_detailed_remaining(),
            pace_indicator_style: Some(pace_style().code().to_string()),
        });
    }
}

fn tray_icon_data_from_state() -> Vec<tray_icon::TrayIconData> {
    let state = lock_state();
    match state.as_ref() {
        Some(s) if s.last_poll_ok => {
            let mut icons = Vec::new();
            let limits = effective_display_limits(s);
            for (
                provider,
                enabled,
                kind,
                name,
                session_percent,
                weekly_percent,
                session_text,
                weekly_text,
            ) in [
                (
                    Provider::Claude,
                    s.show_claude_code,
                    tray_icon::TrayIconKind::Claude,
                    s.language.strings().claude_code_model,
                    s.session_percent,
                    s.weekly_percent,
                    s.session_text.as_str(),
                    s.weekly_text.as_str(),
                ),
                (
                    Provider::Codex,
                    s.show_codex,
                    tray_icon::TrayIconKind::Codex,
                    s.language.strings().codex_model,
                    s.codex_session_percent,
                    s.codex_weekly_percent,
                    s.codex_session_text.as_str(),
                    s.codex_weekly_text.as_str(),
                ),
            ] {
                if !enabled {
                    continue;
                }
                let mut lines = Vec::new();
                if limits.shows(provider, Quota::Session) {
                    lines.push(format!("5h: {session_text}"));
                }
                if limits.shows(provider, Quota::Weekly) {
                    lines.push(format!("7d: {weekly_text}"));
                }
                icons.push(tray_icon::TrayIconData {
                    kind,
                    percent: Some(match limits.badge_quota(provider) {
                        Quota::Session => session_percent,
                        Quota::Weekly => weekly_percent,
                    }),
                    tooltip: format!("{name} {}", lines.join(" | ")),
                });
            }
            icons
        }
        Some(s) => {
            let mut icons = Vec::new();
            if s.show_claude_code {
                icons.push(tray_icon::TrayIconData {
                    kind: tray_icon::TrayIconKind::Claude,
                    percent: None,
                    tooltip: s.language.strings().window_title.to_string(),
                });
            }
            if s.show_codex {
                icons.push(tray_icon::TrayIconData {
                    kind: tray_icon::TrayIconKind::Codex,
                    percent: None,
                    tooltip: s.language.strings().codex_window_title.to_string(),
                });
            }
            icons
        }
        None => Vec::new(),
    }
}

fn tray_owner(widget: HWND) -> HWND {
    let controller = RECOVERY_WINDOW.load(Ordering::Relaxed);
    if controller == 0 {
        widget
    } else {
        SendHwnd(controller).to_hwnd()
    }
}

fn sync_tray_icons(hwnd: HWND) {
    let icons = tray_icon_data_from_state();
    tray_icon::sync(tray_owner(hwnd), &icons);
}

fn toggle_widget_visibility(hwnd: HWND) {
    let new_visible = {
        let mut state = lock_state();
        if let Some(s) = state.as_mut() {
            s.widget_visible = !s.widget_visible;
            diagnose::log(format!(
                "user toggled widget visibility to {}",
                s.widget_visible
            ));
            s.widget_visible
        } else {
            return;
        }
    };
    save_state_settings();
    unsafe {
        if new_visible {
            position_at_taskbar();
            render_layered();
        } else {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
    }
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn update_check_interval() -> Duration {
    Duration::from_secs(24 * 60 * 60)
}

fn auto_update_check_due(last_update_check_unix: Option<u64>) -> bool {
    let Some(last_update_check_unix) = last_update_check_unix else {
        return true;
    };

    now_unix_secs().saturating_sub(last_update_check_unix) >= update_check_interval().as_secs()
}

fn schedule_auto_update_check(hwnd: HWND) {
    let delay_ms = {
        let state = lock_state();
        let Some(s) = state.as_ref() else {
            return;
        };

        if auto_update_check_due(s.last_update_check_unix) {
            None
        } else {
            let elapsed = now_unix_secs().saturating_sub(s.last_update_check_unix.unwrap_or(0));
            let remaining_secs = update_check_interval().as_secs().saturating_sub(elapsed);
            Some((remaining_secs.saturating_mul(1000)).min(u32::MAX as u64) as u32)
        }
    };

    unsafe {
        let _ = KillTimer(hwnd, TIMER_UPDATE_CHECK);
        if let Some(delay_ms) = delay_ms {
            SetTimer(hwnd, TIMER_UPDATE_CHECK, delay_ms.max(1), None);
        }
    }
}

fn refresh_usage_texts(state: &mut AppState) {
    if !state.last_poll_ok {
        return;
    }

    let strings = state.language.strings();
    let sp = show_percentages();
    let st = show_reset_timer();
    let dt = show_detailed_remaining();
    let Some(data) = state.data.as_ref() else {
        return;
    };

    if let Some(claude_code) = data.claude_code.as_ref() {
        state.session_text = poller::format_line(&claude_code.session, strings, sp, st, dt);
        state.weekly_text = poller::format_line(&claude_code.weekly, strings, sp, st, dt);
    } else if state.show_claude_code {
        state.session_text = "!".to_string();
        state.weekly_text = "!".to_string();
    }

    if let Some(codex) = data.codex.as_ref() {
        state.codex_session_text = poller::format_line(&codex.session, strings, sp, st, dt);
        state.codex_weekly_text = poller::format_line(&codex.weekly, strings, sp, st, dt);
    } else if state.show_codex {
        state.codex_session_text = "!".to_string();
        state.codex_weekly_text = "!".to_string();
    }
}

fn set_window_title(hwnd: HWND, strings: Strings) {
    unsafe {
        let title = native_interop::wide_str(strings.window_title);
        let _ = SetWindowTextW(hwnd, PCWSTR::from_raw(title.as_ptr()));
    }
}

fn show_info_message(hwnd: HWND, title: &str, message: &str) {
    unsafe {
        let title_wide = native_interop::wide_str(title);
        let message_wide = native_interop::wide_str(message);
        let _ = MessageBoxW(
            hwnd,
            PCWSTR::from_raw(message_wide.as_ptr()),
            PCWSTR::from_raw(title_wide.as_ptr()),
            MB_OK | MB_ICONINFORMATION,
        );
    }
}

fn show_error_message(hwnd: HWND, title: &str, message: &str) {
    unsafe {
        let title_wide = native_interop::wide_str(title);
        let message_wide = native_interop::wide_str(message);
        let _ = MessageBoxW(
            hwnd,
            PCWSTR::from_raw(message_wide.as_ptr()),
            PCWSTR::from_raw(title_wide.as_ptr()),
            MB_OK | MB_ICONERROR,
        );
    }
}

fn show_update_prompt(hwnd: HWND, strings: Strings, release: &ReleaseDescriptor) -> bool {
    let message = strings
        .update_prompt_now
        .replace("{version}", &release.latest_version);

    unsafe {
        let title_wide = native_interop::wide_str(strings.update_available);
        let message_wide = native_interop::wide_str(&message);
        MessageBoxW(
            hwnd,
            PCWSTR::from_raw(message_wide.as_ptr()),
            PCWSTR::from_raw(title_wide.as_ptr()),
            MB_YESNO | MB_ICONQUESTION,
        ) == IDYES
    }
}

fn apply_language_to_state(state: &mut AppState, language_override: Option<LanguageId>) {
    state.language_override = language_override;
    state.language = localization::resolve_language(language_override);
    set_window_title(state.hwnd.to_hwnd(), state.language.strings());
    refresh_usage_texts(state);
}

fn update_language_change() -> bool {
    let mut state = lock_state();
    let Some(app_state) = state.as_mut() else {
        return false;
    };

    if app_state.language_override.is_some() {
        return false;
    }

    let new_language = localization::detect_system_language();
    if new_language == app_state.language {
        return false;
    }

    apply_language_to_state(app_state, None);
    true
}

fn version_action_label(
    strings: Strings,
    language: LanguageId,
    install_channel: InstallChannel,
    status: &UpdateStatus,
) -> String {
    let current = env!("CARGO_PKG_VERSION");
    match status {
        UpdateStatus::Idle => format!("v{current} - {}", strings.check_for_updates),
        UpdateStatus::Checking => format!("v{current} - {}", strings.checking_for_updates),
        UpdateStatus::Applying => format!("v{current} - {}", strings.applying_update),
        UpdateStatus::UpToDate => format!("v{current} - {}", strings.up_to_date_short),
        UpdateStatus::Available(release) => match install_channel {
            InstallChannel::Portable => {
                format!(
                    "v{current} - {} v{}",
                    strings.update_to, release.latest_version
                )
            }
            InstallChannel::Winget => format!(
                "v{current} - {} v{}",
                localization::update_via_winget(language),
                release.latest_version
            ),
        },
    }
}

fn begin_update_check(hwnd: HWND, interactive: bool) {
    let send_hwnd = SendHwnd::from_hwnd(hwnd);
    let (strings, install_channel) = {
        let mut state = lock_state();
        let Some(app_state) = state.as_mut() else {
            return;
        };

        if matches!(
            app_state.update_status,
            UpdateStatus::Checking | UpdateStatus::Applying
        ) {
            if interactive {
                let strings = app_state.language.strings();
                drop(state);
                show_info_message(hwnd, strings.updates, strings.update_in_progress);
            }
            return;
        }

        app_state.update_status = UpdateStatus::Checking;
        (app_state.language.strings(), app_state.install_channel)
    };

    std::thread::spawn(move || {
        let hwnd = send_hwnd.to_hwnd();
        let checked_at = now_unix_secs();
        match updater::check_for_updates() {
            Ok(UpdateCheckResult::UpToDate) => {
                {
                    let mut state = lock_state();
                    if let Some(s) = state.as_mut() {
                        s.update_status = UpdateStatus::UpToDate;
                        s.last_update_check_unix = Some(checked_at);
                    }
                }
                save_state_settings();
                if interactive {
                    show_info_message(hwnd, strings.updates, strings.up_to_date);
                }
                unsafe {
                    let _ = PostMessageW(hwnd, WM_APP_UPDATE_CHECK_COMPLETE, WPARAM(0), LPARAM(0));
                }
            }
            Ok(UpdateCheckResult::Available(release)) => {
                {
                    let mut state = lock_state();
                    if let Some(s) = state.as_mut() {
                        s.update_status = UpdateStatus::Available(release.clone());
                        s.last_update_check_unix = Some(checked_at);
                    }
                }
                save_state_settings();
                if interactive && show_update_prompt(hwnd, strings, &release) {
                    match install_channel {
                        InstallChannel::Portable => begin_update_apply(hwnd, release),
                        InstallChannel::Winget => begin_winget_update(hwnd),
                    }
                }
                unsafe {
                    let _ = PostMessageW(hwnd, WM_APP_UPDATE_CHECK_COMPLETE, WPARAM(0), LPARAM(0));
                }
            }
            Err(error) => {
                {
                    let mut state = lock_state();
                    if let Some(s) = state.as_mut() {
                        s.update_status = UpdateStatus::Idle;
                        s.last_update_check_unix = Some(checked_at);
                    }
                }
                save_state_settings();
                if interactive {
                    let message = format!("{}.\n\n{}", strings.update_failed, error);
                    show_error_message(hwnd, strings.updates, &message);
                }
                unsafe {
                    let _ = PostMessageW(hwnd, WM_APP_UPDATE_CHECK_COMPLETE, WPARAM(0), LPARAM(0));
                }
            }
        }
    });
}

fn begin_update_apply(hwnd: HWND, release: ReleaseDescriptor) {
    let send_hwnd = SendHwnd::from_hwnd(hwnd);
    let strings = {
        let mut state = lock_state();
        let Some(app_state) = state.as_mut() else {
            return;
        };

        if matches!(
            app_state.update_status,
            UpdateStatus::Checking | UpdateStatus::Applying
        ) {
            let strings = app_state.language.strings();
            drop(state);
            show_info_message(hwnd, strings.updates, strings.update_in_progress);
            return;
        }

        app_state.update_status = UpdateStatus::Applying;
        app_state.language.strings()
    };

    std::thread::spawn(move || {
        let hwnd = send_hwnd.to_hwnd();
        match updater::begin_self_update(&release) {
            Ok(()) => unsafe {
                let _ = PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
            },
            Err(error) => {
                {
                    let mut state = lock_state();
                    if let Some(s) = state.as_mut() {
                        s.update_status = UpdateStatus::Available(release);
                    }
                }
                let message = format!("{}.\n\n{}", strings.update_failed, error);
                show_error_message(hwnd, strings.updates, &message);
                unsafe {
                    let _ = PostMessageW(hwnd, WM_APP_UPDATE_CHECK_COMPLETE, WPARAM(0), LPARAM(0));
                }
            }
        }
    });
}

fn begin_winget_update(hwnd: HWND) {
    let strings = {
        let state = lock_state();
        state.as_ref().map(|s| s.language.strings())
    }
    .unwrap_or(LanguageId::English.strings());

    match updater::begin_winget_update() {
        Ok(()) => unsafe {
            let _ = PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
        },
        Err(error) => {
            let message = format!("{}.\n\n{}", strings.update_failed, error);
            show_error_message(hwnd, strings.updates, &message);
        }
    }
}

const STARTUP_REGISTRY_PATH: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const STARTUP_REGISTRY_KEY: &str = "ClaudeCodexUsageMonitor";

/// Returns true only if the startup registry value points to this executable.
fn is_startup_enabled() -> bool {
    unsafe {
        let path = native_interop::wide_str(STARTUP_REGISTRY_PATH);
        let key_name = native_interop::wide_str(STARTUP_REGISTRY_KEY);

        let mut hkey = HKEY::default();
        let result = RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR::from_raw(path.as_ptr()),
            0,
            KEY_READ,
            &mut hkey,
        );
        if result.is_err() {
            return false;
        }

        // Query the size of the value
        let mut data_size: u32 = 0;
        let result = RegQueryValueExW(
            hkey,
            PCWSTR::from_raw(key_name.as_ptr()),
            None,
            None,
            None,
            Some(&mut data_size),
        );
        if result.is_err() || data_size == 0 {
            let _ = RegCloseKey(hkey);
            return false;
        }

        // Read the value
        let mut buf = vec![0u8; data_size as usize];
        let result = RegQueryValueExW(
            hkey,
            PCWSTR::from_raw(key_name.as_ptr()),
            None,
            None,
            Some(buf.as_mut_ptr()),
            Some(&mut data_size),
        );
        let _ = RegCloseKey(hkey);
        if result.is_err() {
            return false;
        }

        // Convert the registry value (UTF-16) to a string
        let wide_slice =
            std::slice::from_raw_parts(buf.as_ptr() as *const u16, data_size as usize / 2);
        let reg_value = String::from_utf16_lossy(wide_slice)
            .trim_end_matches('\0')
            .to_string();

        // Get the current executable path
        let mut exe_buf = [0u16; 260];
        let len = GetModuleFileNameW(None, &mut exe_buf) as usize;
        if len == 0 {
            return false;
        }
        let current_exe = String::from_utf16_lossy(&exe_buf[..len]);

        // Case-insensitive comparison (Windows paths are case-insensitive)
        reg_value.eq_ignore_ascii_case(&current_exe)
    }
}

fn set_startup_enabled(enable: bool) {
    unsafe {
        let path = native_interop::wide_str(STARTUP_REGISTRY_PATH);

        let mut hkey = HKEY::default();
        let result = RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR::from_raw(path.as_ptr()),
            0,
            KEY_SET_VALUE,
            &mut hkey,
        );
        if result.is_err() {
            return;
        }

        let key_name = native_interop::wide_str(STARTUP_REGISTRY_KEY);

        if enable {
            let mut exe_buf = [0u16; 260];
            let len = GetModuleFileNameW(None, &mut exe_buf) as usize;
            if len > 0 {
                // Write the wide string including null terminator
                let byte_len = ((len + 1) * 2) as u32;
                let _ = RegSetValueExW(
                    hkey,
                    PCWSTR::from_raw(key_name.as_ptr()),
                    0,
                    REG_SZ,
                    Some(std::slice::from_raw_parts(
                        exe_buf.as_ptr() as *const u8,
                        byte_len as usize,
                    )),
                );
            }
        } else {
            let _ = RegDeleteValueW(hkey, PCWSTR::from_raw(key_name.as_ptr()));
        }

        let _ = RegCloseKey(hkey);
    }
}

// Dimensions matching the C# version
const SEGMENT_W: i32 = 10;
const SEGMENT_H: i32 = 13;
const SEGMENT_GAP: i32 = 1;
const CORNER_RADIUS: i32 = 2;

const LEFT_DIVIDER_W: i32 = 3;
const DIVIDER_RIGHT_MARGIN: i32 = 10;
const LABEL_WIDTH: i32 = 18;
const LABEL_RIGHT_MARGIN: i32 = 10;
const BAR_RIGHT_MARGIN: i32 = 4;
const TEXT_WIDTH: i32 = 62;
const MODEL_RIGHT_MARGIN: i32 = 5;
const RIGHT_MARGIN: i32 = 1;
const WIDGET_HEIGHT: i32 = 46;

fn quota_label_width(stacked: bool) -> i32 {
    if stacked {
        90
    } else {
        LABEL_WIDTH
    }
}

fn total_widget_width_for(show_claude: bool, show_codex: bool, limits: DisplayLimits) -> i32 {
    let active_models = limits.columns(show_claude, show_codex);
    let stacked = limits.stacked(show_claude, show_codex);
    let label_part = if show_labels() {
        sc(quota_label_width(stacked)) + sc(LABEL_RIGHT_MARGIN)
    } else {
        0
    };
    let model_width = model_usage_width(current_segment_count());

    sc(LEFT_DIVIDER_W)
        + sc(DIVIDER_RIGHT_MARGIN)
        + label_part
        + model_width * active_models
        + sc(MODEL_RIGHT_MARGIN) * (active_models - 1)
        + sc(RIGHT_MARGIN)
}

fn effective_display_limits(state: &AppState) -> DisplayLimits {
    let available = state
        .data
        .as_ref()
        .and_then(|data| data.codex.as_ref())
        .and_then(|usage| usage.available_windows);
    state.display_limits.with_codex_windows(available)
}

fn total_widget_width_for_state(state: &AppState) -> i32 {
    total_widget_width_for(
        state.show_claude_code,
        state.show_codex,
        effective_display_limits(state),
    )
}

fn total_widget_width() -> i32 {
    let state = lock_state();
    state
        .as_ref()
        .map(total_widget_width_for_state)
        .unwrap_or_else(|| total_widget_width_for(true, false, DisplayLimits::default()))
}

fn claude_accent_color() -> Color {
    Color::from_hex("#D97757")
}

/// Color-code a usage percentage as green -> yellow -> orange -> red. Used by
/// the non-default bar themes. Slightly brighter tones in dark mode.
fn usage_threshold_color(percent: f64, is_dark: bool) -> Color {
    let p = percent.clamp(0.0, 100.0);
    if is_dark {
        if p >= 95.0 {
            Color::from_hex("#FF5C5C")
        } else if p >= 90.0 {
            Color::from_hex("#FF8A4C")
        } else if p >= 80.0 {
            Color::from_hex("#F2C14E")
        } else {
            Color::from_hex("#4CC76B")
        }
    } else if p >= 95.0 {
        Color::from_hex("#D32F2F")
    } else if p >= 90.0 {
        Color::from_hex("#E8731C")
    } else if p >= 80.0 {
        Color::from_hex("#C9A21E")
    } else {
        Color::from_hex("#2E9E4F")
    }
}

fn codex_accent_color(is_dark: bool) -> Color {
    if is_dark {
        Color::from_hex("#F5F5F5")
    } else {
        Color::from_hex("#1F1F1F")
    }
}

/// GUI apps (windows subsystem, panic=abort) show no console and no log when
/// they panic. Record the panic message + location to a crash log so failures
/// are diagnosable.
fn install_panic_logger() {
    std::panic::set_hook(Box::new(|info| {
        let path = std::env::temp_dir().join("claude-codex-usage-monitor-crash.log");
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            use std::io::Write;
            let _ = writeln!(f, "{info}");
        }
        diagnose::log(format!("PANIC: {info}"));
    }));
}

pub fn run() {
    install_panic_logger();

    // Enable Per-Monitor DPI Awareness V2 for crisp rendering at any scale factor
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        CURRENT_DPI.store(GetDpiForSystem(), Ordering::Relaxed);
    }
    diagnose::log("window::run started");

    // Single-instance guard: silently exit if another instance is running
    let mutex_name = native_interop::wide_str("Global\\ClaudeCodexUsageMonitor");
    let _mutex = unsafe {
        let handle = CreateMutexW(None, false, PCWSTR::from_raw(mutex_name.as_ptr()));
        match handle {
            Ok(h) => {
                if GetLastError() == ERROR_ALREADY_EXISTS {
                    diagnose::log("startup aborted: another instance is already running");
                    return;
                }
                h
            }
            Err(error) => {
                diagnose::log_error(
                    "startup aborted: unable to create single-instance mutex",
                    error,
                );
                return;
            }
        }
    };

    let class_name = native_interop::wide_str("ClaudeCodexUsageMonitor");

    unsafe {
        let hinstance = GetModuleHandleW(PCWSTR::null()).unwrap();
        let (large_icon, small_icon) = load_embedded_app_icons();

        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wnd_proc),
            hInstance: HINSTANCE(hinstance.0),
            hIcon: large_icon,
            hIconSm: small_icon,
            hCursor: LoadCursorW(HINSTANCE::default(), IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH(std::ptr::null_mut()),
            lpszClassName: PCWSTR::from_raw(class_name.as_ptr()),
            ..Default::default()
        };

        let atom = RegisterClassExW(&wc);
        if atom == 0 {
            diagnose::log("RegisterClassExW returned 0");
        }

        let mut settings = load_settings();
        if std::env::args().any(|arg| arg == "--show-widget") {
            settings.widget_visible = true;
        }
        let initial_bar_theme = settings
            .bar_theme
            .as_deref()
            .and_then(BarTheme::from_code)
            .unwrap_or(BarTheme::Segmented);
        CURRENT_BAR_THEME.store(initial_bar_theme.to_u8(), Ordering::Relaxed);
        CURRENT_SEGMENT_COUNT.store(settings.segment_count, Ordering::Relaxed);
        SHOW_LABELS.store(settings.show_labels, Ordering::Relaxed);
        SHOW_PERCENTAGES.store(settings.show_percentages, Ordering::Relaxed);
        SHOW_RESET_TIMER.store(settings.show_reset_timer, Ordering::Relaxed);
        SHOW_DETAILED_REMAINING.store(settings.show_detailed_remaining, Ordering::Relaxed);
        let initial_pace = settings
            .pace_indicator_style
            .as_deref()
            .map(PaceStyle::from_code)
            .unwrap_or(PaceStyle::Off);
        PACE_STYLE.store(initial_pace.to_u8(), Ordering::Relaxed);
        let language_override = settings.language.as_deref().and_then(LanguageId::from_code);
        let language = localization::resolve_language(language_override);
        let install_channel = updater::current_install_channel();

        // Create as layered popup (will be reparented into taskbar)
        let title = native_interop::wide_str(language.strings().window_title);
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_NOACTIVATE,
            PCWSTR::from_raw(class_name.as_ptr()),
            PCWSTR::from_raw(title.as_ptr()),
            WS_POPUP,
            0,
            0,
            total_widget_width_for(
                settings.show_claude_code,
                settings.show_codex,
                settings.display_limits,
            ),
            sc(WIDGET_HEIGHT),
            HWND::default(),
            HMENU::default(),
            hinstance,
            None,
        )
        .unwrap();

        if !large_icon.is_invalid() {
            let _ = SendMessageW(
                hwnd,
                WM_SETICON,
                WPARAM(ICON_BIG as usize),
                LPARAM(large_icon.0 as isize),
            );
        }
        if !small_icon.is_invalid() {
            let _ = SendMessageW(
                hwnd,
                WM_SETICON,
                WPARAM(ICON_SMALL as usize),
                LPARAM(small_icon.0 as isize),
            );
        }

        diagnose::log(format!("main window created hwnd={:?}", hwnd));

        let is_dark = theme::is_dark_mode();

        {
            let mut state = lock_state();
            *state = Some(AppState {
                hwnd: SendHwnd::from_hwnd(hwnd),
                taskbar_hwnd: None,
                tray_notify_hwnd: None,
                win_event_hook: None,
                overlay_event_hooks: Vec::new(),
                is_dark,
                language_override,
                language,
                install_channel,
                session_percent: 0.0,
                session_text: "--".to_string(),
                weekly_percent: 0.0,
                weekly_text: "--".to_string(),
                codex_session_percent: 0.0,
                codex_session_text: "--".to_string(),
                codex_weekly_percent: 0.0,
                codex_weekly_text: "--".to_string(),
                show_claude_code: settings.show_claude_code,
                show_codex: settings.show_codex,
                display_limits: settings.display_limits,
                data: None,
                poll_interval_ms: settings.poll_interval_ms,
                retry_count: 0,
                force_notify_auth_error: false,
                auth_error_paused_polling: false,
                auth_watch_mode: poller::CredentialWatchMode::ActiveSource,
                auth_watch_snapshot: Vec::new(),
                last_poll_ok: false,
                update_status: UpdateStatus::Idle,
                last_update_check_unix: settings.last_update_check_unix,
                tray_offset: settings.tray_offset,
                dragging: false,
                drag_start_mouse_x: 0,
                drag_start_offset: 0,
                widget_visible: settings.widget_visible,
                shutting_down: false,
            });
        }

        if std::env::args().any(|arg| arg == "--show-widget") {
            save_state_settings();
        }
        // Anchor an independent layered popup to the taskbar.
        if let Some(taskbar_hwnd) = native_interop::find_taskbar() {
            diagnose::log(format!("taskbar found hwnd={:?}", taskbar_hwnd));
            let _ = native_interop::prepare_taskbar_overlay(hwnd);

            let mut state = lock_state();
            let s = state.as_mut().unwrap();
            s.taskbar_hwnd = Some(taskbar_hwnd);

            let tray_notify = native_interop::find_child_window(taskbar_hwnd, "TrayNotifyWnd");
            s.tray_notify_hwnd = tray_notify;
            if tray_notify.is_some() {
                diagnose::log("TrayNotifyWnd found");
            } else {
                diagnose::log("TrayNotifyWnd not found");
            }

            if let Some(tray_hwnd) = tray_notify {
                let thread_id = native_interop::get_window_thread_id(tray_hwnd);
                let hook = native_interop::set_tray_event_hook(thread_id, on_tray_location_changed);
                s.win_event_hook = hook;
                if hook.is_some() {
                    diagnose::log("tray event hook installed");
                } else {
                    diagnose::log("tray event hook could not be installed");
                }
            }
        } else {
            diagnose::log("taskbar not found; using fallback popup window");
        }

        // Register system tray icon(s)
        sync_tray_icons(hwnd);

        // Position and show (only if widget_visible preference is true)
        position_at_taskbar();
        diagnose::log("window shown");

        // All widget rendering uses the same independent layered surface.
        render_layered();

        // Initialize the pixel character window (floats above the widget).
        let character_kind = settings
            .character_kind
            .as_deref()
            .and_then(CharacterKind::from_code)
            .unwrap_or(CharacterKind::Cat);
        if let Some(anchor) = native_interop::get_window_rect_safe(hwnd) {
            character::init(
                anchor,
                settings.characters_enabled,
                character_kind,
                settings.cat_variant,
                settings.dog_variant,
                language,
            );
        }

        // Poll timer: 15 minutes
        let initial_poll_ms = {
            let state = lock_state();
            state
                .as_ref()
                .map(|s| s.poll_interval_ms)
                .unwrap_or(POLL_15_MIN)
        };
        SetTimer(hwnd, TIMER_POLL, initial_poll_ms, None);

        // Initial poll
        let send_hwnd = SendHwnd::from_hwnd(hwnd);
        std::thread::spawn(move || {
            diagnose::log("initial poll thread started");
            do_poll(send_hwnd);
        });

        schedule_auto_update_check(hwnd);
        let should_check_updates = {
            let state = lock_state();
            state
                .as_ref()
                .map(|s| auto_update_check_due(s.last_update_check_unix))
                .unwrap_or(false)
        };
        if should_check_updates {
            begin_update_check(hwnd, false);
        }

        // Initial theme check
        check_theme_change();

        let recovery_window = create_recovery_window();
        if let Some(controller) = recovery_window {
            // Tray callbacks must survive destruction/replacement of the widget.
            tray_icon::remove_all(hwnd);
            RECOVERY_WINDOW.store(controller.0 as isize, Ordering::Relaxed);
            let hooks = native_interop::set_overlay_event_hooks(on_overlay_event);
            diagnose::log(format!(
                "overlay event hooks installed count={}",
                hooks.len()
            ));
            if let Some(s) = lock_state().as_mut() {
                s.overlay_event_hooks = hooks;
            }
            sync_tray_icons(hwnd);
        } else {
            diagnose::log("unable to create taskbar recovery controller");
        }

        // Message loop
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, HWND::default(), 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// A hidden top-level window survives Explorer and receives broadcasts that
/// taskbar child windows do not receive (message-only windows would miss them).
fn create_recovery_window() -> Option<HWND> {
    unsafe {
        let instance = GetModuleHandleW(PCWSTR::null()).ok()?;
        let class = native_interop::wide_str("ClaudeCodexUsageMonitorRecovery");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(recovery_wnd_proc),
            hInstance: HINSTANCE(instance.0),
            lpszClassName: PCWSTR::from_raw(class.as_ptr()),
            ..Default::default()
        };
        RegisterClassW(&wc);
        let taskbar_message = native_interop::wide_str("TaskbarCreated");
        TASKBAR_CREATED_MSG.store(
            RegisterWindowMessageW(PCWSTR::from_raw(taskbar_message.as_ptr())),
            Ordering::Relaxed,
        );
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            PCWSTR::from_raw(class.as_ptr()),
            PCWSTR::null(),
            WS_POPUP,
            0,
            0,
            0,
            0,
            HWND::default(),
            HMENU::default(),
            instance,
            None,
        )
        .ok()?;
        if let Err(error) = WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION) {
            diagnose::log_error("session notification registration failed", error);
        }
        if SetTimer(hwnd, TIMER_RECOVERY, 2_000, None) == 0 {
            diagnose::log("recovery watchdog timer could not be started");
        }
        Some(hwnd)
    }
}

unsafe extern "system" fn recovery_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let taskbar_created = TASKBAR_CREATED_MSG.load(Ordering::Relaxed);
    if taskbar_created != 0 && msg == taskbar_created {
        diagnose::log("TaskbarCreated: rebuilding taskbar attachment");
        RECREATE_WIDGET.store(true, Ordering::Relaxed);
        recover_taskbar(true);
        return LRESULT(0);
    }
    match msg {
        WM_APP_OVERLAY_REFRESH => {
            OVERLAY_REFRESH_PENDING.store(false, Ordering::Relaxed);
            position_at_taskbar();
            // Explorer can finish raising the taskbar after its foreground event.
            // Repeat once after that transition, without waiting for the watchdog.
            SetTimer(hwnd, TIMER_OVERLAY_SETTLE, 100, None);
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_OVERLAY_SETTLE => {
            let _ = KillTimer(hwnd, TIMER_OVERLAY_SETTLE);
            position_at_taskbar();
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_RECOVERY => {
            recover_taskbar(false);
            // A valid HWND does not prove that DWM retained the layered surface
            // after sleep. Refresh even when the attachment itself is healthy.
            position_at_taskbar();
            render_layered();
            LRESULT(0)
        }
        WM_POWERBROADCAST => {
            // PBT_APMRESUMECRITICAL, PBT_APMRESUMESUSPEND, PBT_APMRESUMEAUTOMATIC.
            if matches!(wparam.0, 0x6 | 0x7 | 0x12) {
                diagnose::log("power resume: queued widget recreation");
                // Defer until the watchdog: Explorer/DWM may still be restoring.
                RECREATE_WIDGET.store(true, Ordering::Relaxed);
            }
            LRESULT(1)
        }
        WM_WTSSESSION_CHANGE if wparam.0 == 8 => {
            // WTS_SESSION_UNLOCK: covers hibernation returning via the sign-in screen.
            RECREATE_WIDGET.store(true, Ordering::Relaxed);
            LRESULT(0)
        }
        _ if msg == WM_APP_TRAY => {
            recover_taskbar(false);
            let widget = {
                let state = lock_state();
                state.as_ref().map(|s| s.hwnd.to_hwnd())
            };
            if let Some(widget) = widget {
                if IsWindow(widget).as_bool() {
                    let _ = SendMessageW(widget, WM_APP_TRAY, wparam, lparam);
                }
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            RECOVERY_WINDOW.store(0, Ordering::Relaxed);
            let hooks = lock_state()
                .as_mut()
                .map(|s| std::mem::take(&mut s.overlay_event_hooks))
                .unwrap_or_default();
            for hook in hooks {
                native_interop::unhook_win_event(hook);
            }
            let _ = WTSUnRegisterSessionNotification(hwnd);
            tray_icon::remove_all(hwnd);
            LRESULT(0)
        }
        WM_DISPLAYCHANGE | WM_SETTINGCHANGE | WM_THEMECHANGED | WM_DWMCOMPOSITIONCHANGED => {
            recover_taskbar(true);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn recover_taskbar(force: bool) {
    unsafe {
        let Some(taskbar) = native_interop::find_taskbar() else {
            return;
        };
        let tray = native_interop::find_child_window(taskbar, "TrayNotifyWnd");
        let (mut hwnd, old_taskbar, old_tray, closing, title, interval) = {
            let state = lock_state();
            let Some(s) = state.as_ref() else { return };
            (
                s.hwnd.to_hwnd(),
                s.taskbar_hwnd,
                s.tray_notify_hwnd,
                s.shutting_down,
                s.language.strings().window_title,
                s.poll_interval_ms,
            )
        };
        if closing {
            return;
        }
        let alive = IsWindow(hwnd).as_bool();
        let requested = RECREATE_WIDGET.load(Ordering::Relaxed);
        if !force
            && !requested
            && alive
            && old_taskbar == Some(taskbar)
            && old_tray == tray
            && GetParent(hwnd).is_err()
            && GetWindowLongW(hwnd, GWL_STYLE) as u32 & WS_CHILD.0 == 0
        {
            return;
        }
        let recreated = !alive || requested;
        if recreated {
            let old_widget = hwnd;
            let class = native_interop::wide_str("ClaudeCodexUsageMonitor");
            let title = native_interop::wide_str(title);
            let Ok(instance) = GetModuleHandleW(PCWSTR::null()) else {
                return;
            };
            hwnd = match CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_NOACTIVATE,
                PCWSTR::from_raw(class.as_ptr()),
                PCWSTR::from_raw(title.as_ptr()),
                WS_POPUP,
                0,
                0,
                total_widget_width(),
                sc(WIDGET_HEIGHT),
                HWND::default(),
                HMENU::default(),
                instance,
                None,
            ) {
                Ok(hwnd) => hwnd,
                Err(error) => {
                    diagnose::log_error("recreating widget failed", error);
                    return;
                }
            };
            {
                let mut state = lock_state();
                if let Some(s) = state.as_mut() {
                    s.hwnd = SendHwnd::from_hwnd(hwnd);
                    s.dragging = false;
                }
            }
            RECREATE_WIDGET.store(false, Ordering::Relaxed);
            if alive {
                // Replacement is already current; the old window's WM_DESTROY
                // must not clear state or remove the controller-owned tray entry.
                let _ = DestroyWindow(old_widget);
            }
        }
        // Reset the layered surface after display/compositor changes.
        let style = GetWindowLongW(hwnd, GWL_EXSTYLE);
        SetWindowLongW(hwnd, GWL_EXSTYLE, style & !(WS_EX_LAYERED.0 as i32));
        SetWindowLongW(hwnd, GWL_EXSTYLE, style | WS_EX_LAYERED.0 as i32);
        let attached = native_interop::prepare_taskbar_overlay(hwnd);
        let old_hook = {
            let mut state = lock_state();
            let Some(s) = state.as_mut() else { return };
            s.taskbar_hwnd = Some(taskbar);
            s.tray_notify_hwnd = tray;
            s.is_dark = theme::is_dark_mode();
            s.win_event_hook.take()
        };
        if let Some(hook) = old_hook {
            native_interop::unhook_win_event(hook);
        }
        let hook = tray.and_then(|tray| {
            native_interop::set_tray_event_hook(
                native_interop::get_window_thread_id(tray),
                on_tray_location_changed,
            )
        });
        {
            let mut state = lock_state();
            if let Some(s) = state.as_mut() {
                s.win_event_hook = hook;
            }
        }
        if !attached {
            diagnose::log("overlay preparation failed; recovery will retry");
        }
        position_at_taskbar();
        render_layered();
        sync_tray_icons(hwnd);
        if recreated {
            SetTimer(hwnd, TIMER_POLL, interval, None);
            schedule_countdown_timer();
            schedule_auto_update_check(hwnd);
            let send_hwnd = SendHwnd::from_hwnd(hwnd);
            std::thread::spawn(move || do_poll(send_hwnd));
        }
        diagnose::log(format!(
            "taskbar overlay restored prepared={attached} recreated={recreated}"
        ));
    }
}

/// Render widget content and push to the layered window via UpdateLayeredWindow.
/// Recover grayscale coverage so pure black text blends directly onto the taskbar,
/// without a background panel, colored text, outlines, or shadows.
fn render_layered() {
    refresh_dpi();
    let (
        hwnd_val,
        is_dark,
        strings,
        session_pct,
        session_text,
        weekly_pct,
        weekly_text,
        codex_session_pct,
        codex_session_text,
        codex_weekly_pct,
        codex_weekly_text,
        show_claude_code,
        show_codex,
        display_limits,
        cc_session_pace,
        cc_weekly_pace,
        cx_session_pace,
        cx_weekly_pace,
    ) = {
        let state = lock_state();
        match state.as_ref() {
            Some(s) => {
                let cc = s.data.as_ref().and_then(|d| d.claude_code.as_ref());
                let cx = s.data.as_ref().and_then(|d| d.codex.as_ref());
                (
                    s.hwnd,
                    s.is_dark,
                    s.language.strings(),
                    s.session_percent,
                    s.session_text.clone(),
                    s.weekly_percent,
                    s.weekly_text.clone(),
                    s.codex_session_percent,
                    s.codex_session_text.clone(),
                    s.codex_weekly_percent,
                    s.codex_weekly_text.clone(),
                    s.show_claude_code,
                    s.show_codex,
                    effective_display_limits(s),
                    pace_expected(
                        cc.and_then(|u| u.session.resets_at),
                        PACE_SESSION_WINDOW_SECS,
                    ),
                    pace_expected(cc.and_then(|u| u.weekly.resets_at), PACE_WEEKLY_WINDOW_SECS),
                    pace_expected(
                        cx.and_then(|u| u.session.resets_at),
                        PACE_SESSION_WINDOW_SECS,
                    ),
                    pace_expected(cx.and_then(|u| u.weekly.resets_at), PACE_WEEKLY_WINDOW_SECS),
                )
            }
            None => return,
        }
    };

    let hwnd = hwnd_val.to_hwnd();

    let width = total_widget_width();
    let height = sc(WIDGET_HEIGHT);

    let accent = claude_accent_color();
    let codex_accent = codex_accent_color(is_dark);
    let track = if is_dark {
        Color::from_hex("#444444")
    } else {
        Color::from_hex("#AAAAAA")
    };
    let text_color = Color::new(0, 0, 0);
    unsafe {
        let screen_dc = GetDC(hwnd);

        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: 0, // BI_RGB
                ..Default::default()
            },
            ..Default::default()
        };

        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let mem_dc = CreateCompatibleDC(screen_dc);
        let dib =
            CreateDIBSection(mem_dc, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).unwrap_or_default();

        if dib.is_invalid() || bits.is_null() {
            // A valid bitmap with a null bits pointer shouldn't happen, but if it
            // does, still free the bitmap so we don't leak it.
            if !dib.is_invalid() {
                let _ = DeleteObject(dib);
            }
            let _ = DeleteDC(mem_dc);
            ReleaseDC(hwnd, screen_dc);
            return;
        }

        let old_bmp = SelectObject(mem_dc, dib);
        let pixel_count = (width * height) as usize;

        let mut black_pixels = Vec::with_capacity(pixel_count);
        for background in [Color::new(0, 0, 0), Color::new(255, 255, 255)] {
            paint_content(
                mem_dc,
                width,
                height,
                is_dark,
                &background,
                &text_color,
                &accent,
                &track,
                strings,
                session_pct,
                &session_text,
                weekly_pct,
                &weekly_text,
                codex_session_pct,
                &codex_session_text,
                codex_weekly_pct,
                &codex_weekly_text,
                show_claude_code,
                show_codex,
                display_limits,
                &codex_accent,
                cc_session_pace,
                cc_weekly_pace,
                cx_session_pace,
                cx_weekly_pace,
            );
            let _ = GdiFlush();
            let pixels = std::slice::from_raw_parts_mut(bits as *mut u32, pixel_count);
            if black_pixels.is_empty() {
                black_pixels.extend_from_slice(pixels);
            } else {
                for (pixel, black) in pixels.iter_mut().zip(&black_pixels) {
                    *pixel = crate::widget_pixels::transparent_pixel(*black, *pixel);
                }
            }
        }

        // Push to window via UpdateLayeredWindow
        let pt_src = POINT { x: 0, y: 0 };
        let sz = SIZE {
            cx: width,
            cy: height,
        };
        let blend = BLENDFUNCTION {
            BlendOp: 0, // AC_SRC_OVER
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: 1, // AC_SRC_ALPHA
        };

        if let Err(error) = UpdateLayeredWindow(
            hwnd,
            screen_dc,
            None,
            Some(&sz),
            mem_dc,
            Some(&pt_src),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        ) {
            diagnose::log_error(
                "UpdateLayeredWindow failed; queued surface recreation",
                error,
            );
            RECREATE_WIDGET.store(true, Ordering::Relaxed);
        }

        // Cleanup
        SelectObject(mem_dc, old_bmp);
        let _ = DeleteObject(dib);
        let _ = DeleteDC(mem_dc);
        ReleaseDC(hwnd, screen_dc);
    }
}

/// Paint all widget content onto a DC with a given background color.
fn paint_content(
    hdc: HDC,
    width: i32,
    height: i32,
    is_dark: bool,
    bg: &Color,
    text_color: &Color,
    accent: &Color,
    track: &Color,
    strings: Strings,
    session_pct: f64,
    session_text: &str,
    weekly_pct: f64,
    weekly_text: &str,
    codex_session_pct: f64,
    codex_session_text: &str,
    codex_weekly_pct: f64,
    codex_weekly_text: &str,
    show_claude_code: bool,
    show_codex: bool,
    display_limits: DisplayLimits,
    codex_accent: &Color,
    cc_session_pace: Option<f64>,
    cc_weekly_pace: Option<f64>,
    cx_session_pace: Option<f64>,
    cx_weekly_pace: Option<f64>,
) {
    unsafe {
        let client_rect = RECT {
            left: 0,
            top: 0,
            right: width,
            bottom: height,
        };

        let bg_brush = CreateSolidBrush(COLORREF(bg.to_colorref()));
        FillRect(hdc, &client_rect, bg_brush);
        let _ = DeleteObject(bg_brush);

        // Left divider
        let divider_h = sc(25);
        let divider_top = (height - divider_h) / 2;
        let divider_bottom = divider_top + divider_h;

        let (div_left, div_right) = if is_dark {
            ((80, 80, 80), (40, 40, 40))
        } else {
            ((160, 160, 160), (230, 230, 230))
        };

        let left_brush = CreateSolidBrush(COLORREF(native_interop::colorref(
            div_left.0, div_left.1, div_left.2,
        )));
        let left_rect = RECT {
            left: 0,
            top: divider_top,
            right: sc(2),
            bottom: divider_bottom,
        };
        FillRect(hdc, &left_rect, left_brush);
        let _ = DeleteObject(left_brush);

        let right_brush = CreateSolidBrush(COLORREF(native_interop::colorref(
            div_right.0,
            div_right.1,
            div_right.2,
        )));
        let right_rect = RECT {
            left: sc(2),
            top: divider_top,
            right: sc(3),
            bottom: divider_bottom,
        };
        FillRect(hdc, &right_rect, right_brush);
        let _ = DeleteObject(right_brush);

        let content_x = sc(LEFT_DIVIDER_W) + sc(DIVIDER_RIGHT_MARGIN);
        let row2_y = height - sc(5) - sc(SEGMENT_H);
        let row1_y = row2_y - sc(10) - sc(SEGMENT_H);

        let _ = SetBkMode(hdc, TRANSPARENT);
        let _ = SetTextColor(hdc, COLORREF(text_color.to_colorref()));

        let font_name = native_interop::wide_str("Segoe UI");
        let font = CreateFontW(
            sc(-12),
            0,
            0,
            0,
            FW_NORMAL.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET.0 as u32,
            OUT_TT_PRECIS.0 as u32,
            CLIP_DEFAULT_PRECIS.0 as u32,
            ANTIALIASED_QUALITY.0 as u32,
            (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
            PCWSTR::from_raw(font_name.as_ptr()),
        );
        let old_font = SelectObject(hdc, font);

        let stacked = display_limits.stacked(show_claude_code, show_codex);
        let slots = display_limits.slots(show_claude_code, show_codex);
        let label_width = sc(quota_label_width(stacked));
        let label_part = if show_labels() {
            label_width + sc(LABEL_RIGHT_MARGIN)
        } else {
            0
        };
        let segment_count = current_segment_count();
        let column_width = model_usage_width(segment_count) + sc(MODEL_RIGHT_MARGIN);
        let mut labeled_rows = [false; 2];
        for slot in &slots {
            let y = if slots.len() == 1 {
                (height - sc(SEGMENT_H)) / 2
            } else if slot.row == 0 {
                row1_y
            } else {
                row2_y
            };
            let period = match slot.quota {
                Quota::Session => strings.session_window,
                Quota::Weekly => strings.weekly_window,
            };
            if show_labels() && !labeled_rows[slot.row] {
                let label = if stacked {
                    format!(
                        "{} {}",
                        match slot.provider {
                            Provider::Claude => "Claude",
                            Provider::Codex => "Codex",
                        },
                        period
                    )
                } else {
                    period.to_string()
                };
                let _ = SetTextColor(hdc, COLORREF(text_color.to_colorref()));
                let mut label_wide: Vec<u16> = label.encode_utf16().collect();
                let mut rect = RECT {
                    left: content_x,
                    top: y,
                    right: content_x + label_width,
                    bottom: y + sc(SEGMENT_H),
                };
                draw_widget_text(hdc, &mut label_wide, &mut rect, text_color);
                labeled_rows[slot.row] = true;
            }
            let (percent, text, pace) = match (slot.provider, slot.quota) {
                (Provider::Claude, Quota::Session) => (session_pct, session_text, cc_session_pace),
                (Provider::Claude, Quota::Weekly) => (weekly_pct, weekly_text, cc_weekly_pace),
                (Provider::Codex, Quota::Session) => {
                    (codex_session_pct, codex_session_text, cx_session_pace)
                }
                (Provider::Codex, Quota::Weekly) => {
                    (codex_weekly_pct, codex_weekly_text, cx_weekly_pace)
                }
            };
            let provider_accent = match slot.provider {
                Provider::Claude => accent,
                Provider::Codex => codex_accent,
            };
            draw_usage_bar(
                hdc,
                content_x + label_part + slot.column as i32 * column_width,
                y,
                segment_count,
                percent,
                text,
                provider_accent,
                track,
                text_color,
                is_dark,
                pace,
            );
        }

        SelectObject(hdc, old_font);
        let _ = DeleteObject(font);
    }
}

fn do_poll(send_hwnd: SendHwnd) {
    let hwnd = send_hwnd.to_hwnd();
    let (show_claude_code, show_codex) = {
        let state = lock_state();
        state
            .as_ref()
            .map(|s| (s.show_claude_code, s.show_codex))
            .unwrap_or((true, false))
    };

    match poller::poll(show_claude_code, show_codex) {
        Ok(data) => {
            let mut state = lock_state();
            if let Some(s) = state.as_mut() {
                if let Some(claude_code) = data.claude_code.as_ref() {
                    s.session_percent = claude_code.session.percentage;
                    s.weekly_percent = claude_code.weekly.percentage;
                } else if s.show_claude_code {
                    s.session_percent = 0.0;
                    s.weekly_percent = 0.0;
                }
                if let Some(codex) = data.codex.as_ref() {
                    s.codex_session_percent = codex.session.percentage;
                    s.codex_weekly_percent = codex.weekly.percentage;
                } else if s.show_codex {
                    s.codex_session_percent = 0.0;
                    s.codex_weekly_percent = 0.0;
                }
                // Stop fast-poll if reset data is now fresh
                if !poller::app_is_past_reset(&data) {
                    unsafe {
                        let _ = KillTimer(hwnd, TIMER_RESET_POLL);
                    }
                }

                s.data = Some(data);
                s.last_poll_ok = true;
                refresh_usage_texts(s);

                // Recovered from errors — restore normal poll interval
                if s.retry_count > 0 {
                    s.retry_count = 0;
                    let interval = s.poll_interval_ms;
                    unsafe {
                        SetTimer(hwnd, TIMER_POLL, interval, None);
                    }
                }
                s.force_notify_auth_error = false;
                s.auth_error_paused_polling = false;
                s.auth_watch_mode = poller::CredentialWatchMode::ActiveSource;
                s.auth_watch_snapshot.clear();
            }

            unsafe {
                let _ = PostMessageW(hwnd, WM_APP_USAGE_UPDATED, WPARAM(0), LPARAM(0));
            }
        }
        Err(e) => {
            let auth_watch = match e {
                poller::PollError::AuthRequired | poller::PollError::TokenExpired => Some((
                    poller::CredentialWatchMode::ActiveSource,
                    poller::credential_watch_snapshot(poller::CredentialWatchMode::ActiveSource),
                )),
                poller::PollError::NoCredentials => Some((
                    poller::CredentialWatchMode::AllSources,
                    poller::credential_watch_snapshot(poller::CredentialWatchMode::AllSources),
                )),
                poller::PollError::RequestFailed => None,
            };
            // Distinguish auth-required errors from transient errors.
            let notify_auth_error = {
                let mut state = lock_state();
                let mut should_notify = false;
                if let Some(s) = state.as_mut() {
                    s.last_poll_ok = false;
                    match auth_watch {
                        Some((watch_mode, watch_snapshot)) => {
                            // Only show the balloon on the first failure so it doesn't spam.
                            if s.retry_count == 0 || s.force_notify_auth_error {
                                should_notify = true;
                            }
                            s.force_notify_auth_error = false;
                            s.auth_error_paused_polling = true;
                            s.auth_watch_mode = watch_mode;
                            s.auth_watch_snapshot = watch_snapshot;
                            s.session_text = "!".to_string();
                            s.weekly_text = "!".to_string();
                            s.codex_session_text = "!".to_string();
                            s.codex_weekly_text = "!".to_string();
                            s.retry_count = s.retry_count.saturating_add(1);
                            unsafe {
                                let _ = KillTimer(hwnd, TIMER_POLL);
                                let _ = KillTimer(hwnd, TIMER_RESET_POLL);
                                let _ = KillTimer(hwnd, TIMER_COUNTDOWN);
                                SetTimer(hwnd, TIMER_POLL, s.poll_interval_ms, None);
                            }
                        }
                        _ => {
                            // Transient network / credential-missing errors: exponential backoff.
                            s.force_notify_auth_error = false;
                            s.auth_error_paused_polling = false;
                            s.auth_watch_mode = poller::CredentialWatchMode::ActiveSource;
                            s.auth_watch_snapshot.clear();
                            s.session_text = "...".to_string();
                            s.weekly_text = "...".to_string();
                            s.codex_session_text = "...".to_string();
                            s.codex_weekly_text = "...".to_string();
                            s.retry_count = s.retry_count.saturating_add(1);
                            let backoff = RETRY_BASE_MS.saturating_mul(
                                1u32.checked_shl(s.retry_count - 1).unwrap_or(u32::MAX),
                            );
                            let retry_ms = backoff.min(s.poll_interval_ms);
                            unsafe {
                                let _ = KillTimer(hwnd, TIMER_RESET_POLL);
                                SetTimer(hwnd, TIMER_POLL, retry_ms, None);
                            }
                        }
                    }
                }
                should_notify
            };

            if notify_auth_error {
                let balloon = {
                    let state = lock_state();
                    state.as_ref().map(|s| {
                        if s.show_claude_code {
                            (
                                s.language.strings(),
                                tray_icon::TrayIconKind::Claude,
                                s.language.strings().token_expired_title,
                                s.language.strings().token_expired_body,
                            )
                        } else {
                            (
                                s.language.strings(),
                                tray_icon::TrayIconKind::Codex,
                                s.language.strings().codex_token_expired_title,
                                s.language.strings().codex_token_expired_body,
                            )
                        }
                    })
                };
                if let Some((_strings, kind, title, body)) = balloon {
                    tray_icon::notify_balloon(tray_owner(hwnd), kind, title, body);
                }
            }

            unsafe {
                let _ = PostMessageW(hwnd, WM_APP_USAGE_UPDATED, WPARAM(0), LPARAM(0));
            }
        }
    }
}

fn schedule_countdown_timer() {
    let state = lock_state();
    let s = match state.as_ref() {
        Some(s) => s,
        None => return,
    };

    let hwnd = s.hwnd.to_hwnd();
    if !s.last_poll_ok {
        unsafe {
            let _ = KillTimer(hwnd, TIMER_COUNTDOWN);
            let _ = KillTimer(hwnd, TIMER_RESET_POLL);
        }
        return;
    }

    let data = match &s.data {
        Some(d) => d,
        None => return,
    };

    // If a reset time has passed, poll every 5s to pick up fresh data
    if poller::app_is_past_reset(data) {
        unsafe {
            SetTimer(hwnd, TIMER_RESET_POLL, 5_000, None);
        }
    }

    let dt = show_detailed_remaining();
    let delays = [
        data.claude_code
            .as_ref()
            .and_then(|usage| poller::time_until_display_change(usage.session.resets_at, dt)),
        data.claude_code
            .as_ref()
            .and_then(|usage| poller::time_until_display_change(usage.weekly.resets_at, dt)),
        data.codex
            .as_ref()
            .and_then(|usage| poller::time_until_display_change(usage.session.resets_at, dt)),
        data.codex
            .as_ref()
            .and_then(|usage| poller::time_until_display_change(usage.weekly.resets_at, dt)),
    ];
    let min_delay = delays.into_iter().flatten().min();

    let ms = min_delay
        .unwrap_or(Duration::from_secs(60))
        .as_millis()
        .max(1000) as u32;

    unsafe {
        SetTimer(hwnd, TIMER_COUNTDOWN, ms, None);
    }
}

fn check_theme_change() {
    let new_dark = theme::is_dark_mode();
    let changed = {
        let mut state = lock_state();
        if let Some(s) = state.as_mut() {
            if s.is_dark != new_dark {
                s.is_dark = new_dark;
                true
            } else {
                false
            }
        } else {
            false
        }
    };
    if changed {
        render_layered();
    }
}

fn check_language_change() {
    if update_language_change() {
        render_layered();
    }
}

fn update_display() {
    let mut state = lock_state();
    let s = match state.as_mut() {
        Some(s) => s,
        None => return,
    };

    // Don't overwrite error text with stale cached data
    if !s.last_poll_ok {
        return;
    }

    refresh_usage_texts(s);
}

fn suppress_tray_reposition_for(duration: Duration) {
    let mut until = SUPPRESS_TRAY_REPOSITION_UNTIL
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    *until = Some(Instant::now() + duration);
}

fn tray_reposition_is_suppressed() -> bool {
    let now = Instant::now();
    let mut until = SUPPRESS_TRAY_REPOSITION_UNTIL
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    match *until {
        Some(deadline) if now < deadline => true,
        Some(_) => {
            *until = None;
            false
        }
        None => false,
    }
}

fn bounds(rect: RECT) -> crate::overlay_visibility::Bounds {
    crate::overlay_visibility::Bounds {
        left: rect.left,
        top: rect.top,
        right: rect.right,
        bottom: rect.bottom,
    }
}

fn window_class(hwnd: HWND) -> String {
    let mut class = [0u16; 128];
    let count = unsafe { GetClassNameW(hwnd, &mut class) }.max(0) as usize;
    String::from_utf16_lossy(&class[..count])
}

/// Shell windows may span the display, while maximized application frames may
/// extend outside it. Only an application's client area counts as fullscreen.
fn fullscreen_application_on_monitor(
    foreground: HWND,
    taskbar: HWND,
    monitor: HMONITOR,
    rect: RECT,
) -> bool {
    unsafe {
        if foreground.is_invalid()
            || foreground == taskbar
            || IsChild(taskbar, foreground).as_bool()
            || !IsWindowVisible(foreground).as_bool()
            || IsIconic(foreground).as_bool()
            || MonitorFromWindow(foreground, MONITOR_DEFAULTTONEAREST) != monitor
        {
            return false;
        }
        let class = window_class(foreground);
        if crate::overlay_visibility::is_shell_class(&class) {
            return false;
        }
        let mut client = RECT::default();
        if GetClientRect(foreground, &mut client).is_err() {
            return false;
        }
        let mut corners = [
            POINT {
                x: client.left,
                y: client.top,
            },
            POINT {
                x: client.right,
                y: client.bottom,
            },
        ];
        // A zero return is also a valid mapping when the window starts at (0,0).
        MapWindowPoints(foreground, HWND::default(), &mut corners);
        crate::overlay_visibility::is_fullscreen_client(
            &class,
            crate::overlay_visibility::Bounds {
                left: corners[0].x,
                top: corners[0].y,
                right: corners[1].x,
                bottom: corners[1].y,
            },
            bounds(rect),
        )
    }
}

fn queue_overlay_refresh() {
    let controller = HWND(RECOVERY_WINDOW.load(Ordering::Relaxed) as *mut _);
    if !controller.is_invalid() && !OVERLAY_REFRESH_PENDING.swap(true, Ordering::Relaxed) {
        if unsafe { PostMessageW(controller, WM_APP_OVERLAY_REFRESH, WPARAM(0), LPARAM(0)) }
            .is_err()
        {
            OVERLAY_REFRESH_PENDING.store(false, Ordering::Relaxed);
        }
    }
}

unsafe extern "system" fn on_overlay_event(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    object: i32,
    _child: i32,
    _thread: u32,
    _time: u32,
) {
    if event == native_interop::EVENT_SYSTEM_FOREGROUND {
        queue_overlay_refresh();
        return;
    }
    if !matches!(
        event,
        native_interop::EVENT_OBJECT_SHOW
            | native_interop::EVENT_OBJECT_HIDE
            | native_interop::EVENT_OBJECT_REORDER
            | native_interop::EVENT_OBJECT_FOCUS
            | native_interop::EVENT_OBJECT_LOCATIONCHANGE
    ) {
        return;
    }
    let taskbar = lock_state().as_ref().and_then(|s| s.taskbar_hwnd);
    let shell_event =
        taskbar.is_some_and(|taskbar| hwnd == taskbar || IsChild(taskbar, hwnd).as_bool());
    let foreground_event = object == 0 && hwnd == GetForegroundWindow();
    // Desktop reorder events also report top-level topmost ordering changes.
    let desktop_reorder = event == native_interop::EVENT_OBJECT_REORDER
        && (hwnd.is_invalid() || hwnd == GetDesktopWindow());
    if shell_event || foreground_event || desktop_reorder {
        queue_overlay_refresh();
    }
}

fn position_at_taskbar() {
    refresh_dpi();
    let (hwnd, tray_offset, taskbar, preferred, dragging) = {
        let state = lock_state();
        let Some(s) = state.as_ref() else { return };
        let Some(taskbar) = s.taskbar_hwnd else {
            return;
        };
        (
            s.hwnd.to_hwnd(),
            s.tray_offset,
            taskbar,
            s.widget_visible,
            s.dragging,
        )
    };
    unsafe {
        if dragging && windows::Win32::UI::Input::KeyboardAndMouse::GetCapture() == hwnd {
            return;
        }
        if dragging {
            let mut state = lock_state();
            if let Some(s) = state.as_mut() {
                s.dragging = false;
            }
            diagnose::log("cleared stale drag state after capture was lost");
        }
        let Some(taskbar_rect) = native_interop::get_window_rect_safe(taskbar) else {
            diagnose::log("overlay positioning failed: taskbar bounds unavailable");
            return;
        };
        if taskbar_rect.right <= taskbar_rect.left || taskbar_rect.bottom <= taskbar_rect.top {
            return;
        }
        let monitor = MonitorFromWindow(taskbar, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(monitor, &mut info).as_bool() {
            diagnose::log("overlay positioning failed: monitor bounds unavailable");
            return;
        }
        let foreground = GetForegroundWindow();
        let foreground_class = window_class(foreground);
        let fullscreen =
            fullscreen_application_on_monitor(foreground, taskbar, monitor, info.rcMonitor);
        let reason = crate::overlay_visibility::hide_reason(
            preferred,
            IsWindowVisible(taskbar).as_bool(),
            bounds(taskbar_rect),
            bounds(info.rcMonitor),
            fullscreen,
        );
        let width = total_widget_width();
        let height = sc(WIDGET_HEIGHT);
        let tray_left = native_interop::find_child_window(taskbar, "TrayNotifyWnd")
            .and_then(native_interop::get_window_rect_safe)
            .map(|r| r.left)
            .unwrap_or(taskbar_rect.right);
        let min_x = info.rcMonitor.left;
        let max_x = (info.rcMonitor.right - width).max(min_x);
        let x = (tray_left - width - tray_offset).clamp(min_x, max_x);
        let y = compute_anchor_y(
            taskbar_rect.top,
            taskbar_rect.bottom - taskbar_rect.top,
            height,
        );
        let flags = SWP_NOACTIVATE
            | if reason.is_none() {
                SWP_SHOWWINDOW
            } else {
                SWP_HIDEWINDOW
            };
        if let Err(error) = SetWindowPos(hwnd, HWND_TOPMOST, x, y, width, height, flags) {
            diagnose::log_error("overlay SetWindowPos failed", error);
            RECREATE_WIDGET.store(true, Ordering::Relaxed);
        }
        let actual_visible = IsWindowVisible(hwnd).as_bool();
        let snapshot = format!(
            "overlay hwnd={hwnd:?} preferred={preferred} visible={actual_visible} reason={reason:?} position=({x},{y},{width},{height}) taskbar={taskbar_rect:?} foreground={foreground_class}");
        static LAST_SNAPSHOT: Mutex<Option<(String, Instant)>> = Mutex::new(None);
        let mut last = LAST_SNAPSHOT.lock().unwrap_or_else(|e| e.into_inner());
        if last
            .as_ref()
            .is_none_or(|(text, at)| text != &snapshot || at.elapsed() >= Duration::from_secs(60))
        {
            diagnose::log(&snapshot);
            *last = Some((snapshot, Instant::now()));
        }
        drop(last);
        character::set_widget_visible(reason.is_none());
        if let Some(rect) = native_interop::get_window_rect_safe(hwnd) {
            character::reposition(rect);
        }
    }
}

fn compute_anchor_y(anchor_top: i32, anchor_height: i32, widget_height: i32) -> i32 {
    let anchor_bottom = anchor_top + anchor_height;
    (anchor_bottom - widget_height).max(anchor_top)
}

#[cfg(test)]
mod desktop_tests {
    use super::*;

    #[test]
    fn native_client_geometry_distinguishes_fullscreen_from_maximized_frame() {
        unsafe {
            let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            let monitor = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
            let mut info = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            assert!(GetMonitorInfoW(monitor, &mut info).as_bool());
            let rect = info.rcMonitor;
            // Alpha zero exercises real HWND/client geometry without displaying
            // test content or taking focus from the user's application.
            let class = native_interop::wide_str("STATIC");
            let hwnd = CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                PCWSTR::from_raw(class.as_ptr()),
                PCWSTR::null(),
                WS_POPUP,
                rect.left,
                rect.top,
                rect.right - rect.left,
                rect.bottom - rect.top,
                HWND::default(),
                HMENU::default(),
                GetModuleHandleW(PCWSTR::null()).unwrap(),
                None,
            )
            .unwrap();
            struct TestWindow(HWND);
            impl Drop for TestWindow {
                fn drop(&mut self) {
                    unsafe {
                        let _ = DestroyWindow(self.0);
                    }
                }
            }
            let _guard = TestWindow(hwnd);
            SetLayeredWindowAttributes(hwnd, COLORREF(0), 0, LWA_ALPHA).unwrap();
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            assert!(fullscreen_application_on_monitor(
                hwnd,
                HWND::default(),
                monitor,
                rect
            ));
            SetWindowLongW(
                hwnd,
                GWL_STYLE,
                WS_OVERLAPPEDWINDOW.0 as i32 | WS_VISIBLE.0 as i32,
            );
            SetWindowPos(
                hwnd,
                HWND::default(),
                rect.left,
                rect.top,
                rect.right - rect.left,
                rect.bottom - rect.top,
                SWP_NOACTIVATE | SWP_NOZORDER | SWP_FRAMECHANGED,
            )
            .unwrap();
            assert!(!fullscreen_application_on_monitor(
                hwnd,
                HWND::default(),
                monitor,
                rect
            ));
            let _ = ShowWindow(hwnd, SW_HIDE);
            assert!(!fullscreen_application_on_monitor(
                hwnd,
                HWND::default(),
                monitor,
                rect
            ));
            if let Some(taskbar) = native_interop::find_taskbar() {
                assert!(!fullscreen_application_on_monitor(
                    taskbar, taskbar, monitor, rect
                ));
            }
        }
    }
}

/// WinEvent callback for tray icon location changes
unsafe extern "system" fn on_tray_location_changed(
    _hook: HWINEVENTHOOK,
    _event: u32,
    hwnd: HWND,
    _id_object: i32,
    _id_child: i32,
    _thread: u32,
    _time: u32,
) {
    static LAST_REPOSITION: Mutex<Option<std::time::Instant>> = Mutex::new(None);

    let is_tray = {
        let state = lock_state();
        state
            .as_ref()
            .and_then(|s| s.tray_notify_hwnd)
            .map(|h| h == hwnd)
            .unwrap_or(false)
    };

    if is_tray {
        if tray_reposition_is_suppressed() {
            return;
        }

        let should_reposition = {
            let mut last = LAST_REPOSITION.lock().unwrap_or_else(|e| e.into_inner());
            let now = std::time::Instant::now();
            if last
                .map(|t| now.duration_since(t).as_millis() > 500)
                .unwrap_or(true)
            {
                *last = Some(now);
                true
            } else {
                false
            }
        };
        if should_reposition {
            position_at_taskbar();
            render_layered();
        }
    }
}

/// Main window procedure
unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let _ = BeginPaint(hwnd, &mut ps);
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_DISPLAYCHANGE | WM_DPICHANGED_MSG | WM_SETTINGCHANGE => {
            if msg == WM_DPICHANGED_MSG {
                let new_dpi = (wparam.0 & 0xFFFF) as u32;
                CURRENT_DPI.store(new_dpi, Ordering::Relaxed);
            }
            if msg == WM_SETTINGCHANGE {
                check_theme_change();
                check_language_change();
            }
            refresh_dpi();
            position_at_taskbar();
            render_layered();
            LRESULT(0)
        }
        WM_TIMER => {
            let timer_id = wparam.0;
            match timer_id {
                TIMER_POLL => {
                    let auth_watch = {
                        let state = lock_state();
                        state.as_ref().map(|s| {
                            (
                                s.auth_error_paused_polling,
                                s.auth_watch_mode,
                                s.auth_watch_snapshot.clone(),
                            )
                        })
                    };
                    match auth_watch {
                        Some((true, watch_mode, previous_snapshot)) => {
                            let current_snapshot = poller::credential_watch_snapshot(watch_mode);
                            if current_snapshot != previous_snapshot {
                                let mut state = lock_state();
                                if let Some(s) = state.as_mut() {
                                    if s.auth_error_paused_polling
                                        && s.auth_watch_mode == watch_mode
                                    {
                                        s.auth_watch_snapshot = current_snapshot;
                                    }
                                }
                                drop(state);
                                let sh = SendHwnd::from_hwnd(hwnd);
                                std::thread::spawn(move || {
                                    do_poll(sh);
                                });
                            }
                        }
                        Some((false, _, _)) => {
                            let sh = SendHwnd::from_hwnd(hwnd);
                            std::thread::spawn(move || {
                                do_poll(sh);
                            });
                        }
                        None => {}
                    }
                }
                TIMER_COUNTDOWN => {
                    update_display();
                    render_layered();
                    schedule_countdown_timer();
                }
                TIMER_RESET_POLL => {
                    let should_poll = {
                        let state = lock_state();
                        state
                            .as_ref()
                            .map(|s| !s.auth_error_paused_polling)
                            .unwrap_or(false)
                    };
                    if should_poll {
                        let sh = SendHwnd::from_hwnd(hwnd);
                        std::thread::spawn(move || {
                            do_poll(sh);
                        });
                    }
                }
                TIMER_UPDATE_CHECK => {
                    begin_update_check(hwnd, false);
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_APP_USAGE_UPDATED => {
            // The real Codex windows can change the number of displayed columns.
            let mut rect = RECT::default();
            if GetClientRect(hwnd, &mut rect).is_ok()
                && rect.right - rect.left != total_widget_width()
            {
                position_at_taskbar();
            }
            check_theme_change();
            check_language_change();
            render_layered();
            {
                let (max_pct, sess_pct, sess_reset, lang) = {
                    let state = lock_state();
                    match state.as_ref() {
                        Some(s) => {
                            let mut m = 0.0_f64;
                            // Track the 5h (session) usage + reset of the most-used
                            // enabled model for the burn-rate prediction.
                            let mut best_sess = -1.0_f64;
                            let mut best_reset = None;
                            if s.show_claude_code {
                                m = m.max(s.session_percent).max(s.weekly_percent);
                                if s.session_percent > best_sess {
                                    best_sess = s.session_percent;
                                    best_reset = s
                                        .data
                                        .as_ref()
                                        .and_then(|d| d.claude_code.as_ref())
                                        .and_then(|u| u.session.resets_at);
                                }
                            }
                            if s.show_codex {
                                m = m.max(s.codex_session_percent).max(s.codex_weekly_percent);
                                if s.codex_session_percent > best_sess {
                                    best_sess = s.codex_session_percent;
                                    best_reset = s
                                        .data
                                        .as_ref()
                                        .and_then(|d| d.codex.as_ref())
                                        .and_then(|u| u.session.resets_at);
                                }
                            }
                            let sess = best_sess.max(0.0);
                            (m, sess, best_reset, s.language)
                        }
                        None => (0.0, 0.0, None, LanguageId::English),
                    }
                };
                character::on_usage_update(max_pct, sess_pct, sess_reset, lang);
            }
            schedule_countdown_timer();
            suppress_tray_reposition_for(Duration::from_millis(
                TRAY_ICON_UPDATE_REPOSITION_SUPPRESS_MS,
            ));
            sync_tray_icons(hwnd);
            LRESULT(0)
        }
        WM_APP_UPDATE_CHECK_COMPLETE => {
            schedule_auto_update_check(hwnd);
            LRESULT(0)
        }
        WM_SETCURSOR => {
            let is_dragging = {
                let state = lock_state();
                state.as_ref().map(|s| s.dragging).unwrap_or(false)
            };
            // Always show resize cursor while dragging or when hovering divider zone
            let hit_test = (lparam.0 & 0xFFFF) as u16;
            if is_dragging {
                let cursor = LoadCursorW(HINSTANCE::default(), IDC_SIZEWE).unwrap_or_default();
                SetCursor(cursor);
                return LRESULT(1);
            }
            if hit_test == 1 {
                // HTCLIENT
                let mut pt = POINT::default();
                let _ = GetCursorPos(&mut pt);
                let _ = ScreenToClient(hwnd, &mut pt);
                if pt.x < sc(DIVIDER_HIT_ZONE) {
                    let cursor = LoadCursorW(HINSTANCE::default(), IDC_SIZEWE).unwrap_or_default();
                    SetCursor(cursor);
                    return LRESULT(1);
                }
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_LBUTTONDOWN => {
            let client_x = (lparam.0 & 0xFFFF) as i16 as i32;
            if client_x < sc(DIVIDER_HIT_ZONE) {
                let mut pt = POINT::default();
                let _ = GetCursorPos(&mut pt);
                let mut state = lock_state();
                if let Some(s) = state.as_mut() {
                    s.dragging = true;
                    s.drag_start_mouse_x = pt.x;
                    s.drag_start_offset = s.tray_offset;
                }
                drop(state);
                SetCapture(hwnd);
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let is_dragging = {
                let state = lock_state();
                state.as_ref().map(|s| s.dragging).unwrap_or(false)
            };
            if is_dragging {
                let mut pt = POINT::default();
                let _ = GetCursorPos(&mut pt);
                let move_target = {
                    let mut state = lock_state();
                    let s = match state.as_mut() {
                        Some(s) => s,
                        None => return LRESULT(0),
                    };

                    // Moving mouse left = positive delta = larger offset (further left)
                    let delta = s.drag_start_mouse_x - pt.x;
                    let mut new_offset = s.drag_start_offset + delta;

                    // Clamp: offset >= 0 (can't go right of default)
                    if new_offset < 0 {
                        new_offset = 0;
                    }

                    let taskbar_hwnd = s.taskbar_hwnd;
                    let hwnd_val = s.hwnd.to_hwnd();

                    // Clamp: don't go past left edge of taskbar
                    if let Some(taskbar_hwnd) = taskbar_hwnd {
                        if let Some(taskbar_rect) = native_interop::get_taskbar_rect(taskbar_hwnd) {
                            let mut tray_left = taskbar_rect.right;
                            if let Some(tray_hwnd) =
                                native_interop::find_child_window(taskbar_hwnd, "TrayNotifyWnd")
                            {
                                if let Some(tray_rect) =
                                    native_interop::get_window_rect_safe(tray_hwnd)
                                {
                                    tray_left = tray_rect.left;
                                }
                            }
                            let widget_width = total_widget_width_for_state(s);
                            let max_offset = (tray_left - taskbar_rect.left - widget_width).max(0);
                            if new_offset > max_offset {
                                new_offset = max_offset;
                            }

                            s.tray_offset = new_offset;

                            let taskbar_height = taskbar_rect.bottom - taskbar_rect.top;
                            let anchor_top = taskbar_rect.top;
                            let anchor_height = taskbar_height;
                            let widget_height = sc(WIDGET_HEIGHT);
                            let y = compute_anchor_y(anchor_top, anchor_height, widget_height);
                            let x = tray_left - widget_width - new_offset;
                            Some((hwnd_val, x, y, widget_width, widget_height))
                        } else {
                            s.tray_offset = new_offset;
                            None
                        }
                    } else {
                        s.tray_offset = new_offset;
                        None
                    }
                };

                if let Some((hwnd_val, x, y, widget_width, widget_height)) = move_target {
                    native_interop::move_window(hwnd_val, x, y, widget_width, widget_height);
                }
            }
            LRESULT(0)
        }
        WM_CAPTURECHANGED | WM_CANCELMODE => {
            let mut state = lock_state();
            if let Some(s) = state.as_mut() {
                s.dragging = false;
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let was_dragging = {
                let mut state = lock_state();
                if let Some(s) = state.as_mut() {
                    if s.dragging {
                        s.dragging = false;
                        let offset = s.tray_offset;
                        Some(offset)
                    } else {
                        None
                    }
                } else {
                    None
                }
            };
            if was_dragging.is_some() {
                let _ = ReleaseCapture();
                save_state_settings();
            }
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            show_context_menu(hwnd);
            LRESULT(0)
        }
        WM_COMMAND => {
            let id = wparam.0 as u16;
            match id {
                1 => {
                    {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            s.session_text = "...".to_string();
                            s.weekly_text = "...".to_string();
                            s.codex_session_text = "...".to_string();
                            s.codex_weekly_text = "...".to_string();
                            s.force_notify_auth_error = true;
                        }
                    }
                    render_layered();
                    let sh = SendHwnd::from_hwnd(hwnd);
                    std::thread::spawn(move || {
                        do_poll(sh);
                    });
                }
                IDM_VERSION_ACTION => {
                    let (install_channel, release) = {
                        let state = lock_state();
                        match state.as_ref() {
                            Some(s) => (
                                s.install_channel,
                                match &s.update_status {
                                    UpdateStatus::Available(release) => Some(release.clone()),
                                    _ => None,
                                },
                            ),
                            None => (InstallChannel::Portable, None),
                        }
                    };

                    match install_channel {
                        InstallChannel::Winget => {
                            if release.is_some() {
                                begin_winget_update(hwnd);
                            } else {
                                begin_update_check(hwnd, true);
                            }
                        }
                        InstallChannel::Portable => {
                            if let Some(release) = release {
                                begin_update_apply(hwnd, release);
                            } else {
                                begin_update_check(hwnd, true);
                            }
                        }
                    }
                }
                2 => {
                    let _ = SendMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
                }
                IDM_RESET_POSITION => {
                    {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            s.tray_offset = 0;
                        }
                    }
                    save_state_settings();
                    position_at_taskbar();
                }
                IDM_START_WITH_WINDOWS => {
                    set_startup_enabled(!is_startup_enabled());
                }
                IDM_FREQ_1MIN | IDM_FREQ_5MIN | IDM_FREQ_15MIN | IDM_FREQ_1HOUR => {
                    let new_interval = match id {
                        IDM_FREQ_1MIN => POLL_1_MIN,
                        IDM_FREQ_5MIN => POLL_5_MIN,
                        IDM_FREQ_15MIN => POLL_15_MIN,
                        IDM_FREQ_1HOUR => POLL_1_HOUR,
                        _ => POLL_15_MIN,
                    };
                    {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            s.poll_interval_ms = new_interval;
                        }
                    }
                    save_state_settings();
                    // Reset the poll timer with the new interval
                    SetTimer(hwnd, TIMER_POLL, new_interval, None);
                }
                IDM_CLAUDE_5H | IDM_CLAUDE_7D | IDM_CODEX_5H | IDM_CODEX_7D => {
                    let (provider, quota) = match id {
                        IDM_CLAUDE_5H => (Provider::Claude, Quota::Session),
                        IDM_CLAUDE_7D => (Provider::Claude, Quota::Weekly),
                        IDM_CODEX_5H => (Provider::Codex, Quota::Session),
                        _ => (Provider::Codex, Quota::Weekly),
                    };
                    {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            s.display_limits.toggle(provider, quota);
                        }
                    }
                    save_state_settings();
                    position_at_taskbar();
                    render_layered();
                    sync_tray_icons(hwnd);
                }
                IDM_MODEL_CLAUDE_CODE | IDM_MODEL_CODEX => {
                    {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            match id {
                                IDM_MODEL_CLAUDE_CODE => {
                                    if s.show_codex || !s.show_claude_code {
                                        s.show_claude_code = !s.show_claude_code;
                                    }
                                }
                                IDM_MODEL_CODEX => {
                                    if s.show_claude_code || !s.show_codex {
                                        s.show_codex = !s.show_codex;
                                    }
                                }
                                _ => {}
                            }
                            s.session_text = "...".to_string();
                            s.weekly_text = "...".to_string();
                            s.codex_session_text = "...".to_string();
                            s.codex_weekly_text = "...".to_string();
                        }
                    }
                    save_state_settings();
                    position_at_taskbar();
                    render_layered();
                    sync_tray_icons(hwnd);
                    let sh = SendHwnd::from_hwnd(hwnd);
                    std::thread::spawn(move || {
                        do_poll(sh);
                    });
                }
                IDM_LANG_SYSTEM
                | IDM_LANG_ENGLISH
                | IDM_LANG_DUTCH
                | IDM_LANG_SPANISH
                | IDM_LANG_FRENCH
                | IDM_LANG_GERMAN
                | IDM_LANG_JAPANESE
                | IDM_LANG_KOREAN
                | IDM_LANG_TRADITIONAL_CHINESE => {
                    let language_override = match id {
                        IDM_LANG_SYSTEM => None,
                        IDM_LANG_ENGLISH => Some(LanguageId::English),
                        IDM_LANG_DUTCH => Some(LanguageId::Dutch),
                        IDM_LANG_SPANISH => Some(LanguageId::Spanish),
                        IDM_LANG_FRENCH => Some(LanguageId::French),
                        IDM_LANG_GERMAN => Some(LanguageId::German),
                        IDM_LANG_JAPANESE => Some(LanguageId::Japanese),
                        IDM_LANG_KOREAN => Some(LanguageId::Korean),
                        IDM_LANG_TRADITIONAL_CHINESE => Some(LanguageId::TraditionalChinese),
                        _ => None,
                    };
                    let new_language = {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            apply_language_to_state(s, language_override);
                            s.language
                        } else {
                            LanguageId::English
                        }
                    };
                    character::set_language(new_language);
                    save_state_settings();
                    render_layered();
                }
                IDM_BAR_THEME_SEGMENTED
                | IDM_BAR_THEME_FLAT
                | IDM_BAR_THEME_GRADIENT
                | IDM_BAR_THEME_PIXEL => {
                    let theme = match id {
                        IDM_BAR_THEME_FLAT => BarTheme::Flat,
                        IDM_BAR_THEME_GRADIENT => BarTheme::Gradient,
                        IDM_BAR_THEME_PIXEL => BarTheme::Pixel,
                        _ => BarTheme::Segmented,
                    };
                    CURRENT_BAR_THEME.store(theme.to_u8(), Ordering::Relaxed);
                    save_state_settings();
                    render_layered();
                }
                IDM_CHAR_SHOW => {
                    character::set_enabled(!character::is_enabled());
                    if let Some(anchor) = native_interop::get_window_rect_safe(hwnd) {
                        character::reposition(anchor);
                    }
                    save_state_settings();
                }
                IDM_CHAR_CAT | IDM_CHAR_DOG | IDM_CHAR_GIRL | IDM_CHAR_BOTH => {
                    let kind = match id {
                        IDM_CHAR_DOG => CharacterKind::Dog,
                        IDM_CHAR_GIRL => CharacterKind::Girl,
                        IDM_CHAR_BOTH => CharacterKind::Both,
                        _ => CharacterKind::Cat,
                    };
                    character::set_kind(kind);
                    save_state_settings();
                }
                IDM_CAT_COLOR_0 | IDM_CAT_COLOR_1 => {
                    character::set_variant(true, if id == IDM_CAT_COLOR_1 { 1 } else { 0 });
                    save_state_settings();
                }
                IDM_DOG_COLOR_0 | IDM_DOG_COLOR_1 => {
                    character::set_variant(false, if id == IDM_DOG_COLOR_1 { 1 } else { 0 });
                    save_state_settings();
                }
                IDM_SEG_4 | IDM_SEG_6 | IDM_SEG_8 | IDM_SEG_10 => {
                    let n: u8 = match id {
                        IDM_SEG_4 => 4,
                        IDM_SEG_6 => 6,
                        IDM_SEG_8 => 8,
                        _ => 10,
                    };
                    CURRENT_SEGMENT_COUNT.store(n, Ordering::Relaxed);
                    save_state_settings();
                    // Width changes, so reposition before re-rendering.
                    position_at_taskbar();
                    render_layered();
                }
                IDM_TOGGLE_LABELS | IDM_TOGGLE_PERCENT | IDM_TOGGLE_TIMER => {
                    match id {
                        IDM_TOGGLE_LABELS => {
                            SHOW_LABELS.fetch_xor(true, Ordering::Relaxed);
                        }
                        IDM_TOGGLE_PERCENT => {
                            SHOW_PERCENTAGES.fetch_xor(true, Ordering::Relaxed);
                        }
                        _ => {
                            SHOW_RESET_TIMER.fetch_xor(true, Ordering::Relaxed);
                        }
                    }
                    {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            refresh_usage_texts(s);
                        }
                    }
                    save_state_settings();
                    position_at_taskbar();
                    render_layered();
                }
                IDM_TOGGLE_DETAILED => {
                    SHOW_DETAILED_REMAINING.fetch_xor(true, Ordering::Relaxed);
                    {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            refresh_usage_texts(s);
                        }
                    }
                    save_state_settings();
                    position_at_taskbar();
                    render_layered();
                    schedule_countdown_timer();
                }
                IDM_PACE_TOGGLE => {
                    let next = if pace_style() == PaceStyle::Tick {
                        PaceStyle::Off
                    } else {
                        PaceStyle::Tick
                    };
                    PACE_STYLE.store(next.to_u8(), Ordering::Relaxed);
                    save_state_settings();
                    render_layered();
                }
                id if id == tray_icon::IDM_TOGGLE_WIDGET => {
                    toggle_widget_visibility(hwnd);
                }
                _ => {}
            }
            LRESULT(0)
        }
        _ if msg == WM_APP_TRAY => {
            match tray_icon::handle_message(lparam) {
                tray_icon::TrayAction::ToggleWidget => {
                    toggle_widget_visibility(hwnd);
                }
                tray_icon::TrayAction::ShowContextMenu => {
                    show_context_menu(hwnd);
                }
                tray_icon::TrayAction::None => {}
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            {
                let mut state = lock_state();
                if let Some(s) = state.as_mut() {
                    s.shutting_down = true;
                }
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_DESTROY => {
            let (hook, closing) = {
                let mut state = lock_state();
                if let Some(s) = state.as_mut() {
                    if s.hwnd.to_hwnd() != hwnd {
                        // Destruction of a replaced window must not mutate current state.
                        return LRESULT(0);
                    }
                    let hook = s.win_event_hook.take();
                    s.hwnd = SendHwnd(0);
                    s.taskbar_hwnd = None;
                    s.tray_notify_hwnd = None;
                    (hook, s.shutting_down)
                } else {
                    (None, true)
                }
            };
            if let Some(hook) = hook {
                native_interop::unhook_win_event(hook);
            }
            tray_icon::remove_all(hwnd);
            if closing {
                let hooks = lock_state()
                    .as_mut()
                    .map(|s| std::mem::take(&mut s.overlay_event_hooks))
                    .unwrap_or_default();
                for hook in hooks {
                    native_interop::unhook_win_event(hook);
                }
                tray_icon::remove_all(tray_owner(hwnd));
                character::destroy();
                PostQuitMessage(0);
            } else {
                diagnose::log("widget destroyed by shell; controller will recreate it");
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn show_context_menu(hwnd: HWND) {
    unsafe {
        let (
            current_interval,
            strings,
            language,
            language_override,
            install_channel,
            update_status,
            widget_visible,
            show_claude_code,
            show_codex,
            display_limits,
            codex_windows,
        ) = {
            let state = lock_state();
            match state.as_ref() {
                Some(s) => (
                    s.poll_interval_ms,
                    s.language.strings(),
                    s.language,
                    s.language_override,
                    s.install_channel,
                    s.update_status.clone(),
                    s.widget_visible,
                    s.show_claude_code,
                    s.show_codex,
                    effective_display_limits(s),
                    s.data
                        .as_ref()
                        .and_then(|data| data.codex.as_ref())
                        .and_then(|usage| usage.available_windows),
                ),
                None => (
                    POLL_15_MIN,
                    LanguageId::English.strings(),
                    LanguageId::English,
                    None,
                    InstallChannel::Portable,
                    UpdateStatus::Idle,
                    true,
                    true,
                    false,
                    DisplayLimits::default(),
                    None,
                ),
            }
        };

        let menu = CreatePopupMenu().unwrap();

        // Quick actions
        let refresh_str = native_interop::wide_str(strings.refresh);
        let _ = AppendMenuW(
            menu,
            MENU_ITEM_FLAGS(0),
            1,
            PCWSTR::from_raw(refresh_str.as_ptr()),
        );

        let widget_label = native_interop::wide_str(strings.show_widget);
        let widget_flags = if widget_visible {
            MF_CHECKED
        } else {
            MENU_ITEM_FLAGS(0)
        };
        let _ = AppendMenuW(
            menu,
            widget_flags,
            tray_icon::IDM_TOGGLE_WIDGET as usize,
            PCWSTR::from_raw(widget_label.as_ptr()),
        );

        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());

        // Models submenu
        let models_menu = CreatePopupMenu().unwrap();
        let claude_model = native_interop::wide_str(strings.claude_code_model);
        let claude_flags = if show_claude_code {
            MF_CHECKED
        } else {
            MENU_ITEM_FLAGS(0)
        };
        let _ = AppendMenuW(
            models_menu,
            claude_flags,
            IDM_MODEL_CLAUDE_CODE as usize,
            PCWSTR::from_raw(claude_model.as_ptr()),
        );
        let codex_model = native_interop::wide_str(strings.codex_model);
        let codex_flags = if show_codex {
            MF_CHECKED
        } else {
            MENU_ITEM_FLAGS(0)
        };
        let _ = AppendMenuW(
            models_menu,
            codex_flags,
            IDM_MODEL_CODEX as usize,
            PCWSTR::from_raw(codex_model.as_ptr()),
        );
        let models_label = native_interop::wide_str(strings.models);
        let _ = AppendMenuW(
            menu,
            MF_POPUP,
            models_menu.0 as usize,
            PCWSTR::from_raw(models_label.as_ptr()),
        );

        let limits_menu = CreatePopupMenu().unwrap();
        for (provider, model_label, session_id, weekly_id) in [
            (
                Provider::Claude,
                strings.claude_code_model,
                IDM_CLAUDE_5H,
                IDM_CLAUDE_7D,
            ),
            (
                Provider::Codex,
                strings.codex_model,
                IDM_CODEX_5H,
                IDM_CODEX_7D,
            ),
        ] {
            let provider_menu = CreatePopupMenu().unwrap();
            for (quota, period, id) in [
                (Quota::Session, strings.session_window, session_id),
                (Quota::Weekly, strings.weekly_window, weekly_id),
            ] {
                let mut flags = MENU_ITEM_FLAGS(0);
                if display_limits.shows(provider, quota) {
                    flags |= MF_CHECKED;
                }
                let unavailable = provider == Provider::Codex
                    && codex_windows.is_some_and(|(session, weekly)| match quota {
                        Quota::Session => !session,
                        Quota::Weekly => !weekly,
                    });
                if unavailable || !display_limits.can_toggle(provider, quota) {
                    flags |= MF_GRAYED;
                }
                let label = native_interop::wide_str(period);
                let _ = AppendMenuW(
                    provider_menu,
                    flags,
                    id as usize,
                    PCWSTR::from_raw(label.as_ptr()),
                );
            }
            let label = native_interop::wide_str(model_label);
            let _ = AppendMenuW(
                limits_menu,
                MF_POPUP,
                provider_menu.0 as usize,
                PCWSTR::from_raw(label.as_ptr()),
            );
        }
        let label = native_interop::wide_str(strings.displayed_limits);
        let _ = AppendMenuW(
            menu,
            MF_POPUP,
            limits_menu.0 as usize,
            PCWSTR::from_raw(label.as_ptr()),
        );

        // Appearance submenu: bar style, segments, element toggles, characters
        let appearance_menu = CreatePopupMenu().unwrap();

        let bar_style_menu = CreatePopupMenu().unwrap();
        let current_theme = current_bar_theme();
        for theme in BarTheme::ALL {
            let id = match theme {
                BarTheme::Segmented => IDM_BAR_THEME_SEGMENTED,
                BarTheme::Flat => IDM_BAR_THEME_FLAT,
                BarTheme::Gradient => IDM_BAR_THEME_GRADIENT,
                BarTheme::Pixel => IDM_BAR_THEME_PIXEL,
            };
            let label_str = native_interop::wide_str(theme.label(strings));
            let flags = if theme == current_theme {
                MF_CHECKED
            } else {
                MENU_ITEM_FLAGS(0)
            };
            let _ = AppendMenuW(
                bar_style_menu,
                flags,
                id as usize,
                PCWSTR::from_raw(label_str.as_ptr()),
            );
        }
        let bar_style_label = native_interop::wide_str(strings.bar_style);
        let _ = AppendMenuW(
            appearance_menu,
            MF_POPUP,
            bar_style_menu.0 as usize,
            PCWSTR::from_raw(bar_style_label.as_ptr()),
        );

        let seg_menu = CreatePopupMenu().unwrap();
        let cur_seg = current_segment_count();
        for (id, n) in [
            (IDM_SEG_4, 4),
            (IDM_SEG_6, 6),
            (IDM_SEG_8, 8),
            (IDM_SEG_10, 10),
        ] {
            let label = native_interop::wide_str(&n.to_string());
            let flags = if n == cur_seg {
                MF_CHECKED
            } else {
                MENU_ITEM_FLAGS(0)
            };
            let _ = AppendMenuW(
                seg_menu,
                flags,
                id as usize,
                PCWSTR::from_raw(label.as_ptr()),
            );
        }
        let seg_label = native_interop::wide_str(strings.segment_count);
        let _ = AppendMenuW(
            appearance_menu,
            MF_POPUP,
            seg_menu.0 as usize,
            PCWSTR::from_raw(seg_label.as_ptr()),
        );

        for (id, label, on) in [
            (IDM_TOGGLE_LABELS, strings.show_labels, show_labels()),
            (
                IDM_TOGGLE_PERCENT,
                strings.show_percentages,
                show_percentages(),
            ),
            (
                IDM_TOGGLE_TIMER,
                strings.show_reset_timer,
                show_reset_timer(),
            ),
        ] {
            let label_str = native_interop::wide_str(label);
            let flags = if on { MF_CHECKED } else { MENU_ITEM_FLAGS(0) };
            let _ = AppendMenuW(
                appearance_menu,
                flags,
                id as usize,
                PCWSTR::from_raw(label_str.as_ptr()),
            );
        }

        // Detailed remaining time toggle
        let detailed_str = native_interop::wide_str(strings.show_detailed_remaining);
        let detailed_flags = if show_detailed_remaining() {
            MF_CHECKED
        } else {
            MENU_ITEM_FLAGS(0)
        };
        let _ = AppendMenuW(
            appearance_menu,
            detailed_flags,
            IDM_TOGGLE_DETAILED as usize,
            PCWSTR::from_raw(detailed_str.as_ptr()),
        );

        // Pace indicator toggle (on = Tick)
        let pace_str = native_interop::wide_str(strings.show_pace_indicator);
        let pace_flags = if pace_style() == PaceStyle::Tick {
            MF_CHECKED
        } else {
            MENU_ITEM_FLAGS(0)
        };
        let _ = AppendMenuW(
            appearance_menu,
            pace_flags,
            IDM_PACE_TOGGLE as usize,
            PCWSTR::from_raw(pace_str.as_ptr()),
        );

        let _ = AppendMenuW(appearance_menu, MF_SEPARATOR, 0, PCWSTR::null());

        // Characters submenu (nested under Appearance)
        let characters_menu = CreatePopupMenu().unwrap();
        let show_chars_str = native_interop::wide_str(strings.show_characters);
        let show_chars_flags = if character::is_enabled() {
            MF_CHECKED
        } else {
            MENU_ITEM_FLAGS(0)
        };
        let _ = AppendMenuW(
            characters_menu,
            show_chars_flags,
            IDM_CHAR_SHOW as usize,
            PCWSTR::from_raw(show_chars_str.as_ptr()),
        );
        let _ = AppendMenuW(characters_menu, MF_SEPARATOR, 0, PCWSTR::null());
        let current_kind = character::current_kind();
        for (id, kind, label) in [
            (IDM_CHAR_CAT, CharacterKind::Cat, strings.character_cat),
            (IDM_CHAR_DOG, CharacterKind::Dog, strings.character_dog),
            (IDM_CHAR_GIRL, CharacterKind::Girl, strings.character_girl),
            (IDM_CHAR_BOTH, CharacterKind::Both, strings.character_both),
        ] {
            let label_str = native_interop::wide_str(label);
            let flags = if kind == current_kind {
                MF_CHECKED
            } else {
                MENU_ITEM_FLAGS(0)
            };
            let _ = AppendMenuW(
                characters_menu,
                flags,
                id as usize,
                PCWSTR::from_raw(label_str.as_ptr()),
            );
        }
        let _ = AppendMenuW(characters_menu, MF_SEPARATOR, 0, PCWSTR::null());
        let cat_var = character::cat_variant();
        let cat_color_menu = CreatePopupMenu().unwrap();
        for (id, v, label) in [
            (IDM_CAT_COLOR_0, 0u8, strings.color_orange),
            (IDM_CAT_COLOR_1, 1u8, strings.color_grey),
        ] {
            let label_str = native_interop::wide_str(label);
            let flags = if v == cat_var {
                MF_CHECKED
            } else {
                MENU_ITEM_FLAGS(0)
            };
            let _ = AppendMenuW(
                cat_color_menu,
                flags,
                id as usize,
                PCWSTR::from_raw(label_str.as_ptr()),
            );
        }
        let cat_color_label = native_interop::wide_str(strings.cat_color);
        let _ = AppendMenuW(
            characters_menu,
            MF_POPUP,
            cat_color_menu.0 as usize,
            PCWSTR::from_raw(cat_color_label.as_ptr()),
        );
        let dog_var = character::dog_variant();
        let dog_color_menu = CreatePopupMenu().unwrap();
        for (id, v, label) in [
            (IDM_DOG_COLOR_0, 0u8, strings.color_brown),
            (IDM_DOG_COLOR_1, 1u8, strings.color_black),
        ] {
            let label_str = native_interop::wide_str(label);
            let flags = if v == dog_var {
                MF_CHECKED
            } else {
                MENU_ITEM_FLAGS(0)
            };
            let _ = AppendMenuW(
                dog_color_menu,
                flags,
                id as usize,
                PCWSTR::from_raw(label_str.as_ptr()),
            );
        }
        let dog_color_label = native_interop::wide_str(strings.dog_color);
        let _ = AppendMenuW(
            characters_menu,
            MF_POPUP,
            dog_color_menu.0 as usize,
            PCWSTR::from_raw(dog_color_label.as_ptr()),
        );
        let characters_label = native_interop::wide_str(strings.characters);
        let _ = AppendMenuW(
            appearance_menu,
            MF_POPUP,
            characters_menu.0 as usize,
            PCWSTR::from_raw(characters_label.as_ptr()),
        );

        let appearance_label = native_interop::wide_str(strings.appearance);
        let _ = AppendMenuW(
            menu,
            MF_POPUP,
            appearance_menu.0 as usize,
            PCWSTR::from_raw(appearance_label.as_ptr()),
        );

        // Settings submenu
        let settings_menu = CreatePopupMenu().unwrap();

        let freq_menu = CreatePopupMenu().unwrap();
        for (id, interval, label) in [
            (IDM_FREQ_1MIN, POLL_1_MIN, strings.one_minute),
            (IDM_FREQ_5MIN, POLL_5_MIN, strings.five_minutes),
            (IDM_FREQ_15MIN, POLL_15_MIN, strings.fifteen_minutes),
            (IDM_FREQ_1HOUR, POLL_1_HOUR, strings.one_hour),
        ] {
            let label_str = native_interop::wide_str(label);
            let flags = if interval == current_interval {
                MF_CHECKED
            } else {
                MENU_ITEM_FLAGS(0)
            };
            let _ = AppendMenuW(
                freq_menu,
                flags,
                id as usize,
                PCWSTR::from_raw(label_str.as_ptr()),
            );
        }
        let freq_label = native_interop::wide_str(strings.update_frequency);
        let _ = AppendMenuW(
            settings_menu,
            MF_POPUP,
            freq_menu.0 as usize,
            PCWSTR::from_raw(freq_label.as_ptr()),
        );

        let startup_str = native_interop::wide_str(strings.start_with_windows);
        let startup_flags = if is_startup_enabled() {
            MF_CHECKED
        } else {
            MENU_ITEM_FLAGS(0)
        };
        let _ = AppendMenuW(
            settings_menu,
            startup_flags,
            IDM_START_WITH_WINDOWS as usize,
            PCWSTR::from_raw(startup_str.as_ptr()),
        );

        let reset_pos_str = native_interop::wide_str(strings.reset_position);
        let _ = AppendMenuW(
            settings_menu,
            MENU_ITEM_FLAGS(0),
            IDM_RESET_POSITION as usize,
            PCWSTR::from_raw(reset_pos_str.as_ptr()),
        );

        let language_menu = CreatePopupMenu().unwrap();
        let system_label = native_interop::wide_str(strings.system_default);
        let system_flags = if language_override.is_none() {
            MF_CHECKED
        } else {
            MENU_ITEM_FLAGS(0)
        };
        let _ = AppendMenuW(
            language_menu,
            system_flags,
            IDM_LANG_SYSTEM as usize,
            PCWSTR::from_raw(system_label.as_ptr()),
        );

        for language in LanguageId::ALL {
            let id = match language {
                LanguageId::English => IDM_LANG_ENGLISH,
                LanguageId::Dutch => IDM_LANG_DUTCH,
                LanguageId::Spanish => IDM_LANG_SPANISH,
                LanguageId::French => IDM_LANG_FRENCH,
                LanguageId::German => IDM_LANG_GERMAN,
                LanguageId::Japanese => IDM_LANG_JAPANESE,
                LanguageId::Korean => IDM_LANG_KOREAN,
                LanguageId::TraditionalChinese => IDM_LANG_TRADITIONAL_CHINESE,
            };
            let label_str = native_interop::wide_str(language.native_name());
            let flags = if language_override == Some(language) {
                MF_CHECKED
            } else {
                MENU_ITEM_FLAGS(0)
            };
            let _ = AppendMenuW(
                language_menu,
                flags,
                id as usize,
                PCWSTR::from_raw(label_str.as_ptr()),
            );
        }

        let language_label = native_interop::wide_str(strings.language);
        let _ = AppendMenuW(
            settings_menu,
            MF_POPUP,
            language_menu.0 as usize,
            PCWSTR::from_raw(language_label.as_ptr()),
        );

        let _ = AppendMenuW(settings_menu, MF_SEPARATOR, 0, PCWSTR::null());

        let version_label =
            version_action_label(strings, language, install_channel, &update_status);
        let version_str = native_interop::wide_str(&version_label);
        let version_flags = if matches!(
            update_status,
            UpdateStatus::Checking | UpdateStatus::Applying
        ) {
            MF_GRAYED
        } else {
            MENU_ITEM_FLAGS(0)
        };
        let _ = AppendMenuW(
            settings_menu,
            version_flags,
            IDM_VERSION_ACTION as usize,
            PCWSTR::from_raw(version_str.as_ptr()),
        );

        let settings_label = native_interop::wide_str(strings.settings);
        let _ = AppendMenuW(
            menu,
            MF_POPUP,
            settings_menu.0 as usize,
            PCWSTR::from_raw(settings_label.as_ptr()),
        );

        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());

        let exit_str = native_interop::wide_str(strings.exit);
        let _ = AppendMenuW(
            menu,
            MENU_ITEM_FLAGS(0),
            2,
            PCWSTR::from_raw(exit_str.as_ptr()),
        );

        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let _ = SetForegroundWindow(hwnd);
        let _ = TrackPopupMenu(menu, TPM_RIGHTBUTTON, pt.x, pt.y, 0, hwnd, None);
        let _ = DestroyMenu(menu);
    }
}

fn model_usage_width(segment_count: i32) -> i32 {
    let bar_w = (sc(SEGMENT_W) + sc(SEGMENT_GAP)) * segment_count - sc(SEGMENT_GAP);
    let text_w = text_column_width_logical();
    if text_w > 0 {
        bar_w + sc(BAR_RIGHT_MARGIN) + sc(text_w)
    } else {
        bar_w
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_usage_bar(
    hdc: HDC,
    bar_x: i32,
    y: i32,
    segment_count: i32,
    percent: f64,
    text: &str,
    accent: &Color,
    track: &Color,
    text_color: &Color,
    is_dark: bool,
    expected: Option<f64>,
) {
    let seg_w = sc(SEGMENT_W);
    let seg_h = sc(SEGMENT_H);
    let seg_gap = sc(SEGMENT_GAP);
    let corner_r = sc(CORNER_RADIUS);
    let bar_w = segment_count * (seg_w + seg_gap) - seg_gap;

    let theme = current_bar_theme();
    // Non-default themes color-code the fill by usage threshold; the default
    // keeps the brand accent color so existing behavior is unchanged.
    let fill = if theme.uses_threshold_color() {
        usage_threshold_color(percent, is_dark)
    } else {
        *accent
    };

    match theme {
        BarTheme::Segmented => draw_bar_segmented(
            hdc,
            bar_x,
            y,
            segment_count,
            percent,
            accent,
            track,
            seg_w,
            seg_h,
            seg_gap,
            corner_r,
        ),
        BarTheme::Flat => {
            draw_bar_flat(hdc, bar_x, y, bar_w, seg_h, percent, &fill, track, corner_r)
        }
        BarTheme::Gradient => {
            draw_bar_gradient(hdc, bar_x, y, bar_w, seg_h, percent, &fill, track, corner_r)
        }
        BarTheme::Pixel => draw_bar_pixel(hdc, bar_x, y, bar_w, seg_h, percent, &fill, track),
    }

    // Pace indicator, drawn inside the bar bounds (does not change width).
    if let Some(exp) = expected {
        draw_pace_marker(
            hdc,
            bar_x,
            y,
            bar_w,
            seg_h,
            segment_count,
            percent,
            exp,
            is_dark,
        );
    }

    if !text.is_empty() {
        unsafe {
            let text_x = bar_x + bar_w + sc(BAR_RIGHT_MARGIN);
            let mut text_wide: Vec<u16> = text.encode_utf16().collect();
            let mut text_rect = RECT {
                left: text_x,
                top: y,
                right: text_x + sc(text_column_width_logical()),
                bottom: y + seg_h,
            };
            draw_widget_text(hdc, &mut text_wide, &mut text_rect, text_color);
        }
    }
}

/// One black text pass per coverage canvas; never offset or outline the glyphs.
unsafe fn draw_widget_text(hdc: HDC, text: &mut [u16], rect: &mut RECT, _color: &Color) {
    let _ = SetTextColor(hdc, COLORREF(0));
    let _ = DrawTextW(hdc, text, rect, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
}

/// Fill an axis-aligned rect with a solid color (no-op for an empty rect).
fn fill_solid_rect(hdc: HDC, left: i32, top: i32, right: i32, bottom: i32, color: &Color) {
    if right <= left || bottom <= top {
        return;
    }
    unsafe {
        let rect = RECT {
            left,
            top,
            right,
            bottom,
        };
        let brush = CreateSolidBrush(COLORREF(color.to_colorref()));
        FillRect(hdc, &rect, brush);
        let _ = DeleteObject(brush);
    }
}

/// Draw the pace indicator: a vertical tick snapped to the nearest segment
/// boundary at the expected-pace position. Green when actual usage is behind
/// the expected pace (headroom), red when ahead (at risk).
///
/// The color is derived from the bar's usage-threshold palette (the same green/
/// red used by the threshold-coloured bar themes) and brightened so it stands
/// out; a dark outline keeps it legible against any theme.
#[allow(clippy::too_many_arguments)]
fn draw_pace_marker(
    hdc: HDC,
    bar_x: i32,
    y: i32,
    bar_w: i32,
    bar_h: i32,
    segment_count: i32,
    percent: f64,
    expected: f64,
    is_dark: bool,
) {
    let style = pace_style();
    if style == PaceStyle::Off || segment_count <= 0 {
        return;
    }
    let actual = (percent / 100.0).clamp(0.0, 1.0);
    let exp = expected.clamp(0.0, 1.0);
    let behind = actual <= exp; // behind pace = headroom = green
                                // Reuse the theme's low/high threshold colors, brightened so the pace
                                // region stands out from the normal fill even at the same hue.
    let base = if behind {
        usage_threshold_color(0.0, is_dark)
    } else {
        usage_threshold_color(100.0, is_dark)
    };
    let color = lighten_color(&base, 0.30);
    let outline = Color::from_hex("#0A0A0A");

    let bar_right = bar_x + bar_w;
    let clampx = |v: i32| v.clamp(bar_x, bar_right);

    match style {
        PaceStyle::Tick => {
            // Snap to the nearest segment boundary.
            let seg = (exp * segment_count as f64).round();
            let tick_ratio = seg / segment_count as f64;
            let tick_x = clampx(bar_x + (tick_ratio * bar_w as f64).round() as i32);
            let core = sc(2).max(2);
            let half = core / 2;
            fill_solid_rect(
                hdc,
                clampx(tick_x - half - 1),
                y,
                clampx(tick_x - half - 1 + core + 2),
                y + bar_h,
                &outline,
            );
            fill_solid_rect(
                hdc,
                clampx(tick_x - half),
                y,
                clampx(tick_x - half + core),
                y + bar_h,
                &color,
            );
        }
        PaceStyle::Off => {}
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_bar_segmented(
    hdc: HDC,
    bar_x: i32,
    y: i32,
    segment_count: i32,
    percent: f64,
    accent: &Color,
    track: &Color,
    seg_w: i32,
    seg_h: i32,
    seg_gap: i32,
    corner_r: i32,
) {
    unsafe {
        let percent_clamped = percent.clamp(0.0, 100.0);
        let segment_percent = 100.0 / segment_count as f64;

        for i in 0..segment_count {
            let seg_x = bar_x + i * (seg_w + seg_gap);
            let seg_start = (i as f64) * segment_percent;
            let seg_end = seg_start + segment_percent;

            let seg_rect = RECT {
                left: seg_x,
                top: y,
                right: seg_x + seg_w,
                bottom: y + seg_h,
            };

            if percent_clamped >= seg_end {
                draw_rounded_rect(hdc, &seg_rect, accent, corner_r);
            } else if percent_clamped <= seg_start {
                draw_rounded_rect(hdc, &seg_rect, track, corner_r);
            } else {
                draw_rounded_rect(hdc, &seg_rect, track, corner_r);
                let fraction = (percent_clamped - seg_start) / segment_percent;
                let fill_width = (seg_w as f64 * fraction) as i32;
                if fill_width > 0 {
                    let fill_rect = RECT {
                        left: seg_x,
                        top: y,
                        right: seg_x + fill_width,
                        bottom: y + seg_h,
                    };
                    let rgn = CreateRoundRectRgn(
                        seg_rect.left,
                        seg_rect.top,
                        seg_rect.right + 1,
                        seg_rect.bottom + 1,
                        corner_r * 2,
                        corner_r * 2,
                    );
                    let _ = SelectClipRgn(hdc, rgn);
                    let brush = CreateSolidBrush(COLORREF(accent.to_colorref()));
                    FillRect(hdc, &fill_rect, brush);
                    let _ = DeleteObject(brush);
                    let _ = SelectClipRgn(hdc, HRGN::default());
                    let _ = DeleteObject(rgn);
                }
            }
        }
    }
}

/// Minimal flat: one continuous rounded track with a proportional rounded fill.
#[allow(clippy::too_many_arguments)]
fn draw_bar_flat(
    hdc: HDC,
    bar_x: i32,
    y: i32,
    bar_w: i32,
    bar_h: i32,
    percent: f64,
    fill: &Color,
    track: &Color,
    corner_r: i32,
) {
    unsafe {
        let track_rect = RECT {
            left: bar_x,
            top: y,
            right: bar_x + bar_w,
            bottom: y + bar_h,
        };
        draw_rounded_rect(hdc, &track_rect, track, corner_r);

        let pct = percent.clamp(0.0, 100.0);
        let fill_w = (bar_w as f64 * pct / 100.0).round() as i32;
        if fill_w > 0 {
            let rgn = CreateRoundRectRgn(
                track_rect.left,
                track_rect.top,
                track_rect.right + 1,
                track_rect.bottom + 1,
                corner_r * 2,
                corner_r * 2,
            );
            let _ = SelectClipRgn(hdc, rgn);
            let fill_rect = RECT {
                left: bar_x,
                top: y,
                right: bar_x + fill_w,
                bottom: y + bar_h,
            };
            let brush = CreateSolidBrush(COLORREF(fill.to_colorref()));
            FillRect(hdc, &fill_rect, brush);
            let _ = DeleteObject(brush);
            let _ = SelectClipRgn(hdc, HRGN::default());
            let _ = DeleteObject(rgn);
        }
    }
}

/// Gradient glow: rounded track with the fill drawn as a horizontal gradient
/// from a lighter tint of the threshold color to the full color.
#[allow(clippy::too_many_arguments)]
fn draw_bar_gradient(
    hdc: HDC,
    bar_x: i32,
    y: i32,
    bar_w: i32,
    bar_h: i32,
    percent: f64,
    fill: &Color,
    track: &Color,
    corner_r: i32,
) {
    unsafe {
        let track_rect = RECT {
            left: bar_x,
            top: y,
            right: bar_x + bar_w,
            bottom: y + bar_h,
        };
        draw_rounded_rect(hdc, &track_rect, track, corner_r);

        let pct = percent.clamp(0.0, 100.0);
        let fill_w = (bar_w as f64 * pct / 100.0).round() as i32;
        if fill_w > 0 {
            let rgn = CreateRoundRectRgn(
                track_rect.left,
                track_rect.top,
                track_rect.right + 1,
                track_rect.bottom + 1,
                corner_r * 2,
                corner_r * 2,
            );
            let _ = SelectClipRgn(hdc, rgn);
            let start = lighten_color(fill, 0.5);
            let denom = fill_w.max(1) as f64;
            for col in 0..fill_w {
                let t = col as f64 / denom;
                let c = lerp_color(&start, fill, t);
                let col_rect = RECT {
                    left: bar_x + col,
                    top: y,
                    right: bar_x + col + 1,
                    bottom: y + bar_h,
                };
                let brush = CreateSolidBrush(COLORREF(c.to_colorref()));
                FillRect(hdc, &col_rect, brush);
                let _ = DeleteObject(brush);
            }
            let _ = SelectClipRgn(hdc, HRGN::default());
            let _ = DeleteObject(rgn);
        }
    }
}

/// Retro pixel: a grid of small sharp-cornered squares; columns light up to the
/// usage percentage, the rest stay dim.
fn draw_bar_pixel(
    hdc: HDC,
    bar_x: i32,
    y: i32,
    bar_w: i32,
    bar_h: i32,
    percent: f64,
    fill: &Color,
    track: &Color,
) {
    unsafe {
        let px = sc(3).max(2);
        let gap = sc(1).max(1);
        let step = px + gap;
        let cols = ((bar_w + gap) / step).max(1);
        let rows = ((bar_h + gap) / step).max(1);
        let used_h = rows * step - gap;
        let top0 = y + (bar_h - used_h).max(0) / 2;

        let pct = percent.clamp(0.0, 100.0);
        let filled_cols = (cols as f64 * pct / 100.0).round() as i32;

        let fill_brush = CreateSolidBrush(COLORREF(fill.to_colorref()));
        let track_brush = CreateSolidBrush(COLORREF(track.to_colorref()));
        for cx in 0..cols {
            let on = cx < filled_cols;
            for ry in 0..rows {
                let left = bar_x + cx * step;
                let top = top0 + ry * step;
                let cell = RECT {
                    left,
                    top,
                    right: left + px,
                    bottom: top + px,
                };
                FillRect(hdc, &cell, if on { fill_brush } else { track_brush });
            }
        }
        let _ = DeleteObject(fill_brush);
        let _ = DeleteObject(track_brush);
    }
}

fn lighten_color(c: &Color, t: f64) -> Color {
    let t = t.clamp(0.0, 1.0);
    Color::new(
        (c.r as f64 + (255.0 - c.r as f64) * t).round() as u8,
        (c.g as f64 + (255.0 - c.g as f64) * t).round() as u8,
        (c.b as f64 + (255.0 - c.b as f64) * t).round() as u8,
    )
}

fn lerp_color(a: &Color, b: &Color, t: f64) -> Color {
    let t = t.clamp(0.0, 1.0);
    Color::new(
        (a.r as f64 + (b.r as f64 - a.r as f64) * t).round() as u8,
        (a.g as f64 + (b.g as f64 - a.g as f64) * t).round() as u8,
        (a.b as f64 + (b.b as f64 - a.b as f64) * t).round() as u8,
    )
}

fn draw_rounded_rect(hdc: HDC, rect: &RECT, color: &Color, radius: i32) {
    unsafe {
        let brush = CreateSolidBrush(COLORREF(color.to_colorref()));
        let rgn = CreateRoundRectRgn(
            rect.left,
            rect.top,
            rect.right + 1,
            rect.bottom + 1,
            radius * 2,
            radius * 2,
        );
        let _ = FillRgn(hdc, rgn, brush);
        let _ = DeleteObject(rgn);
        let _ = DeleteObject(brush);
    }
}

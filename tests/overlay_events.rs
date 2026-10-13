//! Exercise actual out-of-process WinEvent delivery without displaying test UI,
//! reading account credentials, or changing the user's foreground application.
#[path = "../src/native_interop.rs"]
#[allow(dead_code)]
mod native_interop;

use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};
use windows::core::PCWSTR;
use windows::Win32::Foundation::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Accessibility::{NotifyWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::WindowsAndMessaging::*;

static EVENTS: AtomicU32 = AtomicU32::new(0);
const CLASS: &str = "CCUMOverlayEventProbe";

unsafe extern "system" fn probe_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

struct Probe(HWND);
impl Probe {
    fn new() -> Self {
        unsafe {
            let instance = GetModuleHandleW(PCWSTR::null()).unwrap();
            let class = native_interop::wide_str(CLASS);
            RegisterClassW(&WNDCLASSW {
                lpfnWndProc: Some(probe_wnd_proc),
                hInstance: HINSTANCE(instance.0),
                lpszClassName: PCWSTR::from_raw(class.as_ptr()),
                ..Default::default()
            });
            let hwnd = CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                PCWSTR::from_raw(class.as_ptr()),
                PCWSTR::null(),
                WS_POPUP,
                100,
                100,
                20,
                20,
                HWND::default(),
                HMENU::default(),
                instance,
                None,
            )
            .unwrap();
            SetLayeredWindowAttributes(hwnd, COLORREF(0), 0, LWA_ALPHA).unwrap();
            Self(hwnd)
        }
    }
    fn emit(&self) {
        unsafe {
            let _ = ShowWindow(self.0, SW_SHOWNOACTIVATE);
            NotifyWinEvent(native_interop::EVENT_OBJECT_REORDER, self.0, 0, 0);
            MoveWindow(self.0, 110, 110, 25, 25, false).unwrap();
            NotifyWinEvent(native_interop::EVENT_OBJECT_LOCATIONCHANGE, self.0, 0, 0);
        }
    }
}
impl Drop for Probe {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.0);
        }
    }
}

unsafe extern "system" fn event_callback(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    _object: i32,
    _child: i32,
    _thread: u32,
    _time: u32,
) {
    let mut class = [0u16; 128];
    let count = GetClassNameW(hwnd, &mut class).max(0) as usize;
    if String::from_utf16_lossy(&class[..count]) != CLASS {
        return;
    }
    let flag = match event {
        native_interop::EVENT_OBJECT_REORDER => 1,
        native_interop::EVENT_OBJECT_LOCATIONCHANGE => 2,
        native_interop::EVENT_OBJECT_SHOW => 4,
        _ => 0,
    };
    EVENTS.fetch_or(flag, Ordering::Relaxed);
}

fn pump(duration: Duration) {
    let start = Instant::now();
    while start.elapsed() < duration {
        unsafe {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, HWND::default(), 0, 0, PM_REMOVE).as_bool() {
                DispatchMessageW(&msg);
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn hooks_deliver_external_geometry_and_reorder_events_without_own_feedback() {
    let hooks = native_interop::set_overlay_event_hooks(event_callback);
    assert_eq!(hooks.len(), 2);
    let foreground_before = unsafe { GetForegroundWindow() };
    let probe = Probe::new();
    probe.emit();
    pump(Duration::from_millis(100));
    assert_eq!(
        EVENTS.load(Ordering::Relaxed),
        0,
        "own SetWindowPos events must not cause refresh loops"
    );
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "external_event_source",
            "--nocapture",
        ])
        .env("CCUM_EVENT_SOURCE", "1")
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    pump(Duration::from_millis(700));
    let status = child.wait().unwrap();
    for hook in hooks {
        native_interop::unhook_win_event(hook);
    }
    assert!(status.success());
    assert_eq!(
        EVENTS.load(Ordering::Relaxed) & 3,
        3,
        "external reorder and fullscreen geometry events must arrive"
    );
    assert_eq!(
        unsafe { GetForegroundWindow() },
        foreground_before,
        "probes must not take focus"
    );
}

#[test]
#[ignore = "subprocess helper invoked by the event-delivery test"]
fn external_event_source() {
    if std::env::var("CCUM_EVENT_SOURCE").as_deref() != Ok("1") {
        return;
    }
    let probe = Probe::new();
    probe.emit();
    pump(Duration::from_millis(200));
}

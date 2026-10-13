//! Keep shell activation separate from fullscreen application visibility.
#[derive(Clone, Copy, Debug)]
pub struct Bounds {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HideReason {
    UserHidden,
    TaskbarHidden,
    FullscreenApplication,
}

pub fn is_shell_class(class: &str) -> bool {
    matches!(
        class,
        "Shell_TrayWnd"
            | "Shell_SecondaryTrayWnd"
            | "Progman"
            | "WorkerW"
            | "XamlExplorerHostIslandWindow"
            | "MultitaskingViewFrame"
            | "#32768"
    )
}

/// Use the client area: maximized window frames can extend beyond the monitor,
/// but their title bar and taskbar still leave the client smaller than fullscreen.
pub fn is_fullscreen_client(class: &str, client: Bounds, monitor: Bounds) -> bool {
    !is_shell_class(class)
        && client.left <= monitor.left + 1
        && client.top <= monitor.top + 1
        && client.right >= monitor.right - 1
        && client.bottom >= monitor.bottom - 1
}

pub fn hide_reason(
    preferred: bool,
    taskbar_visible: bool,
    taskbar: Bounds,
    monitor: Bounds,
    fullscreen_application: bool,
) -> Option<HideReason> {
    if !preferred {
        return Some(HideReason::UserHidden);
    }
    if fullscreen_application {
        return Some(HideReason::FullscreenApplication);
    }
    let visible_width = (taskbar.right.min(monitor.right) - taskbar.left.max(monitor.left)).max(0);
    let visible_height = (taskbar.bottom.min(monitor.bottom) - taskbar.top.max(monitor.top)).max(0);
    let width = (taskbar.right - taskbar.left).max(1);
    let height = (taskbar.bottom - taskbar.top).max(1);
    if !taskbar_visible || visible_width * 2 < width || visible_height * 2 < height {
        return Some(HideReason::TaskbarHidden);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    const SCREEN: Bounds = Bounds {
        left: 0,
        top: 0,
        right: 1920,
        bottom: 1080,
    };
    const BAR: Bounds = Bounds {
        left: 0,
        top: 1032,
        right: 1920,
        bottom: 1080,
    };

    #[test]
    fn visible_taskbar_keeps_the_widget_shown() {
        assert_eq!(hide_reason(true, true, BAR, SCREEN, false), None);
        let second_screen = Bounds {
            left: 1920,
            right: 3840,
            ..SCREEN
        };
        let second_bar = Bounds {
            left: 1920,
            right: 3840,
            ..BAR
        };
        assert_eq!(
            hide_reason(true, true, second_bar, second_screen, false),
            None
        );
    }

    #[test]
    fn auto_hide_recovers_as_the_taskbar_returns_onscreen() {
        let parked = Bounds {
            top: 1078,
            bottom: 1126,
            ..BAR
        };
        assert_eq!(
            hide_reason(true, true, parked, SCREEN, false),
            Some(HideReason::TaskbarHidden)
        );
        assert_eq!(hide_reason(true, true, BAR, SCREEN, false), None);
    }

    #[test]
    fn user_hidden_preference_is_preserved_after_resume() {
        assert_eq!(
            hide_reason(false, true, BAR, SCREEN, false),
            Some(HideReason::UserHidden)
        );
        assert_eq!(
            hide_reason(true, false, BAR, SCREEN, false),
            Some(HideReason::TaskbarHidden)
        );
    }

    #[test]
    fn fullscreen_hides_and_exit_restores_without_changing_preference() {
        assert!(is_fullscreen_client("Chrome_WidgetWin_1", SCREEN, SCREEN));
        assert_eq!(
            hide_reason(true, true, BAR, SCREEN, true),
            Some(HideReason::FullscreenApplication)
        );
        assert_eq!(hide_reason(true, true, BAR, SCREEN, false), None);
        assert_eq!(
            hide_reason(false, true, BAR, SCREEN, true),
            Some(HideReason::UserHidden)
        );
    }

    #[test]
    fn maximized_client_and_shell_windows_do_not_hide_widget() {
        let maximized_client = Bounds {
            top: 31,
            bottom: BAR.top,
            ..SCREEN
        };
        assert!(!is_fullscreen_client(
            "Chrome_WidgetWin_1",
            maximized_client,
            SCREEN
        ));
        for class in [
            "Progman",
            "WorkerW",
            "Shell_TrayWnd",
            "Shell_SecondaryTrayWnd",
            "XamlExplorerHostIslandWindow",
            "MultitaskingViewFrame",
            "#32768",
        ] {
            assert!(!is_fullscreen_client(class, SCREEN, SCREEN), "{class}");
        }
        // UWP applications must still be eligible; their class is also used by games.
        assert!(is_fullscreen_client(
            "Windows.UI.Core.CoreWindow",
            SCREEN,
            SCREEN
        ));
    }

    #[test]
    fn fullscreen_geometry_is_monitor_specific_and_allows_negative_coordinates() {
        let other = Bounds {
            left: -1920,
            right: 0,
            ..SCREEN
        };
        assert!(!is_fullscreen_client("GameWindow", other, SCREEN));
        assert!(is_fullscreen_client("GameWindow", other, other));
    }
}

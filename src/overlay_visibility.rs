//! Show with the taskbar; foreground-window geometry must not toggle the widget.
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
}

pub fn hide_reason(
    preferred: bool,
    taskbar_visible: bool,
    taskbar: Bounds,
    monitor: Bounds,
) -> Option<HideReason> {
    if !preferred {
        return Some(HideReason::UserHidden);
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
        // Foreground activation is deliberately absent from the visibility API:
        // a taskbar click cannot classify a maximized or shell window as fullscreen.
        assert_eq!(hide_reason(true, true, BAR, SCREEN), None);
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
        assert_eq!(hide_reason(true, true, second_bar, second_screen), None);
    }

    #[test]
    fn auto_hide_recovers_as_the_taskbar_returns_onscreen() {
        let parked = Bounds {
            top: 1078,
            bottom: 1126,
            ..BAR
        };
        assert_eq!(
            hide_reason(true, true, parked, SCREEN),
            Some(HideReason::TaskbarHidden)
        );
        assert_eq!(hide_reason(true, true, BAR, SCREEN), None);
    }

    #[test]
    fn user_hidden_preference_is_preserved_after_resume() {
        assert_eq!(
            hide_reason(false, true, BAR, SCREEN),
            Some(HideReason::UserHidden)
        );
        assert_eq!(
            hide_reason(true, false, BAR, SCREEN),
            Some(HideReason::TaskbarHidden)
        );
    }
}

//! Project provider statuses into one stable application tray entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayIconKind {
    Claude,
    Codex,
}

pub struct TrayIconData {
    pub kind: TrayIconKind,
    pub percent: Option<f64>,
    pub tooltip: String,
}

pub fn combined_icon(providers: &[TrayIconData]) -> Option<TrayIconData> {
    if providers.is_empty() {
        return None;
    }
    Some(TrayIconData {
        // Use the embedded application icon, with no misleading combined percentage.
        kind: TrayIconKind::Claude,
        percent: None,
        tooltip: providers
            .iter()
            .map(|icon| icon.tooltip.as_str())
            .collect::<Vec<_>>()
            .join("\n"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_models_share_one_app_icon_and_keep_both_statuses() {
        let providers = [
            TrayIconData {
                kind: TrayIconKind::Claude,
                percent: Some(10.0),
                tooltip: "Claude 7d: 10%".into(),
            },
            TrayIconData {
                kind: TrayIconKind::Codex,
                percent: Some(40.0),
                tooltip: "Codex 7d: 40%".into(),
            },
        ];
        let icon = combined_icon(&providers).unwrap();
        assert_eq!(icon.kind, TrayIconKind::Claude);
        assert_eq!(icon.percent, None);
        assert_eq!(icon.tooltip, "Claude 7d: 10%\nCodex 7d: 40%");
    }

    #[test]
    fn codex_only_keeps_the_same_app_icon_identity() {
        let icon = combined_icon(&[TrayIconData {
            kind: TrayIconKind::Codex,
            percent: None,
            tooltip: "Codex: signing in".into(),
        }])
        .unwrap();
        assert_eq!(icon.kind, TrayIconKind::Claude);
        assert_eq!(icon.tooltip, "Codex: signing in");
        assert!(combined_icon(&[]).is_none());
    }
}

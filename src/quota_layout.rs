//! Display preferences and layout planning, independent of the Windows renderer.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    Claude,
    Codex,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Quota {
    Session,
    Weekly,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct DisplayLimits {
    #[serde(default = "default_true")]
    pub show_claude_5h: bool,
    #[serde(default = "default_true")]
    pub show_claude_7d: bool,
    #[serde(default = "default_true")]
    pub show_codex_5h: bool,
    #[serde(default = "default_true")]
    pub show_codex_7d: bool,
}

fn default_true() -> bool {
    true
}

impl Default for DisplayLimits {
    fn default() -> Self {
        Self {
            show_claude_5h: true,
            show_claude_7d: true,
            show_codex_5h: true,
            show_codex_7d: true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BarSlot {
    pub provider: Provider,
    pub quota: Quota,
    pub column: usize,
    pub row: usize,
}

impl DisplayLimits {
    pub fn shows(self, provider: Provider, quota: Quota) -> bool {
        match (provider, quota) {
            (Provider::Claude, Quota::Session) => self.show_claude_5h,
            (Provider::Claude, Quota::Weekly) => self.show_claude_7d,
            (Provider::Codex, Quota::Session) => self.show_codex_5h,
            (Provider::Codex, Quota::Weekly) => self.show_codex_7d,
        }
    }

    /// A sole real Codex window takes precedence over stale display preferences.
    /// Keep preferences unchanged so they apply again if both windows return.
    pub fn with_codex_windows(mut self, available: Option<(bool, bool)>) -> Self {
        if let Some((session, weekly)) = available {
            self.show_codex_5h &= session;
            self.show_codex_7d &= weekly;
            if !self.show_codex_5h && !self.show_codex_7d {
                self.show_codex_5h = session;
                self.show_codex_7d = weekly;
            }
        }
        self
    }

    pub fn badge_quota(self, provider: Provider) -> Quota {
        if self.shows(provider, Quota::Session) {
            Quota::Session
        } else {
            Quota::Weekly
        }
    }

    /// Keep at least one quota selected per provider, including disabled providers.
    pub fn normalize(&mut self) {
        if !self.show_claude_5h && !self.show_claude_7d {
            self.show_claude_5h = true;
        }
        if !self.show_codex_5h && !self.show_codex_7d {
            self.show_codex_5h = true;
        }
    }

    pub fn can_toggle(self, provider: Provider, quota: Quota) -> bool {
        let other = match quota {
            Quota::Session => Quota::Weekly,
            Quota::Weekly => Quota::Session,
        };
        !self.shows(provider, quota) || self.shows(provider, other)
    }

    pub fn toggle(&mut self, provider: Provider, quota: Quota) {
        if !self.can_toggle(provider, quota) {
            return;
        }
        let selected = match (provider, quota) {
            (Provider::Claude, Quota::Session) => &mut self.show_claude_5h,
            (Provider::Claude, Quota::Weekly) => &mut self.show_claude_7d,
            (Provider::Codex, Quota::Session) => &mut self.show_codex_5h,
            (Provider::Codex, Quota::Weekly) => &mut self.show_codex_7d,
        };
        *selected = !*selected;
    }

    pub fn stacked(self, claude: bool, codex: bool) -> bool {
        claude
            && codex
            && (self.show_claude_5h ^ self.show_claude_7d)
            && (self.show_codex_5h ^ self.show_codex_7d)
    }

    pub fn columns(self, claude: bool, codex: bool) -> i32 {
        if self.stacked(claude, codex) {
            1
        } else {
            (claude as i32 + codex as i32).max(1)
        }
    }

    pub fn slots(self, claude: bool, codex: bool) -> Vec<BarSlot> {
        let stacked = self.stacked(claude, codex);
        let mut slots = Vec::new();
        let mut index = 0;
        for (provider, enabled) in [(Provider::Claude, claude), (Provider::Codex, codex)] {
            if !enabled {
                continue;
            }
            for (row, quota) in [Quota::Session, Quota::Weekly].into_iter().enumerate() {
                if self.shows(provider, quota) {
                    slots.push(BarSlot {
                        provider,
                        quota,
                        column: if stacked { 0 } else { index },
                        row: if stacked { index } else { row },
                    });
                }
            }
            index += 1;
        }
        slots
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_settings_keep_all_quotas_and_round_trip() {
        let limits: DisplayLimits = serde_json::from_str("{}").unwrap();
        assert_eq!(limits.slots(true, true).len(), 4);
        let limits = DisplayLimits {
            show_claude_5h: false,
            show_codex_5h: false,
            ..limits
        };
        let restored: DisplayLimits =
            serde_json::from_str(&serde_json::to_string(&limits).unwrap()).unwrap();
        assert!(!restored.show_claude_5h);
        assert!(!restored.show_codex_5h);
        assert!(restored.show_claude_7d && restored.show_codex_7d);
    }

    #[test]
    fn weekly_only_codex_overrides_a_stale_5h_selection_without_saving_it() {
        let saved = DisplayLimits {
            show_claude_5h: false,
            show_codex_7d: false,
            ..Default::default()
        };
        let effective = saved.with_codex_windows(Some((false, true)));
        assert!(!effective.show_codex_5h);
        assert!(effective.show_codex_7d);
        assert_eq!(effective.badge_quota(Provider::Codex), Quota::Weekly);
        assert!(effective.stacked(true, true));
        assert_eq!(effective.slots(true, true)[1].quota, Quota::Weekly);
        assert!(saved.show_codex_5h);
        assert!(!saved.show_codex_7d);
        assert!(!saved.with_codex_windows(Some((true, true))).show_codex_7d);
    }

    #[test]
    fn flattened_legacy_settings_and_weekly_badges() {
        #[derive(Serialize, Deserialize)]
        struct Settings {
            show_codex: bool,
            #[serde(flatten)]
            limits: DisplayLimits,
        }
        let old: Settings = serde_json::from_str(r#"{"show_codex":true}"#).unwrap();
        assert!(old.show_codex);
        assert_eq!(old.limits.slots(true, true).len(), 4);
        let weekly: Settings = serde_json::from_str(
            r#"{"show_codex":true,"show_claude_5h":false,"show_codex_5h":false}"#,
        )
        .unwrap();
        assert!(weekly.limits.stacked(true, true));
        assert_eq!(weekly.limits.badge_quota(Provider::Claude), Quota::Weekly);
        assert_eq!(weekly.limits.badge_quota(Provider::Codex), Quota::Weekly);
        let restored: Settings =
            serde_json::from_str(&serde_json::to_string(&weekly).unwrap()).unwrap();
        assert!(restored.limits.stacked(true, true));
    }

    #[test]
    fn every_single_quota_pair_stacks_claude_above_codex() {
        for cc_session in [false, true] {
            for cx_session in [false, true] {
                let limits = DisplayLimits {
                    show_claude_5h: cc_session,
                    show_claude_7d: !cc_session,
                    show_codex_5h: cx_session,
                    show_codex_7d: !cx_session,
                };
                let slots = limits.slots(true, true);
                assert_eq!(limits.columns(true, true), 1);
                assert_eq!(
                    slots,
                    vec![
                        BarSlot {
                            provider: Provider::Claude,
                            quota: if cc_session {
                                Quota::Session
                            } else {
                                Quota::Weekly
                            },
                            column: 0,
                            row: 0
                        },
                        BarSlot {
                            provider: Provider::Codex,
                            quota: if cx_session {
                                Quota::Session
                            } else {
                                Quota::Weekly
                            },
                            column: 0,
                            row: 1
                        },
                    ]
                );
            }
        }
    }

    #[test]
    fn mixed_layout_keeps_provider_columns_and_quota_rows() {
        let limits = DisplayLimits {
            show_claude_5h: false,
            ..Default::default()
        };
        assert_eq!(limits.columns(true, true), 2);
        assert_eq!(
            limits.slots(true, true),
            vec![
                BarSlot {
                    provider: Provider::Claude,
                    quota: Quota::Weekly,
                    column: 0,
                    row: 1
                },
                BarSlot {
                    provider: Provider::Codex,
                    quota: Quota::Session,
                    column: 1,
                    row: 0
                },
                BarSlot {
                    provider: Provider::Codex,
                    quota: Quota::Weekly,
                    column: 1,
                    row: 1
                },
            ]
        );
        let slots = limits.slots(false, true);
        assert_eq!(slots.len(), 2);
        assert!(slots
            .iter()
            .all(|s| s.provider == Provider::Codex && s.column == 0));
        assert_eq!(limits.slots(true, false).len(), 1);
    }

    #[test]
    fn normalization_and_toggles_never_hide_a_providers_last_quota() {
        let mut limits = DisplayLimits {
            show_claude_5h: false,
            show_claude_7d: false,
            show_codex_5h: false,
            show_codex_7d: false,
        };
        limits.normalize();
        for provider in [Provider::Claude, Provider::Codex] {
            limits.toggle(provider, Quota::Session);
            assert!(limits.shows(provider, Quota::Session));
            limits.toggle(provider, Quota::Weekly);
            limits.toggle(provider, Quota::Session);
            assert!(!limits.shows(provider, Quota::Session));
            limits.toggle(provider, Quota::Weekly);
            assert!(limits.shows(provider, Quota::Weekly));
        }
    }
}

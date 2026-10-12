//! Codex window positions do not determine their quota periods.
use std::time::{Duration, UNIX_EPOCH};

use serde::Deserialize;

use crate::models::{UsageData, UsageSection};

#[derive(Deserialize)]
pub struct CodexUsageResponse {
    rate_limit: Option<CodexRateLimitDetails>,
}

#[derive(Deserialize)]
struct CodexRateLimitDetails {
    primary_window: Option<CodexRateLimitWindow>,
    secondary_window: Option<CodexRateLimitWindow>,
}

#[derive(Deserialize)]
struct CodexRateLimitWindow {
    used_percent: f64,
    reset_at: i64,
    limit_window_seconds: Option<u64>,
}

pub fn codex_usage_from_response(response: CodexUsageResponse) -> Option<UsageData> {
    let details = response.rate_limit?;
    let mut data = UsageData::default();
    let mut available = (false, false);
    for (index, window) in [details.primary_window, details.secondary_window]
        .into_iter()
        .enumerate()
    {
        let Some(window) = window else { continue };
        let weekly = match window.limit_window_seconds {
            Some(18_000) => false,
            Some(604_800) => true,
            // Older responses omitted duration; preserve their positional mapping.
            None => index == 1,
            // Unknown periods must not be mislabeled as 5h or 7d.
            Some(_) => continue,
        };
        let section = UsageSection {
            percentage: window.used_percent,
            resets_at: u64::try_from(window.reset_at)
                .ok()
                .map(|secs| UNIX_EPOCH + Duration::from_secs(secs)),
        };
        if weekly {
            data.weekly = section;
            available.1 = true;
        } else {
            data.session = section;
            available.0 = true;
        }
    }
    if !available.0 && !available.1 {
        return None;
    }
    data.available_windows = Some(available);
    Some(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> Option<UsageData> {
        codex_usage_from_response(serde_json::from_str(json).unwrap())
    }

    #[test]
    fn weekly_primary_is_weekly_even_without_a_secondary_window() {
        let data = parse(r#"{"rate_limit":{"primary_window":{"used_percent":37,"reset_at":1800000000,"limit_window_seconds":604800},"secondary_window":null}}"#).unwrap();
        assert_eq!(data.weekly.percentage, 37.0);
        assert_eq!(data.session.percentage, 0.0);
        assert_eq!(
            data.weekly.resets_at,
            Some(UNIX_EPOCH + Duration::from_secs(1800000000))
        );
        assert_eq!(data.available_windows, Some((false, true)));
    }

    #[test]
    fn duration_identifies_both_periods_regardless_of_position() {
        let data = parse(r#"{"rate_limit":{"primary_window":{"used_percent":64,"reset_at":1800000000,"limit_window_seconds":604800},"secondary_window":{"used_percent":12,"reset_at":1799990000,"limit_window_seconds":18000}}}"#).unwrap();
        assert_eq!(data.session.percentage, 12.0);
        assert_eq!(data.weekly.percentage, 64.0);
        assert_eq!(data.available_windows, Some((true, true)));
        let standard = parse(r#"{"rate_limit":{"primary_window":{"used_percent":12,"reset_at":1799990000,"limit_window_seconds":18000},"secondary_window":{"used_percent":64,"reset_at":1800000000,"limit_window_seconds":604800}}}"#).unwrap();
        assert_eq!(standard.session.percentage, 12.0);
        assert_eq!(standard.weekly.percentage, 64.0);
    }

    #[test]
    fn null_unknown_and_legacy_windows_do_not_fabricate_quotas() {
        assert!(parse(r#"{"rate_limit":null}"#).is_none());
        assert!(parse(r#"{"rate_limit":{"primary_window":null}}"#).is_none());
        assert!(parse(r#"{"rate_limit":{"primary_window":{"used_percent":10,"reset_at":1800000000,"limit_window_seconds":3600}}}"#).is_none());
        let legacy = parse(r#"{"rate_limit":{"primary_window":{"used_percent":12,"reset_at":1799990000},"secondary_window":{"used_percent":64,"reset_at":1800000000}}}"#).unwrap();
        assert_eq!(legacy.session.percentage, 12.0);
        assert_eq!(legacy.weekly.percentage, 64.0);
    }
}

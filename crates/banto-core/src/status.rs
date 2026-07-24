//! Pure mtime-to-age-bucket classification — the activity indicator's
//! bucketing math only. Live-process/PID state (`ProcessProbe`,
//! `LiveSession`, `classify`) lives in `banto_io::status`: it needs
//! `sysinfo` and reads `<claude_home>/sessions/*.json`, both forbidden here
//! (`docs/DISCIPLINE.md` §2).

use std::time::{Duration, SystemTime};

use crate::model::AgeBucket;

/// Cut-off ages for the [`AgeBucket`] classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgeThresholds {
    /// Ages strictly below this are [`AgeBucket::Today`].
    pub today: Duration,
    /// Ages strictly below this (but not below `today`) are
    /// [`AgeBucket::ThisWeek`].
    pub week: Duration,
}

impl Default for AgeThresholds {
    /// 24 hours / 7 days.
    fn default() -> Self {
        Self {
            today: Duration::from_secs(24 * 60 * 60),
            week: Duration::from_secs(7 * 24 * 60 * 60),
        }
    }
}

/// Bucket `mtime` by its age relative to `now`.
///
/// Pure function: age < `today` maps to [`AgeBucket::Today`], age < `week`
/// maps to [`AgeBucket::ThisWeek`], anything else to [`AgeBucket::Older`].
/// An mtime in the future (e.g. clock skew) counts as [`AgeBucket::Today`].
pub fn age_bucket(mtime: SystemTime, now: SystemTime, thresholds: &AgeThresholds) -> AgeBucket {
    let Ok(age) = now.duration_since(mtime) else {
        // mtime is in the future: treat as the freshest bucket.
        return AgeBucket::Today;
    };
    if age < thresholds.today {
        AgeBucket::Today
    } else if age < thresholds.week {
        AgeBucket::ThisWeek
    } else {
        AgeBucket::Older
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECOND: Duration = Duration::from_secs(1);

    fn now() -> SystemTime {
        // A fixed anchor keeps the tests deterministic; the absolute value is
        // irrelevant because age_bucket only looks at the difference.
        SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000_000)
    }

    #[test]
    fn default_thresholds_are_24h_and_7d() {
        let t = AgeThresholds::default();
        assert_eq!(t.today, Duration::from_secs(86_400));
        assert_eq!(t.week, Duration::from_secs(604_800));
    }

    #[test]
    fn just_under_today_threshold_is_today() {
        let t = AgeThresholds::default();
        let mtime = now() - (t.today - SECOND);
        assert_eq!(age_bucket(mtime, now(), &t), AgeBucket::Today);
    }

    #[test]
    fn exactly_today_threshold_is_this_week() {
        let t = AgeThresholds::default();
        let mtime = now() - t.today;
        assert_eq!(age_bucket(mtime, now(), &t), AgeBucket::ThisWeek);
    }

    #[test]
    fn just_over_today_threshold_is_this_week() {
        let t = AgeThresholds::default();
        let mtime = now() - (t.today + SECOND);
        assert_eq!(age_bucket(mtime, now(), &t), AgeBucket::ThisWeek);
    }

    #[test]
    fn just_under_week_threshold_is_this_week() {
        let t = AgeThresholds::default();
        let mtime = now() - (t.week - SECOND);
        assert_eq!(age_bucket(mtime, now(), &t), AgeBucket::ThisWeek);
    }

    #[test]
    fn exactly_week_threshold_is_older() {
        let t = AgeThresholds::default();
        let mtime = now() - t.week;
        assert_eq!(age_bucket(mtime, now(), &t), AgeBucket::Older);
    }

    #[test]
    fn well_over_week_threshold_is_older() {
        let t = AgeThresholds::default();
        let mtime = now() - Duration::from_secs(30 * 24 * 60 * 60);
        assert_eq!(age_bucket(mtime, now(), &t), AgeBucket::Older);
    }

    #[test]
    fn future_mtime_is_today() {
        let t = AgeThresholds::default();
        let mtime = now() + Duration::from_secs(3600);
        assert_eq!(age_bucket(mtime, now(), &t), AgeBucket::Today);
    }

    #[test]
    fn zero_age_is_today() {
        let t = AgeThresholds::default();
        assert_eq!(age_bucket(now(), now(), &t), AgeBucket::Today);
    }

    #[test]
    fn custom_thresholds_are_respected() {
        let t = AgeThresholds {
            today: Duration::from_secs(60),
            week: Duration::from_secs(600),
        };
        assert_eq!(
            age_bucket(now() - Duration::from_secs(59), now(), &t),
            AgeBucket::Today
        );
        assert_eq!(
            age_bucket(now() - Duration::from_secs(60), now(), &t),
            AgeBucket::ThisWeek
        );
        assert_eq!(
            age_bucket(now() - Duration::from_secs(600), now(), &t),
            AgeBucket::Older
        );
    }
}

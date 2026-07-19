//! Live-session state and activity classification.
//!
//! Sources, in priority order (docs/REQUIREMENTS.md "アクティビティインジケーター"):
//! 1. `<claude_home>/sessions/<pid>.json` + PID alive + status=busy -> Busy
//! 2. PID alive, not busy -> Alive
//! 3. otherwise bucket the session file mtime into Today / ThisWeek / Older
//!
//! PID liveness must sit behind a trait so tests can mock it (no real
//! processes in tests). Bucketing must be a pure function of
//! (mtime, now, thresholds).
//!
//! TODO(status agent): implement.

//! Timestamped output for the daemon's own log lines.
//!
//! The supervisor appends the daemon's stderr to its log file without stamping
//! lines, so the gateway prefixes its own messages with an RFC3339 UTC time.

/// Current time as RFC3339 UTC with millisecond precision.
pub fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

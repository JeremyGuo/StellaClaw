use std::time::{SystemTime, UNIX_EPOCH};

use time::{format_description::well_known::Rfc3339, OffsetDateTime};

pub(super) fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

pub(super) fn unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

pub(super) fn generated_platform_id() -> String {
    format!("web-{}", unix_millis())
}

pub(super) fn generated_request_id(prefix: &str) -> String {
    format!("{prefix}-{}", unix_millis())
}

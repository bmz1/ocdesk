pub mod app_view;
pub mod composer;
pub mod conversation;
pub mod message;
pub mod sidebar;

/// "now", "5m", "3h", "2d", "Mar 4". `ms` is a Unix epoch timestamp in ms.
pub fn relative_time(ms: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(ms);
    let secs = now.saturating_sub(ms) / 1000;
    match secs {
        0..60 => "now".into(),
        60..3600 => format!("{}m", secs / 60),
        3600..86_400 => format!("{}h", secs / 3600),
        86_400..604_800 => format!("{}d", secs / 86_400),
        _ => format!("{}w", secs / 604_800),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn relative_time_buckets() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        assert_eq!(super::relative_time(now), "now");
        assert_eq!(super::relative_time(now - 5 * 60_000), "5m");
        assert_eq!(super::relative_time(now - 3 * 3_600_000), "3h");
        assert_eq!(super::relative_time(now - 2 * 86_400_000), "2d");
    }
}

use crate::ui;
use std::path::Path;

#[derive(serde::Serialize, serde::Deserialize)]
struct Cache {
    last_checked: String,
}

/// Whether at least `interval` has elapsed since the last recorded check at
/// `path`. Treats a missing path, unreadable/malformed cache file, or
/// unparseable timestamp as "never checked" so callers default to running
/// the check rather than silently skipping it.
pub fn is_due(path: Option<&Path>, interval: chrono::Duration) -> bool {
    let Some(path) = path else {
        return true;
    };

    let Ok(contents) = std::fs::read_to_string(path) else {
        return true;
    };

    let Ok(cache) = serde_json::from_str::<Cache>(&contents) else {
        return true;
    };

    let Ok(last_checked) = chrono::DateTime::parse_from_rfc3339(&cache.last_checked) else {
        return true;
    };

    chrono::Utc::now().signed_duration_since(last_checked) >= interval
}

/// Records that a check happened now at `path`, creating parent directories
/// as needed. Best-effort: failures are logged at debug level and ignored,
/// since a missed cache write only means the next check runs a bit sooner.
pub fn record(path: Option<&Path>) {
    let Some(path) = path else {
        return;
    };

    let Some(parent) = path.parent() else {
        return;
    };

    if let Err(e) = std::fs::create_dir_all(parent) {
        ui::debug(format!(
            "Could not create throttle cache dir {:?}: {}",
            parent, e
        ));
        return;
    }

    let cache = Cache {
        last_checked: chrono::Utc::now().to_rfc3339(),
    };

    match serde_json::to_string(&cache) {
        Ok(json) => {
            if let Err(e) = std::fs::write(path, json) {
                ui::debug(format!("Could not write throttle cache {:?}: {}", path, e));
            }
        }
        Err(e) => ui::debug(format!("Could not serialize throttle cache: {}", e)),
    }
}

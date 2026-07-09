use crate::ui;
use crate::utils;
use anyhow::Result;
use inquire::Confirm;
use std::fs;
use std::path::Path;
use std::time::SystemTime;

pub fn execute(
    project_dir: &Path,
    keep: Option<usize>,
    older_than_days: Option<u64>,
    yes: bool,
    dry_run: bool,
) -> Result<()> {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)?
        .as_secs();

    let mut backups = find_backups(project_dir)?;
    if backups.is_empty() {
        ui::info("No backup folders found.");
        return Ok(());
    }

    backups.sort_by(|a, b| b.1.cmp(&a.1));

    let candidates = select_candidates(&backups, keep, older_than_days, now);
    if candidates.is_empty() {
        ui::info("Nothing to clean up.");
        return Ok(());
    }

    for (name, timestamp) in &candidates {
        let age_days = now.saturating_sub(*timestamp) / 86400;
        ui::info(format!("{} ({} days old)", name, age_days));
    }

    if dry_run {
        ui::info(format!(
            "{} backup folder(s) would be removed.",
            candidates.len()
        ));
        return Ok(());
    }

    if !yes
        && !Confirm::new(&format!("Remove {} backup folder(s)?", candidates.len()))
            .with_default(false)
            .prompt()?
    {
        ui::info("Aborted.");
        return Ok(());
    }

    let mut removed = 0;
    for (name, _) in &candidates {
        let backup_dir = project_dir.join(name);
        match fs::remove_dir_all(&backup_dir) {
            Ok(()) => {
                if let Err(e) = utils::remove_phpstorm_exclude(project_dir, name) {
                    ui::warning(format!(
                        "Failed to remove PhpStorm exclude entry for {}: {}",
                        name, e
                    ));
                }
                removed += 1;
            }
            Err(e) => {
                ui::warning(format!("Failed to remove {}: {}", name, e));
            }
        }
    }

    ui::success(format!(
        "Removed {} of {} backup folder(s).",
        removed,
        candidates.len()
    ));

    Ok(())
}

fn find_backups(project_dir: &Path) -> Result<Vec<(String, u64)>> {
    let mut backups = Vec::new();

    for entry in fs::read_dir(project_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }

        let name = entry.file_name().to_string_lossy().to_string();
        if let Some(suffix) = name.strip_prefix("backup_")
            && let Ok(timestamp) = suffix.parse::<u64>()
        {
            backups.push((name, timestamp));
        }
    }

    Ok(backups)
}

fn select_candidates(
    backups: &[(String, u64)],
    keep: Option<usize>,
    older_than_days: Option<u64>,
    now: u64,
) -> Vec<(String, u64)> {
    if let Some(days) = older_than_days {
        let cutoff = now.saturating_sub(days * 86400);
        backups
            .iter()
            .filter(|(_, ts)| *ts < cutoff)
            .cloned()
            .collect()
    } else {
        let effective_keep = keep.unwrap_or(5);
        backups.iter().skip(effective_keep).cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keep_selects_oldest_beyond_count() {
        let backups = vec![
            ("backup_300".to_string(), 300),
            ("backup_200".to_string(), 200),
            ("backup_100".to_string(), 100),
        ];
        let candidates = select_candidates(&backups, Some(1), None, 400);
        assert_eq!(
            candidates,
            vec![
                ("backup_200".to_string(), 200),
                ("backup_100".to_string(), 100),
            ]
        );
    }

    #[test]
    fn keep_defaults_to_five() {
        let backups: Vec<(String, u64)> = (0..7)
            .map(|i| (format!("backup_{}", i * 100), i * 100))
            .collect();
        let candidates = select_candidates(&backups, None, None, 1000);
        assert_eq!(candidates.len(), 2);
    }

    #[test]
    fn older_than_selects_entries_past_cutoff() {
        let backups = vec![
            ("backup_100".to_string(), 100),
            ("backup_500000".to_string(), 500_000),
        ];
        // now = 1_000_000, 10 days = 864_000s, cutoff = 136_000
        let candidates = select_candidates(&backups, None, Some(10), 1_000_000);
        assert_eq!(candidates, vec![("backup_100".to_string(), 100)]);
    }

    #[test]
    fn no_candidates_when_nothing_old_enough() {
        let backups = vec![("backup_999_999".to_string(), 999_999)];
        let candidates = select_candidates(&backups, None, Some(9999), 1_000_000);
        assert!(candidates.is_empty());
    }

    #[test]
    fn find_backups_matches_naming_pattern() -> Result<()> {
        let dir = tempfile::tempdir()?;
        fs::create_dir(dir.path().join("backup_123"))?;
        fs::create_dir(dir.path().join("backup_456"))?;
        fs::create_dir(dir.path().join("not_a_backup"))?;
        fs::write(dir.path().join("backup_789"), "not a dir")?;

        let mut backups = find_backups(dir.path())?;
        backups.sort();
        assert_eq!(
            backups,
            vec![
                ("backup_123".to_string(), 123),
                ("backup_456".to_string(), 456),
            ]
        );
        Ok(())
    }
}

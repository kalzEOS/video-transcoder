//! Disk-backed, per-encode temporary workspaces.

use std::path::{Path, PathBuf};

use tempfile::TempDir;

/// Resolve the parent filesystem for a job. Automatic mode follows the output
/// filesystem; custom mode is explicit and never falls back to system tmp.
pub fn resolve_parent(mode: &str, custom: &str, output_parent: &Path) -> Result<PathBuf, String> {
    let parent = if mode.eq_ignore_ascii_case("custom") {
        if custom.trim().is_empty() {
            return Err("Custom temporary files location is empty".into());
        }
        PathBuf::from(custom)
    } else {
        output_parent.to_path_buf()
    };
    std::fs::create_dir_all(&parent).map_err(|e| {
        format!(
            "Could not create temporary files location '{}': {e}",
            parent.display()
        )
    })?;
    if !parent.is_dir() {
        return Err(format!(
            "Temporary files location is not a directory: {}",
            parent.display()
        ));
    }
    Ok(parent)
}

/// Create a unique workspace and reject insufficient disk space before a
/// large HDR10+ operation begins. The workspace is removed by `TempDir`.
pub fn create_job_workspace(parent: &Path, source_bytes: u64) -> Result<TempDir, String> {
    let required = source_bytes
        .saturating_mul(2)
        .saturating_add(128 * 1024 * 1024);
    if let Some((_, free)) = crate::disk_monitor::partition_free_space(parent) {
        if free < required {
            return Err(format!(
                "Insufficient disk space for temporary encoding files on '{}': {} free, approximately {} required",
                parent.display(),
                crate::disk_monitor::format_bytes(free),
                crate::disk_monitor::format_bytes(required)
            ));
        }
    }
    tempfile::Builder::new()
        .prefix(".histv-job-")
        .tempdir_in(parent)
        .map_err(|e| format!("Could not create per-encode temporary workspace: {e}"))
}

/// Remove only clearly stale managed workspaces. A one-day grace period avoids
/// touching a long-running concurrent encode after a normal app restart.
pub fn cleanup_abandoned(parent: &Path) -> std::io::Result<usize> {
    let cutoff = std::time::SystemTime::now()
        .checked_sub(std::time::Duration::from_secs(24 * 60 * 60))
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
    let mut removed = 0;
    for entry in std::fs::read_dir(parent)? {
        let entry = entry?;
        if !entry
            .file_name()
            .to_string_lossy()
            .starts_with(".histv-job-")
            || !entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false)
        {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .map(|modified| modified < cutoff)
            .unwrap_or(false);
        if stale && std::fs::remove_dir_all(entry.path()).is_ok() {
            removed += 1;
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatic_uses_output_filesystem() {
        let root = tempfile::tempdir().unwrap();
        let parent = resolve_parent("automatic", "", root.path()).unwrap();
        assert_eq!(parent, root.path());
    }

    #[test]
    fn custom_location_is_explicit() {
        let root = tempfile::tempdir().unwrap();
        let custom = root.path().join("custom with spaces");
        let parent = resolve_parent("custom", &custom.to_string_lossy(), root.path()).unwrap();
        assert_eq!(parent, custom);
    }

    #[test]
    fn workspaces_are_unique_and_cleanable() {
        let root = tempfile::tempdir().unwrap();
        let first = create_job_workspace(root.path(), 0).unwrap();
        let second = create_job_workspace(root.path(), 0).unwrap();
        assert_ne!(first.path(), second.path());
        let first_path = first.path().to_path_buf();
        drop(first);
        assert!(!first_path.exists());
    }

    #[test]
    fn cleanup_ignores_recent_and_unmanaged_directories() {
        let root = tempfile::tempdir().unwrap();
        let recent = root.path().join(".histv-job-recent");
        std::fs::create_dir(&recent).unwrap();
        let keep = root.path().join("keep");
        std::fs::create_dir(&keep).unwrap();
        assert_eq!(cleanup_abandoned(root.path()).unwrap(), 0);
        assert!(recent.exists());
        assert!(keep.exists());
    }

    #[test]
    fn insufficient_space_is_rejected_before_workspace_creation() {
        let root = tempfile::tempdir().unwrap();
        assert!(create_job_workspace(root.path(), u64::MAX).is_err());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }
}

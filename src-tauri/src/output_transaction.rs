//! Job-owned output staging and destination-side installation.
//!
//! FFmpeg and postprocessing only write the pending file. The destination and
//! source remain intact until validation succeeds and installation completes.

use std::{
    fs,
    path::{Path, PathBuf},
    time::SystemTime,
};

struct Snapshot {
    identity: same_file::Handle,
    len: u64,
    modified: Option<SystemTime>,
    permissions: fs::Permissions,
}

impl Snapshot {
    fn read(path: &Path) -> std::io::Result<Self> {
        let identity = same_file::Handle::from_path(path)?;
        let metadata = identity.as_file().metadata()?;
        if !metadata.is_file() {
            return Err(std::io::Error::other("Expected a regular file"));
        }
        Ok(Self {
            identity,
            len: metadata.len(),
            modified: metadata.modified().ok(),
            permissions: metadata.permissions(),
        })
    }

    fn unchanged(&self, path: &Path) -> bool {
        Self::read(path).is_ok_and(|current| {
            self.identity == current.identity
                && self.len == current.len
                && self.modified == current.modified
        })
    }
}

pub struct OutputTransaction {
    source: PathBuf,
    source_snapshot: Snapshot,
    destination: PathBuf,
    destination_snapshot: Option<Snapshot>,
    replaces_source: bool,
    pending: Option<tempfile::TempPath>,
    committed: bool,
}

impl OutputTransaction {
    pub fn new(source: &Path, destination: &Path, replace: bool) -> Result<Self, String> {
        let source_snapshot = Snapshot::read(source)
            .map_err(|e| format!("Cannot inspect source '{}': {e}", source.display()))?;
        let destination_snapshot = match Snapshot::read(destination) {
            Ok(snapshot) => Some(snapshot),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // A dangling symlink is still an existing directory entry.
                if fs::symlink_metadata(destination).is_ok() {
                    return Err("Output is a dangling symlink; choose another destination".into());
                }
                None
            }
            Err(e) => {
                return Err(format!(
                    "Cannot inspect output '{}': {e}",
                    destination.display()
                ))
            }
        };
        let replaces_source = destination_snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.identity == source_snapshot.identity);
        if replaces_source && !replace {
            return Err("Output refers to the source video (including a file alias). Choose a different folder or use Replace source.".into());
        }
        let parent = destination
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let extension = destination
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("mkv");
        let name = destination
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy();
        // A short readable prefix avoids exceeding NAME_MAX for long filenames.
        let name: String = name.chars().take(32).collect();
        let prefix = format!(".vt-{name}-pending-");
        let suffix = format!(".{extension}");
        let mut builder = tempfile::Builder::new();
        builder.prefix(&prefix).suffix(&suffix);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // Match FFmpeg's ordinary output permissions, respecting umask.
            builder.permissions(fs::Permissions::from_mode(0o666));
        }
        let pending = builder
            .tempfile_in(parent)
            .map_err(|e| {
                format!(
                    "Cannot create output staging file on '{}': {e}",
                    parent.display()
                )
            })?
            .into_temp_path();
        Ok(Self {
            source: source.into(),
            source_snapshot,
            destination: destination.into(),
            destination_snapshot,
            replaces_source,
            pending: Some(pending),
            committed: false,
        })
    }

    pub fn pending_path(&self) -> &Path {
        self.pending
            .as_ref()
            .expect("Output transaction is already finished")
            .as_ref()
    }

    pub fn replaces_source(&self) -> bool {
        self.replaces_source
    }

    /// Install only the validated job-owned output. A previously absent target
    /// uses no-clobber installation so another job cannot replace it unnoticed.
    pub fn commit(&mut self) -> Result<(), String> {
        if !self.source_snapshot.unchanged(&self.source) {
            return Err("Source changed while encoding; output installation stopped".into());
        }
        if let Some(snapshot) = &self.destination_snapshot {
            if !snapshot.unchanged(&self.destination) {
                return Err("Destination changed while encoding; existing output retained".into());
            }
        }
        let pending = self
            .pending
            .as_ref()
            .ok_or("Output transaction already finished")?;
        let file = fs::OpenOptions::new()
            .write(true)
            .open(pending)
            .map_err(|e| format!("Cannot open pending output: {e}"))?;
        if file.metadata().map_err(|e| e.to_string())?.len() == 0 {
            return Err("Cannot install empty encoded output".into());
        }
        file.sync_all()
            .map_err(|e| format!("Cannot flush pending output: {e}"))?;
        drop(file);
        if let Some(snapshot) = &self.destination_snapshot {
            fs::set_permissions(pending, snapshot.permissions.clone())
                .map_err(|e| format!("Cannot preserve destination permissions: {e}"))?;
        }
        let pending = self.pending.take().unwrap();
        let result = if self.destination_snapshot.is_some() {
            pending.persist(&self.destination)
        } else {
            pending.persist_noclobber(&self.destination)
        };
        if let Err(error) = result {
            let message = format!(
                "Cannot install encoded output at '{}': {}",
                self.destination.display(),
                error.error
            );
            self.pending = Some(error.path);
            return Err(message);
        }
        self.committed = true;
        Ok(())
    }

    /// Called only after installation. If replacement changed the container,
    /// the old source path is distinct and may now be removed safely.
    pub fn remove_source(&self) -> Result<(), String> {
        if !self.committed {
            return Err("Output has not been installed; source retained".into());
        }
        if self.replaces_source {
            return Ok(());
        }
        if !self.source_snapshot.unchanged(&self.source) {
            return Err("Source changed while encoding; source retained".into());
        }
        fs::remove_file(&self.source).map_err(|e| format!("Could not remove source: {e}"))
    }

    /// Keep a completed encode when installation/recovery fails. TempPath's
    /// guard otherwise removes only this job's pending file on every exit.
    pub fn retain_pending(&mut self) -> Result<Option<PathBuf>, String> {
        self.pending
            .take()
            .map(|path| path.keep().map_err(|e| e.to_string()))
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source.mkv");
        let output = root.path().join("output.mkv");
        fs::write(&source, b"original source").unwrap();
        (root, source, output)
    }

    #[test]
    fn same_path_and_hard_links_are_rejected_without_modifying_source() {
        let (_root, source, output) = paths();
        assert!(OutputTransaction::new(&source, &source, false).is_err());
        fs::hard_link(&source, &output).unwrap();
        assert!(OutputTransaction::new(&source, &output, false).is_err());
        assert_eq!(fs::read(source).unwrap(), b"original source");
    }

    #[cfg(unix)]
    #[test]
    fn symlink_alias_is_rejected() {
        let (_root, source, output) = paths();
        std::os::unix::fs::symlink(&source, &output).unwrap();
        assert!(OutputTransaction::new(&source, &output, false).is_err());
        assert_eq!(fs::read(source).unwrap(), b"original source");
    }

    #[test]
    fn failure_or_cancellation_cleans_only_pending_output() {
        let (root, source, output) = paths();
        fs::write(&output, b"previous output").unwrap();
        let transaction = OutputTransaction::new(&source, &output, false).unwrap();
        fs::write(transaction.pending_path(), b"partial encoding").unwrap();
        let pending = transaction.pending_path().to_path_buf();
        drop(transaction);
        assert!(!pending.exists());
        assert_eq!(fs::read(source).unwrap(), b"original source");
        assert_eq!(fs::read(output).unwrap(), b"previous output");
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 2);
    }

    #[test]
    fn validated_output_replaces_existing_target_without_fixed_backup_names() {
        let (_root, source, output) = paths();
        fs::write(&output, b"previous output").unwrap();
        let unrelated_backup = source.with_file_name("source.mkv.histv-bak");
        fs::write(&unrelated_backup, b"keep backup").unwrap();
        let mut transaction = OutputTransaction::new(&source, &output, false).unwrap();
        fs::write(transaction.pending_path(), b"validated encoding").unwrap();
        transaction.commit().unwrap();
        assert_eq!(fs::read(output).unwrap(), b"validated encoding");
        assert_eq!(fs::read(source).unwrap(), b"original source");
        assert_eq!(fs::read(unrelated_backup).unwrap(), b"keep backup");
    }

    #[test]
    fn replace_source_commits_before_removing_original() {
        let (_root, source, _) = paths();
        let mut transaction = OutputTransaction::new(&source, &source, true).unwrap();
        fs::write(transaction.pending_path(), b"validated encoding").unwrap();
        transaction.commit().unwrap();
        transaction.remove_source().unwrap();
        assert_eq!(fs::read(source).unwrap(), b"validated encoding");
    }

    #[test]
    fn changed_destination_is_preserved_and_encode_can_be_recovered() {
        let (_root, source, output) = paths();
        fs::write(&output, b"previous output").unwrap();
        let mut transaction = OutputTransaction::new(&source, &output, false).unwrap();
        fs::write(transaction.pending_path(), b"completed encoding").unwrap();
        fs::write(&output, b"new external output").unwrap();
        assert!(transaction.commit().is_err());
        let recovery = transaction.retain_pending().unwrap().unwrap();
        assert!(transaction.remove_source().is_err());
        drop(transaction);
        assert_eq!(fs::read(recovery).unwrap(), b"completed encoding");
        assert_eq!(fs::read(output).unwrap(), b"new external output");
        assert_eq!(fs::read(source).unwrap(), b"original source");
    }

    #[test]
    fn competing_jobs_have_unique_staging_and_cannot_clobber_new_output() {
        let (_root, source, output) = paths();
        let mut first = OutputTransaction::new(&source, &output, false).unwrap();
        let mut second = OutputTransaction::new(&source, &output, false).unwrap();
        assert_ne!(first.pending_path(), second.pending_path());
        fs::write(first.pending_path(), b"first encode").unwrap();
        fs::write(second.pending_path(), b"second encode").unwrap();
        first.commit().unwrap();
        assert!(second.commit().is_err());
        assert_eq!(fs::read(output).unwrap(), b"first encode");
        assert_eq!(fs::read(source).unwrap(), b"original source");
    }

    #[test]
    fn changed_source_and_empty_outputs_cannot_be_committed() {
        let (_root, source, output) = paths();
        let mut transaction = OutputTransaction::new(&source, &output, false).unwrap();
        assert!(transaction.commit().is_err());
        fs::write(transaction.pending_path(), b"encoded").unwrap();
        fs::write(&source, b"new source content").unwrap();
        assert!(transaction.commit().is_err());
        assert!(transaction.remove_source().is_err());
        assert!(!output.exists());
        assert_eq!(fs::read(source).unwrap(), b"new source content");
    }

    #[cfg(unix)]
    #[test]
    fn replacement_retains_destination_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let (_root, source, output) = paths();
        fs::write(&output, b"previous output").unwrap();
        fs::set_permissions(&output, fs::Permissions::from_mode(0o640)).unwrap();
        let mut transaction = OutputTransaction::new(&source, &output, false).unwrap();
        fs::write(transaction.pending_path(), b"validated encoding").unwrap();
        transaction.commit().unwrap();
        assert_eq!(
            fs::metadata(output).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }
}

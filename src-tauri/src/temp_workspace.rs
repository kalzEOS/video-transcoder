//! Owned, locked metadata workspaces and stage-aware capacity reservations.
use crate::workspace_registry::{filesystem_key, Allocation, Record, Registry};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

const MARKER: &str = ".histv-owner.json";
const MAGIC: &str = "video-transcoder-metadata-workspace-v1";
const HEADROOM: u64 = 128 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
struct Owner {
    magic: String,
    workspace: PathBuf,
    token: String,
}

pub fn resolve_parent(mode: &str, custom: &str, output_parent: &Path) -> Result<PathBuf, String> {
    let parent = if mode.eq_ignore_ascii_case("custom") {
        if custom.trim().is_empty() {
            return Err("Custom temporary files location is empty".into());
        }
        PathBuf::from(custom)
    } else if mode.eq_ignore_ascii_case("automatic") {
        output_parent.to_path_buf()
    } else {
        return Err("Unknown Temporary Files Location setting".into());
    };
    fs::create_dir_all(&parent).map_err(|e| {
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

pub struct JobWorkspace {
    directory: Option<tempfile::TempDir>,
    lease: Option<File>,
    record: Record,
}
impl JobWorkspace {
    pub fn path(&self) -> &Path {
        &self.record.workspace
    }
}
impl Drop for JobWorkspace {
    fn drop(&mut self) {
        let registry = Registry::open();
        // Keep registry serialization across release/deletion: Windows cannot
        // remove an open marker. A live job always retains the lease until here.
        drop(self.lease.take());
        let removed = self
            .directory
            .take()
            .is_none_or(|directory| directory.close().is_ok());
        if let Ok(mut registry) = registry {
            if removed {
                registry.jobs.retain(|job| {
                    job.token != self.record.token || job.workspace != self.record.workspace
                });
                let _ = registry.save();
            }
        }
    }
}

pub fn create_job_workspace(parent: &Path, source_bytes: u64) -> Result<JobWorkspace, String> {
    create_job_workspace_for_output(parent, source_bytes, None)
}
pub fn create_job_workspace_for_output(
    parent: &Path,
    source_bytes: u64,
    output: Option<&Path>,
) -> Result<JobWorkspace, String> {
    cleanup_abandoned(parent)
        .map_err(|e| format!("Cannot inspect abandoned metadata jobs: {e}"))?;
    let directory = tempfile::Builder::new()
        .prefix(".histv-job-")
        .tempdir_in(parent)
        .map_err(|e| format!("Cannot create metadata workspace: {e}"))?;
    let workspace = directory.path().canonicalize().map_err(|e| e.to_string())?;
    let token = workspace
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let mut lease = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(workspace.join(MARKER))
        .map_err(|e| e.to_string())?;
    lease
        .try_lock()
        .map_err(|e| format!("Cannot lock metadata workspace: {e}"))?;
    serde_json::to_writer(
        &mut lease,
        &Owner {
            magic: MAGIC.into(),
            workspace: workspace.clone(),
            token: token.clone(),
        },
    )
    .map_err(|e| e.to_string())?;
    lease.flush().map_err(|e| e.to_string())?;
    lease.sync_all().map_err(|e| e.to_string())?;
    let output = output
        .map(|path| {
            let parent = path
                .parent()
                .unwrap_or(Path::new("."))
                .canonicalize()
                .map_err(|e| e.to_string())?;
            Ok::<_, String>(parent.join(path.file_name().ok_or("Output filename missing")?))
        })
        .transpose()?;
    let output_baseline = output
        .as_ref()
        .and_then(|p| fs::metadata(p).ok())
        .map(|m| m.len())
        .unwrap_or(0);
    let record = Record {
        workspace,
        token,
        output,
        output_baseline,
        allocations: Vec::new(),
    };
    let job = JobWorkspace {
        directory: Some(directory),
        lease: Some(lease),
        record,
    };
    {
        let mut registry = Registry::open()?;
        let parent = parent.canonicalize().map_err(|e| e.to_string())?;
        if registry
            .jobs
            .iter()
            .any(|record| parent.starts_with(&record.workspace))
        {
            return Err("Temporary Files Location is inside a managed job workspace; choose its parent folder".into());
        }
        registry.jobs.push(job.record.clone());
        registry.save()?;
    }
    // Existing source bytes are already reflected in filesystem free space.
    // Budget four metadata/mux copies plus an output estimate, on each volume.
    check_stage(
        job.path(),
        job.record.output.as_deref(),
        source_bytes.saturating_mul(4).saturating_add(HEADROOM),
        source_bytes.saturating_add(HEADROOM),
        "before encoding",
    )?;
    Ok(job)
}

fn directory_bytes(path: &Path) -> u64 {
    fn visit(path: &Path, depth: usize) -> u64 {
        if depth > 32 {
            return 0;
        }
        let Ok(entries) = fs::read_dir(path) else {
            return 0;
        };
        entries.filter_map(Result::ok).fold(0u64, |sum, entry| {
            let bytes = match entry.file_type() {
                Ok(kind) if kind.is_symlink() => 0,
                Ok(kind) if kind.is_dir() => visit(&entry.path(), depth + 1),
                Ok(_) => entry.metadata().map(|m| m.len()).unwrap_or(0),
                Err(_) => 0,
            };
            sum.saturating_add(bytes)
        })
    }
    visit(path, 0)
}
fn used_bytes(record: &Record, key: &str) -> u64 {
    let mut used = 0u64;
    if filesystem_key(&record.workspace).is_ok_and(|v| v == key) {
        used = directory_bytes(&record.workspace);
    }
    if let Some(output) = &record.output {
        if output
            .parent()
            .and_then(|p| filesystem_key(p).ok())
            .is_some_and(|v| v == key)
        {
            used = used.saturating_add(
                fs::metadata(output)
                    .map(|m| m.len().saturating_sub(record.output_baseline))
                    .unwrap_or(0),
            );
        }
    }
    used
}

/// Return a lease only when ownership/path binding is valid and the job is
/// inactive. No filename prefix, age or PID alone authorizes cleanup.
fn inactive_lease(record: &Record) -> Option<File> {
    if fs::symlink_metadata(&record.workspace)
        .ok()?
        .file_type()
        .is_symlink()
    {
        return None;
    }
    if record.workspace.canonicalize().ok()? != record.workspace {
        return None;
    }
    let marker = record.workspace.join(MARKER);
    if !fs::symlink_metadata(&marker).ok()?.is_file() {
        return None;
    }
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(marker)
        .ok()?;
    file.try_lock().ok()?;
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(16385)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > 16384 {
        return None;
    }
    let owner: Owner = serde_json::from_slice(&bytes).ok()?;
    if owner.magic != MAGIC || owner.workspace != record.workspace || owner.token != record.token {
        return None;
    }
    Some(file)
}
fn active(record: &Record) -> bool {
    // A missing directory has no future allocations. Permission/lock/ownership
    // uncertainty is treated conservatively as active for capacity accounting.
    if fs::symlink_metadata(&record.workspace)
        .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
    {
        return false;
    }
    inactive_lease(record).is_none()
}

fn reserve(
    registry: &mut Registry,
    owner: Option<&Record>,
    requests: &[(PathBuf, u64)],
    query: &dyn Fn(&Path) -> Option<(u64, u64)>,
    stage: &str,
) -> Result<(), String> {
    let mut volumes = std::collections::BTreeMap::<String, (PathBuf, u64)>::new();
    for (path, bytes) in requests {
        let key = filesystem_key(path)?;
        let entry = volumes.entry(key).or_insert((path.clone(), 0));
        entry.1 = entry.1.saturating_add(*bytes);
    }
    let mut allocations = Vec::new();
    for (key, (path, needed)) in volumes {
        let (_,free)=query(&path).ok_or_else(||format!("Cannot check available disk space on '{}' {stage}; no temporary-storage fallback was made",path.display()))?;
        let other = registry
            .jobs
            .iter()
            .filter(|job| {
                owner.is_none_or(|owner| {
                    job.token != owner.token || job.workspace != owner.workspace
                })
            })
            .filter(|job| active(job))
            .flat_map(|job| {
                job.allocations
                    .iter()
                    .filter(|a| a.key == key)
                    .map(|allocation| allocation.peak.saturating_sub(used_bytes(job, &key)))
            })
            .fold(0u64, u64::saturating_add);
        let available = free.saturating_sub(other);
        if available < needed {
            return Err(format!("Insufficient disk space {stage} on '{}': {} free, {} reserved for other jobs, approximately {} additional required",path.display(),crate::disk_monitor::format_bytes(free),crate::disk_monitor::format_bytes(other),crate::disk_monitor::format_bytes(needed)));
        }
        let used = owner.map(|job| used_bytes(job, &key)).unwrap_or(0);
        allocations.push(Allocation {
            path,
            key,
            peak: used.saturating_add(needed),
        });
    }
    if let Some(owner) = owner {
        if let Some(job) = registry
            .jobs
            .iter_mut()
            .find(|job| job.token == owner.token && job.workspace == owner.workspace)
        {
            job.allocations = allocations;
            registry.save()?;
        }
    }
    Ok(())
}

pub fn check_stage(
    workspace: &Path,
    encoded: Option<&Path>,
    work_bytes: u64,
    output_bytes: u64,
    stage: &str,
) -> Result<(), String> {
    let workspace = workspace.canonicalize().map_err(|e| e.to_string())?;
    let mut registry = Registry::open()?;
    let owner = registry
        .jobs
        .iter()
        .filter(|job| workspace.starts_with(&job.workspace))
        .max_by_key(|job| job.workspace.components().count())
        .cloned();
    let mut requests = vec![(workspace, work_bytes)];
    if let Some(encoded) = encoded {
        requests.push((
            encoded.parent().unwrap_or(Path::new(".")).to_path_buf(),
            output_bytes,
        ));
    }
    reserve(
        &mut registry,
        owner.as_ref(),
        &requests,
        &crate::disk_monitor::partition_free_space,
        stage,
    )
}

pub fn reinsertion_budget(encoded_bytes: u64, metadata_bytes: u64) -> (u64, u64) {
    // HEVC extraction + injected HEVC + GPAC MP4 + remux coexist. A final
    // destination-side copy may be needed when custom storage is elsewhere.
    let one = encoded_bytes
        .saturating_add(metadata_bytes)
        .saturating_add(HEADROOM);
    (one.saturating_mul(4), one)
}

fn cleanup(parent: Option<&Path>) -> Result<usize, String> {
    let parent = parent
        .map(|p| p.canonicalize().map_err(|e| e.to_string()))
        .transpose()?;
    let mut registry = Registry::open()?;
    let mut removed = 0;
    let mut keep = Vec::new();
    for record in &registry.jobs {
        if parent
            .as_ref()
            .is_some_and(|p| record.workspace.parent() != Some(p.as_path()))
        {
            keep.push(record.clone());
            continue;
        }
        if fs::symlink_metadata(&record.workspace)
            .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
        {
            continue;
        }
        if let Some(lease) = inactive_lease(record) {
            // Registry lock excludes other cleanup/recovery operations. No
            // running job can adopt this unique directory after lease release.
            drop(lease);
            if fs::remove_dir_all(&record.workspace).is_ok() {
                removed += 1;
                continue;
            }
        }
        keep.push(record.clone());
    }
    registry.jobs = keep;
    registry.save()?;
    Ok(removed)
}
pub fn cleanup_abandoned(parent: &Path) -> std::io::Result<usize> {
    cleanup(Some(parent)).map_err(std::io::Error::other)
}
pub fn cleanup_registered_abandoned() -> Result<usize, String> {
    cleanup(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn old_active_and_unmanaged_workspaces_cannot_be_cleaned() {
        let root = tempfile::tempdir().unwrap();
        let job = create_job_workspace(root.path(), 0).unwrap();
        #[cfg(unix)]
        File::open(job.path())
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(std::time::UNIX_EPOCH))
            .unwrap();
        let unmanaged = root.path().join(".histv-job-unrelated");
        fs::create_dir(&unmanaged).unwrap();
        fs::write(unmanaged.join("important.txt"), b"unrelated").unwrap();
        fs::copy(job.path().join(MARKER), unmanaged.join(MARKER)).unwrap();
        assert_eq!(cleanup_abandoned(root.path()).unwrap(), 0);
        assert!(job.path().is_dir());
        assert_eq!(
            fs::read(unmanaged.join("important.txt")).unwrap(),
            b"unrelated"
        );
        assert!(create_job_workspace(job.path(), 0).is_err());
    }
    #[test]
    fn interrupted_fixture() {
        if let Some(root) = std::env::var_os("HISTV_INTERRUPTED_FIXTURE") {
            let root = PathBuf::from(root);
            let job = create_job_workspace(&root, 0).unwrap();
            fs::write(job.path().join("partial.hevc"), b"temporary").unwrap();
            fs::write(
                root.join("where.txt"),
                job.path().to_string_lossy().as_bytes(),
            )
            .unwrap();
            std::process::exit(0); // Simulate process termination without Drop.
        }
    }
    #[test]
    fn interrupted_job_is_cleaned_after_lock_is_released_by_process_exit() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("source.mkv"), b"source").unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "temp_workspace::tests::interrupted_fixture",
                "--nocapture",
            ])
            .env("HISTV_INTERRUPTED_FIXTURE", root.path())
            .output()
            .unwrap();
        assert!(
            status.status.success(),
            "{}",
            String::from_utf8_lossy(&status.stderr)
        );
        let path = PathBuf::from(fs::read_to_string(root.path().join("where.txt")).unwrap());
        assert!(path.exists());
        assert_eq!(cleanup_abandoned(root.path()).unwrap(), 1);
        assert!(!path.exists());
        assert_eq!(fs::read(root.path().join("source.mkv")).unwrap(), b"source");
    }
    #[test]
    fn stage_reservations_account_for_other_jobs_and_unknown_space_fails_closed() {
        let root = tempfile::tempdir().unwrap();
        let a = create_job_workspace(root.path(), 0).unwrap();
        let b = create_job_workspace(root.path(), 0).unwrap();
        let mut registry = Registry::open().unwrap();
        registry.use_test_file(root.path().join("simulated-reservations.json"));
        let owner_a = registry
            .jobs
            .iter()
            .find(|v| v.workspace == a.path())
            .unwrap()
            .clone();
        let owner_b = registry
            .jobs
            .iter()
            .find(|v| v.workspace == b.path())
            .unwrap()
            .clone();
        let total = 1024 * 1024 * 1024 * 1024u64;
        let requested = total * 3 / 4;
        reserve(
            &mut registry,
            Some(&owner_a),
            &[(a.path().into(), requested)],
            &|_| Some((total, total)),
            "test",
        )
        .unwrap();
        assert!(reserve(
            &mut registry,
            Some(&owner_b),
            &[(b.path().into(), requested)],
            &|_| Some((total, total)),
            "test"
        )
        .unwrap_err()
        .contains("reserved for other jobs"));
        assert!(reserve(
            &mut registry,
            Some(&owner_b),
            &[(b.path().into(), 1)],
            &|_| None,
            "test"
        )
        .unwrap_err()
        .contains("Cannot check available"));
        drop(registry);
    }
    #[test]
    fn reinsertion_budget_includes_all_coexisting_files_and_saturates_safely() {
        let (work, output) = reinsertion_budget(10_000_000_000, 1_000_000);
        assert!(work >= 40_000_000_000);
        assert!(output > 10_000_000_000);
        assert_eq!(reinsertion_budget(u64::MAX, u64::MAX), (u64::MAX, u64::MAX));
    }

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

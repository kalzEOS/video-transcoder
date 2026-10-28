//! Per-user, cross-process reservations. Registry data never lives in an
//! output directory; a separate lock protects atomic JSON replacement.
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
};

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Allocation {
    pub path: PathBuf,
    pub key: String,
    pub peak: u64,
}
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Record {
    pub workspace: PathBuf,
    pub token: String,
    pub output: Option<PathBuf>,
    pub output_baseline: u64,
    pub allocations: Vec<Allocation>,
}
pub(crate) struct Registry {
    _lock: File,
    file: PathBuf,
    pub jobs: Vec<Record>,
}

impl Registry {
    #[cfg(test)]
    pub fn use_test_file(&mut self, path: PathBuf) {
        self.file = path;
    }
    pub fn open() -> Result<Self, String> {
        let parent = registry_parent()?;
        fs::create_dir_all(&parent)
            .map_err(|e| format!("Cannot create workspace registry: {e}"))?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(parent.join("registry.lock"))
            .map_err(|e| e.to_string())?;
        lock.lock()
            .map_err(|e| format!("Cannot lock workspace registry: {e}"))?;
        let file = parent.join("jobs.json");
        let jobs = match fs::read(&file) {
            Ok(bytes) if bytes.len() <= 1024 * 1024 => serde_json::from_slice(&bytes)
                .map_err(|e| format!("Invalid workspace registry; existing jobs retained: {e}"))?,
            Ok(_) => {
                return Err(
                    "Workspace registry is unexpectedly large; existing jobs retained".into(),
                )
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(format!("Cannot read workspace registry: {e}")),
        };
        Ok(Self {
            _lock: lock,
            file,
            jobs,
        })
    }
    pub fn save(&self) -> Result<(), String> {
        use std::io::Write;
        let mut temporary = tempfile::NamedTempFile::new_in(self.file.parent().unwrap())
            .map_err(|e| e.to_string())?;
        temporary
            .write_all(&serde_json::to_vec(&self.jobs).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        temporary.as_file().sync_all().map_err(|e| e.to_string())?;
        temporary.persist(&self.file).map_err(|e| e.to_string())?;
        Ok(())
    }
}

fn registry_parent() -> Result<PathBuf, String> {
    #[cfg(windows)]
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    #[cfg(target_os = "macos")]
    let base =
        std::env::var_os("HOME").map(|v| PathBuf::from(v).join("Library/Application Support"));
    #[cfg(not(any(windows, target_os = "macos")))]
    let base = std::env::var_os("XDG_STATE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|v| PathBuf::from(v).join(".local/state")));
    base.map(|base|base.join("video-transcoder/workspaces"))
        .ok_or_else(||"Cannot resolve workspace registry location; no fallback to system temporary storage was made".into())
}

pub(crate) fn filesystem_key(path: &Path) -> Result<String, String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = fs::metadata(path).map_err(|e| e.to_string())?;
        Ok(format!("unix:{}", metadata.dev()))
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        #[link(name = "kernel32")]
        extern "system" {
            fn GetVolumePathNameW(path: *const u16, volume: *mut u16, size: u32) -> i32;
            fn GetVolumeNameForVolumeMountPointW(
                path: *const u16,
                volume: *mut u16,
                size: u32,
            ) -> i32;
        }
        let path: Vec<_> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut root = vec![0u16; 32768];
        let mut guid = vec![0u16; 32768];
        if unsafe { GetVolumePathNameW(path.as_ptr(), root.as_mut_ptr(), root.len() as u32) } == 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let value = if unsafe {
            GetVolumeNameForVolumeMountPointW(root.as_ptr(), guid.as_mut_ptr(), guid.len() as u32)
        } != 0
        {
            &guid
        } else {
            &root
        };
        let end = value.iter().position(|v| *v == 0).unwrap_or(value.len());
        Ok(format!(
            "windows:{}",
            String::from_utf16_lossy(&value[..end]).to_lowercase()
        ))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Err("Cannot identify temporary-storage filesystem on this platform".into())
    }
}

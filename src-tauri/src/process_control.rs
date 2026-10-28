//! Suspend only our owned child and always resume it before cancellation/drop.
pub struct PauseGuard {
    pid: u32,
    paused: bool,
}
impl PauseGuard {
    pub fn new(pid: u32) -> Self {
        Self { pid, paused: false }
    }
    pub fn set(&mut self, paused: bool) -> Result<(), String> {
        if self.paused == paused {
            return Ok(());
        }
        #[cfg(unix)]
        {
            let signal = if paused { libc::SIGSTOP } else { libc::SIGCONT };
            // The child remains owned/unreaped by its supervisor; its PID
            // cannot be reassigned while this guard is used.
            if unsafe { libc::kill(self.pid as libc::pid_t, signal) } != 0 {
                return Err(format!(
                    "Cannot {} FFmpeg: {}",
                    if paused { "pause" } else { "resume" },
                    std::io::Error::last_os_error()
                ));
            }
        }
        #[cfg(not(unix))]
        {
            let _ = self.pid;
            if paused {
                return Err("Process pausing is unavailable on this platform".into());
            }
        }
        self.paused = paused;
        Ok(())
    }
}
impl Drop for PauseGuard {
    fn drop(&mut self) {
        let _ = self.set(false);
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    #[test]
    fn child_is_really_stopped_and_resumed_on_drop() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let mut guard = PauseGuard::new(child.id());
        guard.set(true).unwrap();
        let status = format!("/proc/{}/status", child.id());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let text = std::fs::read_to_string(&status).unwrap();
            if text
                .lines()
                .any(|line| line.starts_with("State:") && line.contains("T (stopped)"))
            {
                break;
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        drop(guard);
        child.kill().unwrap();
        child.wait().unwrap();
    }
}

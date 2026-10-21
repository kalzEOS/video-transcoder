//! Forward encoding stages and cancellation to the existing UI event sink.
use crate::events::{BatchControl, EventSink};
use crate::queue::QueueItem;

pub struct PipelineSink<'a> {
    pub inner: &'a dyn EventSink,
    pub control: &'a dyn BatchControl,
}

impl EventSink for PipelineSink<'_> {
    fn cancellation_requested(&self) -> bool {
        self.control.should_cancel_all() || self.control.should_cancel_current()
    }
    fn processing_stage(&self, s: &str, m: &str, p: Option<f64>) {
        self.inner.processing_stage(s, m, p);
    }
    fn log(&self, m: &str) {
        self.inner.log(m);
    }
    fn file_progress(&self, p: f64, t: f64, d: f64, pass: Option<(u8, u8)>) {
        self.inner.file_progress(p, t, d, pass);
    }
    fn batch_progress(&self, c: u32, t: usize) {
        self.inner.batch_progress(c, t);
    }
    fn batch_status(&self, m: &str) {
        self.inner.batch_status(m);
    }
    fn queue_item_updated(&self, i: usize, s: &str) {
        match s {
            "Failed" => self.processing_stage(
                "failed",
                "Processing failed. See the log for details.",
                None,
            ),
            "Cancelled" => self.processing_stage("cancelled", "Cancelled", None),
            "Done" => self.processing_stage("complete", "Completed", Some(100.0)),
            _ => {}
        }
        self.inner.queue_item_updated(i, s);
    }
    fn queue_item_probed(&self, i: usize, q: &QueueItem) {
        self.inner.queue_item_probed(i, q);
    }
    fn batch_started(&self) {
        self.inner.batch_started();
    }
    fn batch_finished(&self, d: u32, f: u32, s: u32, t: &str) {
        self.inner.batch_finished(d, f, s, t);
    }
    fn ffmpeg_stderr(&self, m: &str) {
        self.inner.ffmpeg_stderr(m);
    }
    fn batch_command(&self, m: &str) {
        self.inner.batch_command(m);
    }
    fn ffmpeg_download_progress(&self, m: &str) {
        self.inner.ffmpeg_download_progress(m);
    }
    fn toast(&self, m: &str) {
        self.inner.toast(m);
    }
    fn post_batch(&self, a: &str, c: u32) {
        self.inner.post_batch(a, c);
    }
    fn wave_progress(&self, w: u32, t: u32, f: u32, s: u32) {
        self.inner.wave_progress(w, t, f, s);
    }
    fn wave_status(&self, m: &str) {
        self.inner.wave_status(m);
    }
    fn batch_time_estimate(&self, e: f64, r: f64) {
        self.inner.batch_time_estimate(e, r);
    }
    fn wave_time_estimate(&self, e: f64, r: f64) {
        self.inner.wave_time_estimate(e, r);
    }
}

pub fn check(sink: &dyn EventSink) -> std::io::Result<()> {
    if sink.cancellation_requested() {
        Err(std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "Cancelled",
        ))
    } else {
        Ok(())
    }
}

/// Metadata parsing uses synchronous buffered file IO. Let Tokio replace this
/// worker while it runs so GUI commands (especially Cancel) remain responsive.
pub async fn metadata_work<F: std::future::Future>(future: F) -> F::Output {
    if tokio::runtime::Handle::current().runtime_flavor()
        == tokio::runtime::RuntimeFlavor::MultiThread
    {
        tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(future))
    } else {
        future.await
    }
}

/// Drain stderr concurrently, poll cancellation, and reap the child before
/// returning, so TempDir cleanup never races a live metadata subprocess.
pub async fn wait_output(
    mut child: tokio::process::Child,
    sink: &dyn EventSink,
) -> Result<std::process::Output, String> {
    use tokio::io::AsyncReadExt;
    let stderr = child.stderr.take();
    let reader = tokio::spawn(async move {
        let mut tail = Vec::new();
        if let Some(mut stderr) = stderr {
            let mut buf = [0u8; 8192];
            loop {
                match stderr.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        tail.extend_from_slice(&buf[..n]);
                        if tail.len() > 65536 {
                            tail.drain(..tail.len() - 65536);
                        }
                    }
                    Err(_) => break,
                }
            }
        }
        tail
    });
    let result = loop {
        if sink.cancellation_requested() {
            let _ = child.kill().await;
            let _ = child.wait().await;
            break Err("Cancelled".to_string());
        }
        tokio::select! {
            status = child.wait() => break status.map_err(|e| e.to_string()),
            _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {}
        }
    };
    let stderr = reader.await.map_err(|e| e.to_string())?;
    Ok(std::process::Output {
        status: result?,
        stdout: Vec::new(),
        stderr,
    })
}

/// Throttle frame-based progress without inventing time estimates.
pub fn frames(
    sink: &dyn EventSink,
    stage: &str,
    message: &str,
    done: usize,
    total: usize,
) -> std::io::Result<()> {
    check(sink)?;
    if total > 0 && (done == total || done.is_multiple_of(256)) {
        sink.processing_stage(stage, message, Some(done as f64 / total as f64 * 100.0));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::{CancellableBatchControl, RecordingSink};
    use std::sync::{atomic::Ordering, Arc};

    #[test]
    fn stages_and_terminal_errors_forward_to_ui() {
        let inner = Arc::new(RecordingSink::new());
        let control = CancellableBatchControl::new();
        let sink = PipelineSink {
            inner: &inner,
            control: &control,
        };
        sink.processing_stage("preparing", "Preparing video", None);
        sink.processing_stage("encoding", "Starting hardware encoding", None);
        sink.processing_stage("finalizing", "Checking HDR10 output", None);
        sink.queue_item_updated(0, "Done");
        sink.queue_item_updated(1, "Failed");
        let stages = inner.stages.lock().unwrap();
        assert_eq!(
            stages.iter().map(|s| s.0.as_str()).collect::<Vec<_>>(),
            ["preparing", "encoding", "finalizing", "complete", "failed"]
        );
        assert_eq!(stages[0].2, None);
        assert_eq!(stages[3].2, Some(100.0));
    }

    #[test]
    fn cancellation_is_forwarded_without_reporting_a_failure() {
        let inner = RecordingSink::new();
        let control = CancellableBatchControl::new();
        let sink = PipelineSink {
            inner: &inner,
            control: &control,
        };
        assert!(!sink.cancellation_requested());
        control.cancel_current.store(true, Ordering::SeqCst);
        assert!(sink.cancellation_requested());
        sink.queue_item_updated(0, "Cancelled");
        assert_eq!(inner.stages.lock().unwrap()[0].0, "cancelled");
        control.cancel_current.store(false, Ordering::SeqCst);
        control.cancel_all.store(true, Ordering::SeqCst);
        assert!(sink.cancellation_requested());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancelled_metadata_child_is_killed_and_reaped() {
        let inner = RecordingSink::new();
        let control = CancellableBatchControl::new();
        control.cancel_current.store(true, Ordering::SeqCst);
        let sink = PipelineSink {
            inner: &inner,
            control: &control,
        };
        let child = tokio::process::Command::new("sleep")
            .arg("30")
            .kill_on_drop(true)
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        let result =
            tokio::time::timeout(std::time::Duration::from_secs(2), wait_output(child, &sink))
                .await
                .unwrap();
        assert_eq!(result.unwrap_err(), "Cancelled");
        assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn subprocess_failure_retains_stderr_without_classifying_it_as_encoder_failure() {
        let child = tokio::process::Command::new("cat")
            .arg("/nonexistent/histv-metadata-test-input")
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let result = wait_output(child, &crate::test_helpers::NoopSink)
            .await
            .unwrap();
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains("histv-metadata-test-input"));
    }
}

use super::*;
use crate::test_helpers::{CancellableBatchControl, RecordingSink};
use std::sync::{atomic::Ordering, Arc};

#[tokio::test(flavor = "multi_thread")]
async fn ffmpeg_error_tail_is_reported_without_file_logging() {
    let sink = RecordingSink::new();
    let control = CancellableBatchControl::new();
    let ctx = FfmpegRunContext {
        low_priority: false,
        stderr_log: None,
        stderr_label: "test",
        sink: &sink,
        batch_control: &control,
    };
    let result = run_ffmpeg_with_progress(
        &[
            "-v",
            "error",
            "-i",
            "/nonexistent/histv-missing-input-for-diagnostics",
            "-f",
            "null",
            NULL_DEVICE,
        ]
        .map(String::from)
        .to_vec(),
        1.0,
        None,
        &ctx,
    )
    .await
    .unwrap();
    assert_ne!(result.exit_code, 0);
    assert!(sink
        .take_logs()
        .iter()
        .any(|line| line.contains("histv-missing-input-for-diagnostics")
            && line.contains("FFmpeg failed")));
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn paused_ffmpeg_cancels_and_is_reaped() {
    let control = Arc::new(CancellableBatchControl::new());
    control.paused.store(true, Ordering::SeqCst);
    let sink = Arc::new(RecordingSink::new());
    let task = {
        let control = control.clone();
        let sink = sink.clone();
        tokio::spawn(async move {
            let ctx = FfmpegRunContext {
                low_priority: false,
                stderr_log: None,
                stderr_label: "test",
                sink: &sink,
                batch_control: &*control,
            };
            run_ffmpeg_with_progress(
                &[
                    "-re",
                    "-f",
                    "lavfi",
                    "-i",
                    "testsrc2=size=128x96:rate=24:duration=30",
                    "-f",
                    "null",
                    NULL_DEVICE,
                ]
                .map(String::from)
                .to_vec(),
                30.0,
                None,
                &ctx,
            )
            .await
            .unwrap()
        })
    };
    let pid = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let pid = sink.take_logs().iter().find_map(|line| {
                line.strip_prefix("  ffmpeg PID: ")
                    .and_then(|v| v.parse::<u32>().ok())
            });
            if let Some(pid) = pid {
                if std::fs::read_to_string(format!("/proc/{pid}/status")).is_ok_and(|s| {
                    s.lines()
                        .any(|l| l.starts_with("State:") && l.contains("T (stopped)"))
                }) {
                    break pid;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    control.cancel_current.store(true, Ordering::SeqCst);
    let result = tokio::time::timeout(std::time::Duration::from_secs(7), task)
        .await
        .unwrap()
        .unwrap();
    assert!(result.was_cancelled_current);
    assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
}

#[cfg(unix)]
#[tokio::test]
async fn stderr_tail_and_unterminated_lines_are_bounded() {
    let mut child = tokio::process::Command::new("python3")
        .args([
            "-c",
            "import sys;sys.stderr.write('x'*200000+'final diagnostic')",
        ])
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let progress = FfmpegProgress::new();
    let reader = spawn_stderr_reader(child.stderr.take().unwrap(), &progress, None, "test");
    assert!(child.wait().await.unwrap().success());
    reader.join().unwrap();
    let tail = progress.stderr_tail();
    assert!(tail.len() <= 65536);
    assert!(tail.ends_with("final diagnostic"));
}

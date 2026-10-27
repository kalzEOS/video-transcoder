//! Output-safety regressions run only on small disposable files.
use super::*;
use crate::test_helpers::{default_settings, make_queue_item, CancellableBatchControl, NoopSink};
use std::{
    path::{Path, PathBuf},
    sync::{atomic::Ordering, Mutex},
};

fn settings(folder: &Path) -> BatchSettings {
    BatchSettings {
        output_folder: folder.to_string_lossy().into_owned(),
        threshold: 100.0,
        codec_family: "h264".into(),
        video_encoder: "libx264".into(),
        audio_encoder: "copy".into(),
        ..default_settings()
    }
}

fn synthetic_source(root: &Path) -> PathBuf {
    let source = root.join("movie with spaces ü.mkv");
    let result = std::process::Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=128x96:rate=24:duration=1",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-pix_fmt",
            "yuv420p",
        ])
        .arg(&source)
        .output()
        .expect("FFmpeg is required for output-safety regressions");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    source
}

async fn queue_item(source: &Path) -> QueueItem {
    let probe = crate::probe::probe_file(source.to_str().unwrap(), &NoopSink)
        .await
        .unwrap();
    make_queue_item(source.to_str().unwrap(), QueueItemStatus::Pending, probe)
}

struct Sink<'a> {
    control: &'a CancellableBatchControl,
    cancel_on_command: bool,
    block_install: Option<PathBuf>,
    logs: Mutex<Vec<String>>,
}

impl EventSink for Sink<'_> {
    fn log(&self, m: &str) {
        self.logs.lock().unwrap().push(m.into());
    }
    fn processing_stage(&self, stage: &str, _: &str, _: Option<f64>) {
        if stage == "finalizing" {
            if let Some(path) = &self.block_install {
                std::fs::create_dir_all(path).unwrap();
            }
        }
    }
    fn batch_command(&self, _: &str) {
        if self.cancel_on_command {
            self.control.cancel_current.store(true, Ordering::SeqCst);
        }
    }
    fn file_progress(&self, _: f64, _: f64, _: f64, _: Option<(u8, u8)>) {}
    fn batch_progress(&self, _: u32, _: usize) {}
    fn batch_status(&self, _: &str) {}
    fn queue_item_updated(&self, _: usize, _: &str) {}
    fn queue_item_probed(&self, _: usize, _: &QueueItem) {}
    fn batch_started(&self) {}
    fn batch_finished(&self, _: u32, _: u32, _: u32, _: &str) {}
    fn ffmpeg_stderr(&self, _: &str) {}
    fn ffmpeg_download_progress(&self, _: &str) {}
    fn toast(&self, _: &str) {}
    fn post_batch(&self, _: &str, _: u32) {}
}

#[tokio::test(flavor = "multi_thread")]
async fn same_output_folder_never_deletes_source() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("movie.mkv");
    std::fs::write(&source, b"disposable source sentinel").unwrap();
    let probe = crate::test_helpers::synthetic_probe("h264", 1.0, 1.0, false);
    let mut queue = vec![make_queue_item(
        source.to_str().unwrap(),
        QueueItemStatus::Pending,
        probe,
    )];
    let result = run_encode_loop(
        &NoopSink,
        &CancellableBatchControl::new(),
        &mut queue,
        &settings(root.path()),
        &[],
        None,
        None,
    )
    .await;
    assert_eq!(result, (0, 1, 0, false));
    assert_eq!(
        std::fs::read(source).unwrap(),
        b"disposable source sentinel"
    );
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn ffmpeg_failure_preserves_previous_output_and_source() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source.mkv");
    let output_dir = root.path().join("output");
    std::fs::create_dir(&output_dir).unwrap();
    let output = output_dir.join("source.mkv");
    std::fs::write(&source, b"invalid disposable video").unwrap();
    std::fs::write(&output, b"previous successful output").unwrap();
    let probe = crate::test_helpers::synthetic_probe("h264", 1.0, 1.0, false);
    let mut queue = vec![make_queue_item(
        source.to_str().unwrap(),
        QueueItemStatus::Pending,
        probe,
    )];
    let result = run_encode_loop(
        &NoopSink,
        &CancellableBatchControl::new(),
        &mut queue,
        &settings(&output_dir),
        &[],
        None,
        None,
    )
    .await;
    assert_eq!(result, (0, 1, 0, false));
    assert_eq!(std::fs::read(source).unwrap(), b"invalid disposable video");
    assert_eq!(
        std::fs::read(output).unwrap(),
        b"previous successful output"
    );
    assert_eq!(std::fs::read_dir(output_dir).unwrap().count(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn cancellation_preserves_existing_destination() {
    let root = tempfile::tempdir().unwrap();
    let source = synthetic_source(root.path());
    let original = std::fs::read(&source).unwrap();
    let output_dir = root.path().join("output");
    std::fs::create_dir(&output_dir).unwrap();
    let output = output_dir.join(source.file_name().unwrap());
    std::fs::write(&output, b"previous successful output").unwrap();
    let control = CancellableBatchControl::new();
    let sink = Sink {
        control: &control,
        cancel_on_command: true,
        block_install: None,
        logs: Mutex::default(),
    };
    let mut queue = vec![queue_item(&source).await];
    run_encode_loop(
        &sink,
        &control,
        &mut queue,
        &settings(&output_dir),
        &[],
        None,
        None,
    )
    .await;
    assert_eq!(queue[0].status, QueueItemStatus::Cancelled);
    assert_eq!(std::fs::read(source).unwrap(), original);
    assert_eq!(
        std::fs::read(output).unwrap(),
        b"previous successful output"
    );
    assert_eq!(std::fs::read_dir(output_dir).unwrap().count(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn installation_failure_is_failed_and_preserves_encode_for_recovery() {
    let root = tempfile::tempdir().unwrap();
    let source = synthetic_source(root.path());
    let original = std::fs::read(&source).unwrap();
    let output_dir = root.path().join("output");
    std::fs::create_dir(&output_dir).unwrap();
    let output = output_dir.join(source.file_name().unwrap());
    let control = CancellableBatchControl::new();
    let sink = Sink {
        control: &control,
        cancel_on_command: false,
        block_install: Some(output.clone()),
        logs: Mutex::default(),
    };
    let mut queue = vec![queue_item(&source).await];
    let result = run_encode_loop(
        &sink,
        &control,
        &mut queue,
        &settings(&output_dir),
        &[],
        None,
        None,
    )
    .await;
    assert_eq!(result, (0, 1, 0, false));
    assert_eq!(queue[0].status, QueueItemStatus::Failed);
    assert_eq!(std::fs::read(source).unwrap(), original);
    assert!(output.is_dir());
    assert!(sink
        .logs
        .lock()
        .unwrap()
        .iter()
        .any(|m| m.contains("RECOVERY:")));
    let retained = std::fs::read_dir(output_dir)
        .unwrap()
        .filter_map(Result::ok)
        .find(|entry| entry.path().is_file())
        .unwrap()
        .path();
    assert!(
        crate::probe::probe_file_for_validation(retained.to_str().unwrap(), &NoopSink)
            .await
            .is_ok()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn success_installs_playable_output_and_replacement_preserves_existing_backup() {
    let root = tempfile::tempdir().unwrap();
    let source = synthetic_source(root.path());
    let original = std::fs::read(&source).unwrap();
    let output_dir = root.path().join("output");
    std::fs::create_dir(&output_dir).unwrap();
    let output = output_dir.join(source.file_name().unwrap());
    std::fs::write(&output, b"previous output").unwrap();
    let mut queue = vec![queue_item(&source).await];
    let result = run_encode_loop(
        &NoopSink,
        &CancellableBatchControl::new(),
        &mut queue,
        &settings(&output_dir),
        &[],
        None,
        None,
    )
    .await;
    assert_eq!(result, (1, 0, 0, false));
    assert_eq!(std::fs::read(&source).unwrap(), original);
    assert!(
        crate::probe::probe_file_for_validation(output.to_str().unwrap(), &NoopSink)
            .await
            .is_ok()
    );
    assert_eq!(std::fs::read_dir(output_dir).unwrap().count(), 1);
    let backup = source.with_file_name(format!(
        "{}.histv-bak",
        source.file_name().unwrap().to_string_lossy()
    ));
    std::fs::write(&backup, b"existing recovery backup").unwrap();
    let mut queue = vec![queue_item(&source).await];
    let mut replace = settings(root.path());
    replace.output_mode = "replace".into();
    assert_eq!(
        run_encode_loop(
            &NoopSink,
            &CancellableBatchControl::new(),
            &mut queue,
            &replace,
            &[],
            None,
            None
        )
        .await,
        (1, 0, 0, false)
    );
    assert!(
        crate::probe::probe_file_for_validation(source.to_str().unwrap(), &NoopSink)
            .await
            .is_ok()
    );
    assert_eq!(std::fs::read(backup).unwrap(), b"existing recovery backup");
}

struct SkipOverwrite;

impl BatchControl for SkipOverwrite {
    fn should_cancel_current(&self) -> bool {
        false
    }
    fn should_cancel_all(&self) -> bool {
        false
    }
    fn is_paused(&self) -> bool {
        false
    }
    fn clear_cancel_current(&self) {}
    fn overwrite_always(&self) -> bool {
        false
    }
    fn set_overwrite_always(&self) {}
    fn overwrite_prompt(&self, _: &str) -> String {
        "skip".into()
    }
    fn hw_fallback_offered(&self) -> bool {
        false
    }
    fn set_hw_fallback_offered(&self) {}
    fn fallback_prompt(&self, _: &str) -> String {
        "no".into()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn replacement_cannot_silently_overwrite_sibling_with_another_extension() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("movie.avi");
    let sibling = root.path().join("movie.mkv");
    std::fs::write(&source, b"source sentinel").unwrap();
    std::fs::write(&sibling, b"existing sibling output").unwrap();
    let mut queue = vec![make_queue_item(
        source.to_str().unwrap(),
        QueueItemStatus::Pending,
        crate::test_helpers::synthetic_probe("h264", 1.0, 1.0, false),
    )];
    let mut replace = settings(root.path());
    replace.output_mode = "replace".into();
    let result = run_encode_loop(
        &NoopSink,
        &SkipOverwrite,
        &mut queue,
        &replace,
        &[],
        None,
        None,
    )
    .await;
    assert_eq!(result, (0, 0, 1, false));
    assert_eq!(std::fs::read(source).unwrap(), b"source sentinel");
    assert_eq!(std::fs::read(sibling).unwrap(), b"existing sibling output");
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 2);
}

#[test]
fn metadata_failure_recovery_survives_pending_guard_cleanup() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source.mkv");
    let output = root.path().join("output.mkv");
    std::fs::write(&source, b"source sentinel").unwrap();
    std::fs::write(&output, b"previous output").unwrap();
    let transaction =
        crate::output_transaction::OutputTransaction::new(&source, &output, false).unwrap();
    std::fs::write(transaction.pending_path(), b"completed encode for recovery").unwrap();
    let mut queue = vec![make_queue_item(
        source.to_str().unwrap(),
        QueueItemStatus::Encoding,
        crate::test_helpers::synthetic_probe("hevc", 8.0, 1.0, true),
    )];
    assert!(!metadata_failure(
        0,
        &mut queue,
        Some(transaction.pending_path()),
        "packaging failure",
        &NoopSink
    ));
    drop(transaction);
    assert_eq!(std::fs::read(source).unwrap(), b"source sentinel");
    assert_eq!(std::fs::read(output).unwrap(), b"previous output");
    let recovery = std::fs::read_dir(root.path())
        .unwrap()
        .filter_map(Result::ok)
        .find(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .contains("hdr10plus-failed-encoded")
        })
        .unwrap()
        .path();
    assert_eq!(
        std::fs::read(recovery).unwrap(),
        b"completed encode for recovery"
    );
}

struct FallbackControl {
    offered: std::sync::atomic::AtomicBool,
    accept: bool,
}

impl BatchControl for FallbackControl {
    fn should_cancel_current(&self) -> bool {
        false
    }
    fn should_cancel_all(&self) -> bool {
        false
    }
    fn is_paused(&self) -> bool {
        false
    }
    fn clear_cancel_current(&self) {}
    fn overwrite_always(&self) -> bool {
        true
    }
    fn set_overwrite_always(&self) {}
    fn overwrite_prompt(&self, _: &str) -> String {
        "yes".into()
    }
    fn hw_fallback_offered(&self) -> bool {
        self.offered.load(Ordering::SeqCst)
    }
    fn set_hw_fallback_offered(&self) {
        self.offered.store(true, Ordering::SeqCst);
    }
    fn fallback_prompt(&self, _: &str) -> String {
        if self.accept { "yes" } else { "no" }.into()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn repeated_hw_failure_never_enters_finalization_or_overwrites_output() {
    let root = tempfile::tempdir().unwrap();
    let source = synthetic_source(root.path());
    let output_dir = root.path().join("output");
    std::fs::create_dir(&output_dir).unwrap();
    let output = output_dir.join(source.file_name().unwrap());
    std::fs::write(&output, b"previous successful output").unwrap();
    let mut queue = vec![queue_item(&source).await];
    let mut settings = settings(&output_dir);
    settings.threshold = 0.001; // Force FFmpeg to invoke the selected encoder.
    settings.video_encoder = "nonexistent_test_hardware_encoder".into();
    let control = FallbackControl {
        offered: true.into(),
        accept: false,
    };
    let sink = crate::test_helpers::RecordingSink::new();
    assert_eq!(
        run_encode_loop(&sink, &control, &mut queue, &settings, &[], None, None).await,
        (0, 1, 0, false)
    );
    assert_eq!(queue[0].status, QueueItemStatus::Failed);
    assert!(!sink
        .stages
        .lock()
        .unwrap()
        .iter()
        .any(|(stage, _, _)| stage == "finalizing"));
    assert_eq!(
        std::fs::read(output).unwrap(),
        b"previous successful output"
    );
    assert!(source.exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn declined_fallback_is_a_counted_failure_not_batch_cancellation() {
    let root = tempfile::tempdir().unwrap();
    let source = synthetic_source(root.path());
    let output_dir = root.path().join("output");
    std::fs::create_dir(&output_dir).unwrap();
    let mut queue = vec![queue_item(&source).await, queue_item(&source).await];
    let mut settings = settings(&output_dir);
    settings.threshold = 0.001;
    settings.video_encoder = "nonexistent_test_hardware_encoder".into();
    let control = FallbackControl {
        offered: false.into(),
        accept: false,
    };
    assert_eq!(
        run_encode_loop(&NoopSink, &control, &mut queue, &settings, &[], None, None).await,
        (0, 1, 0, false)
    );
    assert!(control.hw_fallback_offered());
    assert_eq!(queue[0].status, QueueItemStatus::Failed);
    assert_eq!(queue[1].status, QueueItemStatus::Pending);
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_software_retry_counts_failure_and_stops_without_cancellation() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("invalid.mkv");
    std::fs::write(&source, b"invalid disposable input").unwrap();
    let output_dir = root.path().join("output");
    std::fs::create_dir(&output_dir).unwrap();
    let probe = crate::test_helpers::synthetic_probe("h264", 8.0, 1.0, false);
    let item = make_queue_item(source.to_str().unwrap(), QueueItemStatus::Pending, probe);
    let mut queue = vec![item.clone(), item];
    let mut settings = settings(&output_dir);
    settings.threshold = 1.0;
    settings.video_encoder = "nonexistent_test_hardware_encoder".into();
    let control = FallbackControl {
        offered: false.into(),
        accept: true,
    };
    assert_eq!(
        run_encode_loop(&NoopSink, &control, &mut queue, &settings, &[], None, None).await,
        (0, 1, 0, false)
    );
    assert!(control.hw_fallback_offered());
    assert_eq!(queue[0].status, QueueItemStatus::Failed);
    assert_eq!(queue[1].status, QueueItemStatus::Pending);
    assert_eq!(std::fs::read(source).unwrap(), b"invalid disposable input");
    assert_eq!(std::fs::read_dir(output_dir).unwrap().count(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn remote_install_failure_retains_completed_encode_after_wave_cleanup() {
    struct BlockDelivery {
        destination: PathBuf,
        logs: Mutex<Vec<String>>,
    }
    impl EventSink for BlockDelivery {
        fn processing_stage(&self, _: &str, message: &str, _: Option<f64>) {
            if message == "Delivering completed output..." {
                std::fs::create_dir(&self.destination).unwrap();
            }
        }
        fn log(&self, m: &str) {
            self.logs.lock().unwrap().push(m.into());
        }
        fn file_progress(&self, _: f64, _: f64, _: f64, _: Option<(u8, u8)>) {}
        fn batch_progress(&self, _: u32, _: usize) {}
        fn batch_status(&self, _: &str) {}
        fn queue_item_updated(&self, _: usize, status: &str) {
            assert_ne!(status, "Done");
        }
        fn queue_item_probed(&self, _: usize, _: &QueueItem) {}
        fn batch_started(&self) {}
        fn batch_finished(&self, done: u32, failed: u32, _: u32, _: &str) {
            assert_eq!((done, failed), (0, 1));
        }
        fn ffmpeg_stderr(&self, _: &str) {}
        fn batch_command(&self, _: &str) {}
        fn ffmpeg_download_progress(&self, _: &str) {}
        fn toast(&self, _: &str) {}
        fn post_batch(&self, _: &str, _: u32) {}
    }
    let root = tempfile::tempdir().unwrap();
    let source = synthetic_source(root.path());
    let original = std::fs::read(&source).unwrap();
    let destination = root.path().join("output").join(source.file_name().unwrap());
    let sink = BlockDelivery {
        destination,
        logs: Mutex::default(),
    };
    let mut queue = vec![queue_item(&source).await];
    let mut settings = settings(root.path());
    settings.output_mode = "beside".into();
    let plan = vec![WaveItem::Wave {
        indices: vec![0],
        total_stage_bytes: original.len() as u64,
    }];
    let result = run_encode_loop(
        &sink,
        &CancellableBatchControl::new(),
        &mut queue,
        &settings,
        &[],
        Some(plan),
        None,
    )
    .await;
    assert_eq!(result, (0, 1, 0, false));
    assert_eq!(queue[0].status, QueueItemStatus::Failed);
    assert_eq!(queue[0].full_path, source.to_str().unwrap());
    assert_eq!(std::fs::read(source).unwrap(), original);
    let recovery = sink
        .logs
        .lock()
        .unwrap()
        .iter()
        .find_map(|line| {
            line.strip_prefix("  RECOVERY: Encoded file preserved at: ")
                .map(PathBuf::from)
        })
        .unwrap();
    assert!(recovery.is_file());
    assert!(
        crate::probe::probe_file_for_validation(recovery.to_str().unwrap(), &NoopSink)
            .await
            .is_ok()
    );
    // The test owns this unique wave directory; remove its disposable recovery.
    std::fs::remove_file(&recovery).unwrap();
    let output_dir = recovery.parent().unwrap();
    std::fs::remove_dir(output_dir).unwrap();
    let input_dir = output_dir.parent().unwrap();
    std::fs::remove_dir(input_dir).unwrap();
    std::fs::remove_dir(input_dir.parent().unwrap()).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires FFmpeg with 10-bit libx265/E-AC3 and MP4Box; no GPU required"]
async fn hdr10plus_mode_keeps_hdr10_after_clean_scan_but_rejects_missing_advertised_metadata() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("hdr10-only.mkv");
    let generated = std::process::Command::new("ffmpeg").args([
        "-v", "error", "-y", "-f", "lavfi", "-i",
        "testsrc2=size=384x216:rate=24:duration=1", "-f", "lavfi", "-i",
        "sine=frequency=440:sample_rate=48000:duration=1",
        "-c:v", "libx265", "-preset", "ultrafast", "-pix_fmt", "yuv420p10le",
        "-x265-params", "pools=2:frame-threads=2:hdr10=1:colorprim=bt2020:transfer=smpte2084:colormatrix=bt2020nc:master-display=G(8500,39850)B(6550,2300)R(35400,14600)WP(15635,16450)L(10000000,1):max-cll=1000,400:log-level=error",
        "-color_primaries", "bt2020", "-color_trc", "smpte2084", "-colorspace", "bt2020nc",
        "-c:a", "eac3", "-b:a", "384k",
    ]).arg(&source).output().unwrap();
    assert!(
        generated.status.success(),
        "{}",
        String::from_utf8_lossy(&generated.stderr)
    );
    let original = std::fs::read(&source).unwrap();
    let item = queue_item(&source).await;
    assert!(item.probe.is_hdr && !item.probe.has_hdr10plus);
    for advertised in [false, true] {
        let folder = root.path().join(if advertised {
            "advertised-but-missing"
        } else {
            "clean-hdr10"
        });
        std::fs::create_dir(&folder).unwrap();
        let output = folder.join(source.file_name().unwrap());
        if advertised {
            std::fs::write(&output, b"previous output").unwrap();
        }
        let mut item = item.clone();
        item.probe.has_hdr10plus = advertised;
        let mut queue = vec![item];
        let mut config = settings(&folder);
        config.codec_family = "hevc".into();
        config.video_encoder = "libx265".into();
        config.hdr_mode = Some(HdrMode::Hdr10Plus);
        let sink = crate::test_helpers::RecordingSink::new();
        let result = run_encode_loop(
            &sink,
            &crate::test_helpers::NoopBatchControl,
            &mut queue,
            &config,
            &[],
            None,
            None,
        )
        .await;
        if advertised {
            assert_eq!(result, (0, 1, 0, false));
            assert_eq!(std::fs::read(&output).unwrap(), b"previous output");
        } else {
            assert_eq!(result, (1, 0, 0, false), "{:?}", sink.take_logs());
            assert!(sink
                .take_logs()
                .iter()
                .any(|line| line.contains("full source scan found no HDR10+ metadata")));
            assert!(!sink
                .stages
                .lock()
                .unwrap()
                .iter()
                .any(|(stage, _, _)| stage == "reinserting"));
            let probe =
                crate::probe::probe_file_for_validation(output.to_str().unwrap(), &NoopSink)
                    .await
                    .unwrap();
            queue[0]
                .probe
                .video_format
                .verify(&probe.video_format, false)
                .unwrap();
            assert_eq!(probe.video_format.bit_depth, 10);
            assert_eq!(probe.color_transfer, "smpte2084");
            let audio_hash = |path: &Path| {
                let output = std::process::Command::new("ffmpeg")
                    .args(["-v", "error", "-i"])
                    .arg(path)
                    .args([
                        "-map",
                        "0:a",
                        "-c",
                        "copy",
                        "-f",
                        "streamhash",
                        "-hash",
                        "sha256",
                        "-",
                    ])
                    .output()
                    .unwrap();
                assert!(output.status.success());
                output.stdout
            };
            assert_eq!(audio_hash(&source), audio_hash(&output));
        }
        assert_eq!(std::fs::read(&source).unwrap(), original);
        assert_eq!(std::fs::read_dir(folder).unwrap().count(), 1);
    }
    let invalid = root.path().join("unreadable-bitstream.mkv");
    std::fs::write(&invalid, b"invalid disposable video").unwrap();
    let folder = root.path().join("extraction-failure");
    std::fs::create_dir(&folder).unwrap();
    let output = folder.join("unreadable-bitstream.mkv");
    std::fs::write(&output, b"previous successful output").unwrap();
    let mut invalid_item = queue_item(&source).await;
    invalid_item.full_path = invalid.to_string_lossy().into_owned();
    invalid_item.file_name = "unreadable-bitstream.mkv".into();
    invalid_item.base_name = "unreadable-bitstream".into();
    let mut queue = vec![invalid_item];
    let mut config = settings(&folder);
    config.codec_family = "hevc".into();
    config.video_encoder = "libx265".into();
    config.hdr_mode = Some(HdrMode::Hdr10Plus);
    assert_eq!(
        run_encode_loop(
            &NoopSink,
            &crate::test_helpers::NoopBatchControl,
            &mut queue,
            &config,
            &[],
            None,
            None
        )
        .await,
        (0, 1, 0, false)
    );
    assert_eq!(
        std::fs::read(output).unwrap(),
        b"previous successful output"
    );
    assert_eq!(std::fs::read(invalid).unwrap(), b"invalid disposable video");
}

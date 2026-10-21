//! Short, opt-in physical GPU regression. No existing media are modified.
use hdr10plus::{hevc::encode_hdr10plus_nal, metadata::Hdr10PlusMetadata};
use histv_lib::{hdr10plus_pipeline as hdr, EventSink, QueueItem};
use sha2::{Digest, Sha256};
use std::{path::Path, process::Command};

struct Sink;
impl EventSink for Sink {
    fn log(&self, m: &str) {
        eprintln!("{m}");
    }
    fn file_progress(&self, _: f64, _: f64, _: f64, _: Option<(u8, u8)>) {}
    fn batch_progress(&self, _: u32, _: usize) {}
    fn batch_status(&self, _: &str) {}
    fn queue_item_updated(&self, _: usize, _: &str) {}
    fn queue_item_probed(&self, _: usize, _: &QueueItem) {}
    fn batch_started(&self) {}
    fn batch_finished(&self, _: u32, _: u32, _: u32, _: &str) {}
    fn ffmpeg_stderr(&self, _: &str) {}
    fn batch_command(&self, _: &str) {}
    fn ffmpeg_download_progress(&self, _: &str) {}
    fn toast(&self, _: &str) {}
    fn post_batch(&self, _: &str, _: u32) {}
}

fn run(command: &mut Command) -> std::process::Output {
    let result = command.output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    result
}
fn packets(path: &Path, selector: &str) -> serde_json::Value {
    serde_json::from_slice(
        &run(Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-select_streams",
                selector,
                "-show_packets",
                "-show_data_hash",
                "sha256",
                "-show_entries",
                "packet=data_hash",
                "-of",
                "json",
            ])
            .arg(path))
        .stdout,
    )
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Linux VAAPI, x265 and bundled MP4Box beside test executable"]
async fn opt_in_preserves_sparse_late_metadata_with_cpu_or_gpu_decode() {
    let root = tempfile::tempdir().unwrap();
    let plain = root.path().join("plain.mkv");
    let subtitles = root.path().join("captions.srt");
    std::fs::write(
        &subtitles,
        "1\n00:00:00,000 --> 00:00:01,500\nHDR10+ regression\n",
    )
    .unwrap();
    run(Command::new("ffmpeg").args(["-v","error","-y","-f","lavfi","-i",
        "testsrc2=size=1920x1080:rate=24:duration=2","-f","lavfi","-i",
        "sine=frequency=440:duration=2","-i"]).arg(&subtitles).args([
        "-map","0:v","-map","1:a","-map","2:s","-c:v","libx265","-preset","ultrafast",
        "-pix_fmt","yuv420p10le","-x265-params",
        "pools=2:frame-threads=2:bframes=3:hdr10=1:repeat-headers=1:colorprim=bt2020:transfer=smpte2084:colormatrix=bt2020nc:master-display=G(8500,39850)B(6550,2300)R(35400,14600)WP(15635,16450)L(10000000,1):max-cll=1000,400",
        "-color_primaries","bt2020","-color_trc","smpte2084","-colorspace","bt2020nc",
        "-c:a","flac","-c:s","srt","-metadata:s:a:0","language=eng",
        "-metadata:s:s:0","language=spa"]).arg(&plain));
    let expected = hdr::ExtractedHdr10Plus {
        frames: (0..48)
            .map(|frame| {
                if frame < 25 {
                    return vec![];
                }
                let metadata = Hdr10PlusMetadata {
                    itu_t_t35_country_code: 0xb5,
                    itu_t_t35_terminal_provider_code: 0x003c,
                    itu_t_t35_terminal_provider_oriented_code: 1,
                    application_identifier: 4,
                    application_version: 1,
                    num_windows: 1,
                    targeted_system_display_maximum_luminance: 500,
                    average_maxrgb: 100 + frame,
                    ..Default::default()
                };
                vec![encode_hdr10plus_nal(&metadata, false).unwrap()]
            })
            .collect(),
    };
    let source = root.path().join("late-hdr10plus.mkv");
    assert!(
        hdr::inject_hdr10plus(&plain, &source, &expected, &Sink)
            .await
            .unwrap()
            .success
    );
    hdr::verify_output(&source, &expected, &Sink).await.unwrap();
    // A delayed video track must stay delayed relative to copied audio/subs.
    let delayed = root.path().join("delayed.mkv");
    run(Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-copyts", "-itsoffset", "0.25", "-i"])
        .arg(&plain)
        .arg("-i")
        .arg(&plain)
        .args(["-map", "0:v", "-map", "1:a", "-map", "1:s", "-c", "copy"])
        .arg(&delayed));
    let delayed_preserved = root.path().join("delayed-preserved.mkv");
    assert!(
        hdr::inject_hdr10plus(&delayed, &delayed_preserved, &expected, &Sink)
            .await
            .unwrap()
            .success
    );
    let timestamps = |path: &Path| -> serde_json::Value {
        serde_json::from_slice(
            &run(Command::new("ffprobe")
                .args([
                    "-v",
                    "error",
                    "-show_packets",
                    "-show_entries",
                    "packet=stream_index,pts_time",
                    "-of",
                    "json",
                ])
                .arg(path))
            .stdout,
        )
        .unwrap()
    };
    let original_times = timestamps(&delayed);
    let preserved_times = timestamps(&delayed_preserved);
    // Packet interleaving may change, but each stream's presentation timeline may not.
    for stream in 0..3 {
        let stream_times = |value: &serde_json::Value| -> Vec<f64> {
            value["packets"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|p| p["stream_index"] == stream)
                .map(|p| p["pts_time"].as_str().unwrap().parse().unwrap())
                .collect()
        };
        let before = stream_times(&original_times);
        let after = stream_times(&preserved_times);
        assert_eq!(before.len(), after.len());
        for (a, b) in before.iter().zip(&after) {
            assert!(
                (a - b).abs() <= 0.0011,
                "Stream {stream} timestamp shifted: {a} -> {b}"
            );
        }
    }
    // Metadata insertion must not alter any decoded pixel.
    let pixels = |path: &Path| {
        run(Command::new("ffmpeg")
            .args(["-v", "error", "-i"])
            .arg(path)
            .args(["-map", "0:v", "-f", "hash", "-hash", "sha256", "-"]))
        .stdout
    };
    assert_eq!(pixels(&plain), pixels(&source));
    let mp4_plain = root.path().join("plain.mp4");
    run(Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-i"])
        .arg(&plain)
        .args(["-map", "0:v", "-c", "copy", "-tag:v", "hev1"])
        .arg(&mp4_plain));
    let mp4_preserved = root.path().join("preserved.mp4");
    assert!(
        hdr::inject_hdr10plus(&mp4_plain, &mp4_preserved, &expected, &Sink)
            .await
            .unwrap()
            .success
    );
    hdr::verify_output(&mp4_preserved, &expected, &Sink)
        .await
        .unwrap();
    assert_eq!(pixels(&plain), pixels(&mp4_preserved));
    let mut misaligned = hdr::ExtractedHdr10Plus {
        frames: expected.frames.clone(),
    };
    misaligned.frames.swap(25, 26);
    assert!(hdr::verify_output(&source, &misaligned, &Sink)
        .await
        .is_err());
    assert!(source.exists());
    let hash = Sha256::digest(std::fs::read(&source).unwrap());
    assert!(hdr::prepare(&plain, Some(root.path()), "hevc", &Sink)
        .await
        .unwrap()
        .is_none());
    assert!(hdr::prepare(&source, Some(root.path()), "h264", &Sink)
        .await
        .err()
        .unwrap()
        .contains("requires HEVC"));
    for enabled in [false, true] {
        for gpu in [false, true] {
            let dest = root.path().join(format!("output-{enabled}-{gpu}"));
            let mut command = Command::new(env!("CARGO_BIN_EXE_histv-cli"));
            command
                .args([
                    "--codec",
                    "hevc",
                    "--encoder",
                    "hevc_vaapi",
                    "--bitrate",
                    "0.1",
                    "--audio",
                    "copy",
                    "--fallback",
                    "no",
                    "--overwrite",
                    "yes",
                    "--output",
                ])
                .arg(&dest);
            if enabled {
                command.arg("--preserve-hdr10plus");
            }
            if gpu {
                command.arg("--hardware-decoding");
            }
            let start = std::time::Instant::now();
            let result = run(command.arg(&source));
            let log = format!(
                "{}{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            eprintln!(
                "OPTIONAL HDR10+ enabled={enabled} gpu_decode={gpu}: {:.3}s\n{log}",
                start.elapsed().as_secs_f64()
            );
            assert!(log.contains("-c:v hevc_vaapi"), "{log}");
            if let Ok(device) = std::env::var("HISTV_EXPECT_VAAPI_DEVICE") {
                assert!(log.contains(&device), "{log}");
            }
            assert_eq!(log.contains("Verified exact metadata"), enabled, "{log}");
            assert_eq!(log.contains("Extracting HEVC bitstream"), enabled, "{log}");
            let files: Vec<_> = std::fs::read_dir(&dest)
                .unwrap()
                .map(|p| p.unwrap().path())
                .collect();
            assert_eq!(files.len(), 1, "Temporary artifacts left: {files:?}");
            let output = &files[0];
            assert_eq!(output.extension().unwrap(), "mkv");
            let actual = hdr::extract_hdr10plus_if_present(output, &Sink)
                .await
                .unwrap();
            if enabled {
                assert_eq!(actual.unwrap().frames, expected.frames);
            } else {
                assert!(actual.is_none());
            }
            assert_eq!(packets(&source, "a"), packets(output, "a"));
            assert_eq!(packets(&source, "s"), packets(output, "s"));
            let probe: serde_json::Value = serde_json::from_slice(
                &run(Command::new("ffprobe")
                    .args(["-v", "error", "-show_streams", "-of", "json"])
                    .arg(output))
                .stdout,
            )
            .unwrap();
            let video = &probe["streams"][0];
            assert_eq!(video["pix_fmt"], "yuv420p10le");
            assert_eq!(video["color_primaries"], "bt2020");
            assert_eq!(video["color_transfer"], "smpte2084");
            let frames: serde_json::Value = serde_json::from_slice(
                &run(Command::new("ffprobe")
                    .args([
                        "-v",
                        "error",
                        "-select_streams",
                        "v:0",
                        "-show_frames",
                        "-of",
                        "json",
                    ])
                    .arg(output))
                .stdout,
            )
            .unwrap();
            let frames = frames["frames"].as_array().unwrap();
            assert_eq!(frames.len(), 48);
            for (index, frame) in frames.iter().enumerate() {
                let side = frame["side_data_list"].as_array();
                let has_dynamic = side.is_some_and(|entries| {
                    entries.iter().any(|entry| {
                        entry["side_data_type"]
                            .as_str()
                            .unwrap_or("")
                            .contains("2094-40")
                    })
                });
                assert_eq!(
                    has_dynamic,
                    enabled && index >= 25,
                    "Independent FFprobe metadata mismatch at frame {index}"
                );
            }
            assert_eq!(probe["streams"][1]["tags"]["language"], "eng");
            assert_eq!(probe["streams"][2]["tags"]["language"], "spa");
            let decode = run(Command::new("ffmpeg")
                .args(["-v", "error", "-xerror", "-i"])
                .arg(output)
                .args(["-map", "0:v", "-f", "null", "-"]));
            assert!(
                decode.stderr.is_empty(),
                "{}",
                String::from_utf8_lossy(&decode.stderr)
            );
            assert_eq!(Sha256::digest(std::fs::read(&source).unwrap()), hash);
        }
    }
}

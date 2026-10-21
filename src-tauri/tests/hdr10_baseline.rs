//! Opt-in physical media regression: set HISTV_HDR10_TEST_CASES to the path of
//! a JSON array of {"name":..., "path":...}, then run with --ignored.
//! All outputs and logs are isolated in a TempDir, never beside the source.
use std::{path::Path, process::Command};

fn probe(path: &Path, count: bool) -> serde_json::Value {
    let mut command = Command::new("ffprobe");
    command.args([
        "-v",
        "error",
        "-show_streams",
        "-show_format",
        "-of",
        "json",
    ]);
    if count {
        command.arg("-count_frames");
    }
    let result = command.arg(path).output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(result.stderr.is_empty());
    serde_json::from_slice(&result.stdout).unwrap()
}

fn copied_payloads(path: &Path, selector: &str) -> std::collections::BTreeMap<u64, Vec<String>> {
    let result = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            selector,
            "-show_packets",
            "-show_data_hash",
            "sha256",
            "-show_entries",
            "packet=stream_index,data_hash",
            "-of",
            "json",
        ])
        .arg(path)
        .output()
        .unwrap();
    assert!(result.status.success());
    let json: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    let mut streams = std::collections::BTreeMap::<u64, Vec<String>>::new();
    for packet in json["packets"].as_array().unwrap() {
        streams
            .entry(packet["stream_index"].as_u64().unwrap())
            .or_default()
            .push(packet["data_hash"].as_str().unwrap().to_owned());
    }
    streams
}

#[test]
#[ignore = "requires Linux VAAPI hardware and HISTV_HDR10_TEST_CASES media fixtures"]
fn real_sources_keep_hdr10_pixels_tracks_and_container_without_metadata_passes() {
    let cases: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(
            std::env::var("HISTV_HDR10_TEST_CASES").expect("Set the test cases JSON file path"),
        )
        .unwrap(),
    )
    .unwrap();
    let root = tempfile::tempdir().unwrap();
    for case in cases.as_array().unwrap() {
        let source = Path::new(case["path"].as_str().unwrap());
        let original = probe(source, true);
        for gpu in [false, true] {
            let dest = root
                .path()
                .join(format!("{}-{gpu}", case["name"].as_str().unwrap()));
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
            if gpu {
                command.arg("--hardware-decoding");
            }
            let started = std::time::Instant::now();
            let result = command.arg(source).output().unwrap();
            let log = format!(
                "{}{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            assert!(result.status.success(), "{log}");
            assert!(log.contains("-c:v hevc_vaapi"), "{log}");
            // Metadata paths are absent from the application, not just skipped
            // based on whether the first decoded frame happened to advertise HDR.
            for absent in [
                "Extracting HEVC",
                "Scanning for dynamic",
                "Pre-encoded",
                "MP4Box",
                "RPU extraction",
            ] {
                assert!(
                    !log.contains(absent),
                    "Unexpected full-file metadata work: {log}"
                );
            }
            let outputs: Vec<_> = std::fs::read_dir(&dest)
                .unwrap()
                .map(|v| v.unwrap().path())
                .collect();
            assert_eq!(outputs.len(), 1, "Unexpected output artifacts: {outputs:?}");
            let output = &outputs[0];
            assert_eq!(source.extension(), output.extension());
            let info = probe(output, true);
            let videos = |v: &serde_json::Value| {
                v["streams"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|s| s["codec_type"] == "video")
                    .unwrap()
                    .clone()
            };
            let a = videos(&original);
            let b = videos(&info);
            for key in [
                "pix_fmt",
                "width",
                "height",
                "sample_aspect_ratio",
                "display_aspect_ratio",
                "nb_read_frames",
                "color_primaries",
                "color_transfer",
                "color_space",
                "color_range",
            ] {
                if !a[key].is_null() {
                    assert_eq!(a[key], b[key], "{} {key}", case["name"]);
                }
            }
            let duration = |v: &serde_json::Value| {
                v["format"]["duration"]
                    .as_str()
                    .unwrap()
                    .parse::<f64>()
                    .unwrap()
            };
            let start = |v: &serde_json::Value| {
                v["format"]["start_time"]
                    .as_str()
                    .and_then(|s| s.parse::<f64>().ok())
                    .unwrap_or(0.0)
            };
            assert!(
                ((duration(&original) - start(&original)) - (duration(&info) - start(&info))).abs()
                    < 0.15
            );
            for audio in original["streams"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|s| s["codec_type"] == "audio")
            {
                // Assert explicit Copy bypasses the bitrate cap, including EAC3
                // Atmos above 640 kbps; matching codec names alone is insufficient.
                let audio_index = original["streams"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|s| s["codec_type"] == "audio")
                    .position(|s| s == audio)
                    .unwrap();
                assert!(log.contains(&format!("-c:a:{audio_index} copy")), "{log}");
            }
            let tracks = |v: &serde_json::Value| {
                v["streams"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|s| s["codec_type"] != "video")
                    .map(|s| {
                        (
                            s["codec_type"].clone(),
                            s["codec_name"].clone(),
                            s["tags"]["language"].clone(),
                            s["tags"]["title"].clone(),
                        )
                    })
                    .collect::<Vec<_>>()
            };
            assert_eq!(
                tracks(&original),
                tracks(&info),
                "Track copy/metadata changed"
            );
            for selector in ["a", "s"] {
                assert_eq!(
                    copied_payloads(source, selector),
                    copied_payloads(output, selector),
                    "Copied audio/subtitle payloads changed for {selector}"
                );
            }
            let decoded = Command::new("ffmpeg")
                .args(["-v", "error", "-xerror", "-i"])
                .arg(output)
                .args(["-map", "0:v:0", "-f", "null", "-"])
                .output()
                .unwrap();
            assert!(
                decoded.status.success() && decoded.stderr.is_empty(),
                "{}",
                String::from_utf8_lossy(&decoded.stderr)
            );
            println!(
                "{} gpu_decode={gpu}: {:.3}s; clean {} frames; {}",
                case["name"],
                started.elapsed().as_secs_f64(),
                b["nb_read_frames"],
                output.display()
            );
        }
    }
    if std::env::var_os("HISTV_KEEP_TEST_OUTPUTS").is_some() {
        println!("Retained test outputs: {}", root.keep().display());
    }
}

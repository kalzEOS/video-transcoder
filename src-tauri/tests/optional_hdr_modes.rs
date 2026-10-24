//! Short physical FFmpeg regression for the explicit HDR and SDR modes.
use serde_json::Value;
use std::{path::Path, process::Command};

fn run(command: &mut Command) -> std::process::Output {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn probe(path: &Path) -> Value {
    serde_json::from_slice(
        &run(Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-read_intervals",
                "%+#1",
                "-show_streams",
                "-show_frames",
                "-of",
                "json",
            ])
            .arg(path))
        .stdout,
    )
    .unwrap()
}

fn audio_hash(path: &Path) -> Vec<u8> {
    run(Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(path)
        .args([
            "-map",
            "0:a:0",
            "-c",
            "copy",
            "-f",
            "streamhash",
            "-hash",
            "sha256",
            "-",
        ]))
    .stdout
}

fn static_hdr(frame: &Value) -> Vec<Value> {
    frame["side_data_list"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|entry| {
            matches!(
                entry["side_data_type"].as_str(),
                Some("Mastering display metadata" | "Content light level metadata")
            )
        })
        .cloned()
        .collect()
}

#[test]
#[ignore = "requires FFmpeg with libx265/E-AC3 and the cli feature; no GPU required"]
fn hdr_and_sdr_choices_preserve_audio_and_never_silently_restore_source_hdr() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source.mkv");
    run(Command::new("ffmpeg").args([
        "-v", "error", "-y", "-f", "lavfi", "-i",
        "testsrc2=size=384x216:rate=24:duration=2", "-f", "lavfi", "-i",
        "sine=frequency=440:sample_rate=48000:duration=2", "-map", "0:v", "-map", "1:a",
        "-c:v", "libx265", "-preset", "ultrafast", "-pix_fmt", "yuv420p10le",
        "-x265-params",
        "pools=2:frame-threads=2:bframes=3:repeat-headers=1:hdr10=1:colorprim=bt2020:transfer=smpte2084:colormatrix=bt2020nc:master-display=G(8500,39850)B(6550,2300)R(35400,14600)WP(15635,16450)L(10000000,1):max-cll=1000,400:log-level=error",
        "-color_primaries", "bt2020", "-color_trc", "smpte2084", "-colorspace", "bt2020nc",
        "-c:a", "eac3", "-b:a", "384k",
    ]).arg(&source));

    let original = probe(&source);
    let source_audio = audio_hash(&source);
    for (choice, hdr) in [("--hdr", true), ("--no-hdr", false)] {
        let destination = root.path().join(if hdr { "hdr" } else { "sdr" });
        // Above-source target would normally select stream copy. SDR mode must
        // still transcode and must never remux the untouched HDR source back.
        run(Command::new(env!("CARGO_BIN_EXE_histv-cli"))
            .args([
                choice,
                "--encoder",
                "libx265",
                "--codec",
                "hevc",
                "--bitrate",
                "100",
                "--audio",
                "copy",
                "--fallback",
                "no",
                "--overwrite",
                "yes",
                "--log-level",
                "quiet",
                "--output",
            ])
            .arg(&destination)
            .arg(&source));
        let output = destination.join("source.mkv");
        let video = probe(&output);
        let stream = &video["streams"][0];
        let frame = &video["frames"][0];
        assert_eq!(audio_hash(&output), source_audio);
        if hdr {
            assert_eq!(stream["pix_fmt"], "yuv420p10le");
            assert_eq!(frame["color_transfer"], "smpte2084");
            assert_eq!(frame["color_primaries"], "bt2020");
            assert_eq!(static_hdr(frame).len(), 2);
            assert_eq!(static_hdr(frame), static_hdr(&original["frames"][0]));
        } else {
            assert_eq!(stream["pix_fmt"], "yuv420p");
            for field in ["color_primaries", "color_transfer", "color_space"] {
                assert_eq!(frame[field], "bt709");
            }
            assert!(static_hdr(frame).is_empty());
        }
        run(Command::new("ffmpeg")
            .args(["-v", "error", "-xerror", "-i"])
            .arg(&output)
            .args(["-map", "0:v", "-map", "0:a", "-f", "null", "-"]));
    }
}

#[test]
#[ignore = "requires FFmpeg with 12-bit libx265/libx264 and the cli feature; no GPU required"]
fn twelve_bit_hdr_tonemaps_to_eight_bit_compatibility_sdr() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source.mkv");
    run(Command::new("ffmpeg").args([
        "-v", "error", "-y", "-f", "lavfi", "-i",
        "testsrc2=size=384x216:rate=24:duration=2", "-c:v", "libx265",
        "-preset", "ultrafast", "-pix_fmt", "yuv420p12le", "-x265-params",
        "pools=2:frame-threads=2:hdr10=1:colorprim=bt2020:transfer=smpte2084:colormatrix=bt2020nc:log-level=error",
        "-color_primaries", "bt2020", "-color_trc", "smpte2084", "-colorspace", "bt2020nc",
    ]).arg(&source));
    let original = probe(&source);
    assert_eq!(original["streams"][0]["pix_fmt"], "yuv420p12le");
    assert_eq!(original["frames"][0]["color_transfer"], "smpte2084");

    let destination = root.path().join("output");
    run(Command::new(env!("CARGO_BIN_EXE_histv-cli"))
        .args([
            "--no-hdr",
            "--compat",
            "--encoder",
            "libx264",
            "--codec",
            "h264",
            "--bitrate",
            "100",
            "--fallback",
            "no",
            "--overwrite",
            "yes",
            "--log-level",
            "quiet",
            "--output",
        ])
        .arg(&destination)
        .arg(&source));
    let output = destination.join("source.mp4");
    let video = probe(&output);
    assert_eq!(video["streams"][0]["codec_name"], "h264");
    assert_eq!(video["streams"][0]["pix_fmt"], "yuv420p");
    for field in ["color_primaries", "color_transfer", "color_space"] {
        assert_eq!(video["frames"][0][field], "bt709");
    }
    run(Command::new("ffmpeg")
        .args(["-v", "error", "-xerror", "-i"])
        .arg(&output)
        .args(["-map", "0:v", "-f", "null", "-"]));
}

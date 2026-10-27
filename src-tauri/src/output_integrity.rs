//! Bounded-memory packet validation. Container duration alone can be supplied
//! entirely by audio even when the video ends early.
use crate::events::EventSink;
use std::{collections::BTreeMap, process::Stdio};
use tokio::io::{AsyncBufReadExt, BufReader};

#[derive(Default, Debug)]
struct Stream {
    kind: String,
    codec: String,
    packets: u64,
    first: Option<f64>,
    end: Option<f64>,
    last_dts: Option<f64>,
}

impl Stream {
    fn packet(
        &mut self,
        pts: Option<f64>,
        dts: Option<f64>,
        duration: Option<f64>,
        fps: f64,
    ) -> Result<(), String> {
        self.packets += 1;
        if let Some(dts) = dts {
            if self.last_dts.is_some_and(|last| dts < last - 0.000001) {
                return Err("Non-monotonic packet decode timestamps".into());
            }
            self.last_dts = Some(dts);
        }
        if let Some(pts) = pts {
            self.first = Some(self.first.map_or(pts, |old| old.min(pts)));
            let end = pts + duration.filter(|v| *v > 0.0).unwrap_or(1.0 / fps.max(1.0));
            self.end = Some(self.end.map_or(end, |old| old.max(end)));
        }
        Ok(())
    }

    fn timeline(&self) -> Result<(f64, f64), String> {
        self.first
            .zip(self.end)
            .filter(|(first, end)| self.packets > 0 && end > first)
            .ok_or_else(|| format!("Missing or unusable {} packet timeline", self.kind))
    }
}

#[derive(Default, Debug)]
struct Scan(BTreeMap<usize, Stream>);

impl Scan {
    fn line(&mut self, line: &str, fps: f64) -> Result<(), String> {
        let mut fields = line.split('|');
        let kind = fields.next().unwrap_or_default();
        if !matches!(kind, "packet" | "stream") {
            return Ok(());
        }
        let fields: BTreeMap<_, _> = fields.filter_map(|v| v.split_once('=')).collect();
        let index = fields
            .get(if kind == "packet" {
                "stream_index"
            } else {
                "index"
            })
            .and_then(|v| v.parse::<usize>().ok())
            .ok_or("Invalid packet/stream index")?;
        let stream = self.0.entry(index).or_default();
        if kind == "stream" {
            stream.kind = fields.get("codec_type").unwrap_or(&"").to_string();
            stream.codec = fields.get("codec_name").unwrap_or(&"").to_string();
        } else {
            let number = |name| {
                fields
                    .get(name)
                    .and_then(|v| v.parse::<f64>().ok())
                    .filter(|v| v.is_finite())
            };
            stream.packet(
                number("pts_time"),
                number("dts_time"),
                number("duration_time"),
                fps,
            )?;
        }
        Ok(())
    }

    fn streams(&self, kind: &str) -> Vec<&Stream> {
        self.0.values().filter(|s| s.kind == kind).collect()
    }
}

async fn scan(path: &str, fps: f64, sink: &dyn EventSink) -> Result<Scan, String> {
    let mut child = crate::ffmpeg::ffprobe_command()
        .kill_on_drop(true)
        .args(["-v", "error", "-show_packets", "-show_streams", "-show_entries",
            "packet=stream_index,pts_time,dts_time,duration_time:stream=index,codec_type,codec_name",
            "-of", "compact=p=1:nk=0", path])
        .stdout(Stdio::piped()).stderr(Stdio::piped())
        .spawn().map_err(|e| format!("Cannot start packet validation: {e}"))?;
    let stdout = child.stdout.take().ok_or("Missing validation stdout")?;
    let reader = tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        let mut scan = Scan::default();
        while let Some(line) = lines.next_line().await.map_err(|e| e.to_string())? {
            scan.line(&line, fps)?;
        }
        Ok::<_, String>(scan)
    });
    let result = crate::pipeline_progress::wait_output(child, sink).await;
    let scan = reader.await.map_err(|e| e.to_string())??;
    let result = result?;
    if !result.status.success() || !result.stderr.is_empty() {
        return Err(format!(
            "Packet validation failed: {}",
            String::from_utf8_lossy(&result.stderr)
        ));
    }
    Ok(scan)
}

fn compare(
    source: &Scan,
    output: &Scan,
    audio: &[(usize, bool)],
    subtitles: usize,
    copy_subtitles: bool,
    fps: f64,
) -> Result<(), String> {
    let sv = source.streams("video");
    let ov = output.streams("video");
    if ov.len() != 1 {
        return Err(format!("Expected one video stream; found {}", ov.len()));
    }
    let sv = sv.first().ok_or("Source video stream missing")?;
    let ov = ov[0];
    let (source_start, source_end) = sv.timeline()?;
    let (output_start, output_end) = ov.timeline()?;
    let tolerance = (2.0 / fps.max(1.0)).max(0.1);
    if ((source_end - source_start) - (output_end - output_start)).abs() > tolerance {
        return Err("Video packet timeline is incomplete or changed".into());
    }
    // These codecs have one access unit per container packet. Do not assume
    // that for codecs with superframes or packets containing hidden frames.
    if matches!(sv.codec.as_str(), "h264" | "hevc")
        && matches!(ov.codec.as_str(), "h264" | "hevc")
        && sv.packets != ov.packets
    {
        return Err(format!(
            "Video packet count changed: {} -> {}",
            sv.packets, ov.packets
        ));
    }
    let sa = source.streams("audio");
    let oa = output.streams("audio");
    if oa.len() != audio.len() {
        return Err(format!(
            "Expected {} audio streams; found {}",
            audio.len(),
            oa.len()
        ));
    }
    for ((source_index, copied), output) in audio.iter().zip(oa) {
        let source = sa
            .get(*source_index)
            .ok_or("Expected source audio missing")?;
        let (start, end) = source.timeline()?;
        let (out_start, out_end) = output.timeline()?;
        if *copied && (source.codec != output.codec || source.packets != output.packets) {
            return Err(format!(
                "Copied audio stream {source_index} codec or packet count changed"
            ));
        }
        if ((start - source_start) - (out_start - output_start)).abs() > tolerance
            || ((end - source_start) - (out_end - output_start)).abs() > tolerance
        {
            return Err(format!(
                "Audio stream {source_index} is incomplete or out of sync"
            ));
        }
    }
    let ss = source.streams("subtitle");
    let os = output.streams("subtitle");
    if os.len() != subtitles {
        return Err(format!(
            "Expected {subtitles} subtitle streams; found {}",
            os.len()
        ));
    }
    if copy_subtitles
        && ss.iter().zip(os).any(|(s, o)| {
            !(s.codec == "mov_text" && o.codec == "ass")
                && (s.codec != o.codec || s.packets != o.packets)
        })
    {
        return Err("Copied subtitle codec or packet count changed".into());
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn validate(
    source: &str,
    output: &str,
    audio: &[(usize, bool)],
    subtitles: usize,
    copy_subtitles: bool,
    fps: f64,
    sink: &dyn EventSink,
) -> Result<(), String> {
    sink.processing_stage(
        "finalizing",
        "Checking complete video, audio and subtitle timelines...",
        None,
    );
    let source = scan(source, fps, sink).await?;
    let output = scan(output, fps, sink).await?;
    compare(&source, &output, audio, subtitles, copy_subtitles, fps)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn media(video_packets: usize, audio_packets: usize, audio_offset: f64) -> Scan {
        let mut scan = Scan::default();
        for (index, kind, codec, count, offset) in [
            (0, "video", "hevc", video_packets, 0.0),
            (1, "audio", "eac3", audio_packets, audio_offset),
        ] {
            scan.line(
                &format!("stream|index={index}|codec_name={codec}|codec_type={kind}"),
                24.0,
            )
            .unwrap();
            for n in 0..count {
                scan.line(&format!("packet|stream_index={index}|pts_time={}|dts_time={}|duration_time=0.0416666667", n as f64 / 24.0 + offset, n as f64 / 24.0 + offset), 24.0).unwrap();
            }
        }
        scan
    }
    #[test]
    fn complete_audio_cannot_hide_truncated_video() {
        assert!(compare(
            &media(144, 144, 0.0),
            &media(72, 144, 0.0),
            &[(0, true)],
            0,
            true,
            24.0
        )
        .unwrap_err()
        .contains("Video packet timeline"));
    }
    #[test]
    fn missing_middle_video_packets_are_rejected_even_when_end_matches() {
        let source = media(144, 144, 0.0);
        let mut output = media(144, 144, 0.0);
        output.0.get_mut(&0).unwrap().packets -= 1;
        assert!(compare(&source, &output, &[(0, true)], 0, true, 24.0)
            .unwrap_err()
            .contains("packet count"));
    }
    #[test]
    fn missing_tracks_and_av_sync_shift_are_rejected() {
        let source = media(144, 144, 0.0);
        assert!(
            compare(&source, &media(144, 144, 0.5), &[(0, true)], 0, true, 24.0)
                .unwrap_err()
                .contains("sync")
        );
        let mut output = media(144, 144, 0.0);
        output.0.remove(&1);
        assert!(compare(&source, &output, &[(0, true)], 0, true, 24.0)
            .unwrap_err()
            .contains("audio streams"));
        assert!(compare(&source, &source, &[(0, true)], 1, true, 24.0)
            .unwrap_err()
            .contains("subtitle streams"));
    }
    #[test]
    fn common_timestamp_rebase_is_allowed_and_b_frames_need_not_have_monotonic_pts() {
        let source = media(144, 144, 0.0);
        let mut output = media(144, 144, 0.0);
        for stream in output.0.values_mut() {
            stream.first = stream.first.map(|t| t + 1.0);
            stream.end = stream.end.map(|t| t + 1.0);
        }
        assert!(compare(&source, &output, &[(0, true)], 0, true, 24.0).is_ok());
        let mut stream = Stream::default();
        stream
            .packet(Some(0.08), Some(-0.08), Some(0.04), 24.0)
            .unwrap();
        stream
            .packet(Some(0.0), Some(-0.04), Some(0.04), 24.0)
            .unwrap();
        assert!(stream
            .packet(Some(0.04), Some(-0.1), Some(0.04), 24.0)
            .is_err());
    }

    #[tokio::test]
    async fn real_packet_scan_rejects_video_truncation_hidden_by_complete_audio() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source with spaces ü.mkv");
        let truncated = root.path().join("truncated.mkv");
        for (path, video_duration) in [(&source, "6"), (&truncated, "3")] {
            let result = std::process::Command::new("ffmpeg")
                .args([
                    "-v",
                    "error",
                    "-y",
                    "-f",
                    "lavfi",
                    "-i",
                    &format!("testsrc2=size=128x96:rate=24:duration={video_duration}"),
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=frequency=440:duration=6",
                    "-c:v",
                    "libx264",
                    "-preset",
                    "ultrafast",
                    "-c:a",
                    "eac3",
                ])
                .arg(path)
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
        }
        let sink = crate::test_helpers::NoopSink;
        let src = crate::probe::probe_file_for_validation(source.to_str().unwrap(), &sink)
            .await
            .unwrap();
        let short = crate::probe::probe_file_for_validation(truncated.to_str().unwrap(), &sink)
            .await
            .unwrap();
        assert!((src.duration_secs - short.duration_secs).abs() < 0.01);
        assert!(validate(
            source.to_str().unwrap(),
            source.to_str().unwrap(),
            &[(0, true)],
            0,
            true,
            24.0,
            &sink
        )
        .await
        .is_ok());
        assert!(validate(
            source.to_str().unwrap(),
            truncated.to_str().unwrap(),
            &[(0, true)],
            0,
            true,
            24.0,
            &sink
        )
        .await
        .unwrap_err()
        .contains("Video packet timeline"));
    }
}

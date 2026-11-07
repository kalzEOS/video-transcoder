//! Subtitle/container policy shared by encoding and HDR10+ final remuxing.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackInfo {
    pub subtitles: Vec<SubtitleInfo>,
    pub attachment_count: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SubtitleInfo {
    pub codec: String,
    pub disposition: BTreeMap<String, u8>,
}

impl SubtitleInfo {
    fn muxed_disposition(&self, mp4: bool) -> BTreeMap<String, u8> {
        let mut flags = self.disposition.clone();
        if mp4 {
            // FFmpeg's MOV kind table represents each pair as one DASH role
            // (caption/description), and the demuxer restores BOTH bits.
            // Predict that serialization exactly; never ignore lost flags.
            // https://www.ffmpeg.org/doxygen/7.0/isom_8c_source.html
            for (a, b) in [
                ("hearing_impaired", "captions"),
                ("visual_impaired", "descriptions"),
            ] {
                if flags.contains_key(a) || flags.contains_key(b) {
                    flags.insert(a.into(), 1);
                    flags.insert(b.into(), 1);
                }
            }
        }
        flags
    }
}

impl TrackInfo {
    pub fn from_streams(streams: &[serde_json::Value]) -> Self {
        let mut result = Self::default();
        for stream in streams {
            match stream["codec_type"].as_str() {
                Some("attachment") => result.attachment_count += 1,
                Some("subtitle") => result.subtitles.push(SubtitleInfo {
                    codec: stream["codec_name"].as_str().unwrap_or("unknown").into(),
                    disposition: stream["disposition"]
                        .as_object()
                        .into_iter()
                        .flatten()
                        .filter_map(|(key, value)| {
                            value.as_u64().filter(|v| *v != 0).map(|_| (key.clone(), 1))
                        })
                        .collect(),
                }),
                _ => {}
            }
        }
        result
    }

    pub fn output_args(&self, container: &str, input: u8) -> Result<Vec<String>, String> {
        let mp4 = container.eq_ignore_ascii_case("mp4");
        if mp4 && self.attachment_count > 0 {
            return Err(
                "MP4 cannot preserve attached files/fonts. Choose MKV to retain them.".into(),
            );
        }
        let mut args = Vec::new();
        if !mp4 && self.attachment_count > 0 {
            args.extend([
                "-map".into(),
                format!("{input}:t?"),
                "-c:t".into(),
                "copy".into(),
            ]);
        }
        for (index, subtitle) in self.subtitles.iter().enumerate() {
            if mp4
                && !matches!(
                    subtitle.codec.as_str(),
                    "subrip"
                        | "srt"
                        | "ass"
                        | "ssa"
                        | "mov_text"
                        | "text"
                        | "webvtt"
                        | "ttml"
                        | "sami"
                        | "realtext"
                        | "subviewer"
                        | "subviewer1"
                        | "microdvd"
                        | "mpl2"
                        | "jacosub"
                        | "pjs"
                        | "vplayer"
                        | "stl"
                        | "aqtitle"
                )
            {
                return Err(format!("Subtitle {} ({}) cannot be converted to MP4 timed text. Choose MKV to preserve bitmap or unsupported subtitles.",index+1,subtitle.codec));
            }
            if !mp4 && subtitle.codec == "mov_text" {
                args.extend([format!("-c:s:{index}"), "ass".into()]);
            }
            let flags = subtitle
                .muxed_disposition(mp4)
                .iter()
                .filter(|(_, value)| **value != 0)
                .map(|(key, _)| key.as_str())
                .collect::<Vec<_>>()
                .join("+");
            args.extend([
                format!("-disposition:s:{index}"),
                if flags.is_empty() { "0".into() } else { flags },
            ]);
        }
        Ok(args)
    }

    pub fn verify(&self, output: &Self, mp4: bool) -> Result<(), String> {
        if self.subtitles.len() != output.subtitles.len()
            || (!mp4 && self.attachment_count != output.attachment_count)
        {
            return Err("Subtitle or attachment streams changed unexpectedly".into());
        }
        for (index, (source, output)) in self.subtitles.iter().zip(&output.subtitles).enumerate() {
            let expected = source.muxed_disposition(mp4);
            if expected != output.disposition {
                return Err(format!("Subtitle dispositions changed unexpectedly for track {}: expected {:?}, found {:?}", index + 1, expected, output.disposition));
            }
        }
        Ok(())
    }
}

/// Append options after the generic defaults but before the output filename.
pub fn apply(mut args: Vec<String>, track_args: &[String]) -> Vec<String> {
    let output = args.pop().expect("FFmpeg output path missing");
    args.extend_from_slice(track_args);
    args.push(output);
    args
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mp4_requires_exact_serialized_flags_without_relaxing_mkv_validation() {
        let source = TrackInfo::from_streams(&[
            serde_json::json!({"codec_type":"subtitle","codec_name":"subrip","disposition":{"default":1}}),
            serde_json::json!({"codec_type":"subtitle","codec_name":"subrip","disposition":{"default":1,"hearing_impaired":1}}),
        ]);
        let mut output = source.clone();
        output.subtitles[1].codec = "mov_text".into();
        output.subtitles[1].disposition.insert("captions".into(), 1);
        assert!(source.verify(&output, true).is_ok());
        assert!(source.verify(&output, false).is_err());
        let args = source.output_args("mp4", 0).unwrap();
        assert!(args
            .windows(2)
            .any(|p| p == ["-disposition:s:1", "captions+default+hearing_impaired"]));
        for flag in ["default", "hearing_impaired", "captions"] {
            let mut missing = output.clone();
            missing.subtitles[1].disposition.remove(flag);
            assert!(source.verify(&missing, true).is_err(), "Lost {flag}");
        }
        output.subtitles[0].disposition.insert("forced".into(), 1);
        assert!(source.verify(&output, true).is_err());
        output.subtitles.pop();
        assert!(source.verify(&output, true).is_err());
    }

    #[test]
    fn mp4_conversion_and_remux_preserve_multiple_subtitle_roles() {
        let root = tempfile::tempdir().unwrap();
        let cue = root.path().join("subtitle ü.srt");
        std::fs::write(&cue, "1\n00:00:00,000 --> 00:00:00,500\nHello\n").unwrap();
        let input = root.path().join("source.mkv");
        let status = std::process::Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "color=size=128x96:rate=24:duration=1",
                "-i",
            ])
            .arg(&cue)
            .args([
                "-map",
                "0:v",
                "-map",
                "1:s",
                "-map",
                "1:s",
                "-map",
                "1:s",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-c:s",
                "srt",
                "-disposition:s:0",
                "default",
                "-disposition:s:1",
                "default+hearing_impaired",
                "-disposition:s:2",
                "0",
            ])
            .arg(&input)
            .status()
            .unwrap();
        assert!(status.success());
        let inspect = |path: &std::path::Path| {
            let result = std::process::Command::new("ffprobe")
                .args(["-v", "error", "-show_streams", "-of", "json"])
                .arg(path)
                .output()
                .unwrap();
            assert!(result.status.success());
            let data: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
            TrackInfo::from_streams(data["streams"].as_array().unwrap())
        };
        let source = inspect(&input);
        assert!(!source.subtitles[1].disposition.contains_key("captions"));
        let mut previous = input;
        for index in 0..2 {
            let output = root.path().join(format!("pass {index}.mp4"));
            let info = inspect(&previous);
            let status = std::process::Command::new("ffmpeg")
                .args(["-v", "error", "-y", "-i"])
                .arg(&previous)
                .args(["-map", "0", "-c", "copy", "-c:s", "mov_text"])
                .args(info.output_args("mp4", 0).unwrap())
                .arg(&output)
                .status()
                .unwrap();
            assert!(status.success());
            let actual = inspect(&output);
            source.verify(&actual, true).unwrap();
            assert!(actual.subtitles.iter().all(|s| s.codec == "mov_text"));
            previous = output;
        }
    }
    #[test]
    fn bitmap_mp4_is_rejected_and_mkv_preserves_fonts_and_dispositions() {
        let info = TrackInfo::from_streams(&[
            serde_json::json!({"codec_type":"subtitle","codec_name":"hdmv_pgs_subtitle","disposition":{"default":0,"forced":1}}),
            serde_json::json!({"codec_type":"attachment"}),
        ]);
        assert!(info.output_args("mp4", 0).unwrap_err().contains("attached"));
        let args = info.output_args("mkv", 1).unwrap();
        assert!(args.windows(2).any(|p| p == ["-map", "1:t?"]));
        assert!(args.windows(2).any(|p| p == ["-disposition:s:0", "forced"]));
        let bitmap = TrackInfo {
            attachment_count: 0,
            ..info
        };
        assert!(bitmap
            .output_args("mp4", 0)
            .unwrap_err()
            .contains("hdmv_pgs_subtitle"));
    }
    #[test]
    fn unset_default_stays_unset_and_mov_text_can_be_muxed_into_mkv() {
        let info = TrackInfo::from_streams(&[
            serde_json::json!({"codec_type":"subtitle","codec_name":"mov_text","disposition":{"default":0}}),
        ]);
        let args = info.output_args("mkv", 0).unwrap();
        assert!(args.windows(2).any(|p| p == ["-disposition:s:0", "0"]));
        assert!(args.windows(2).any(|p| p == ["-c:s:0", "ass"]));
        assert!(info.output_args("mp4", 0).is_ok());
    }
}

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
                .disposition
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
        for (source, output) in self.subtitles.iter().zip(&output.subtitles) {
            if source.disposition != output.disposition {
                return Err("Subtitle dispositions changed unexpectedly".into());
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

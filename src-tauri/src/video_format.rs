//! Source format tracking and conservative preservation policy.
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct VideoFormat {
    pub pixel_format: String,
    pub bit_depth: u8,
    pub primaries: String,
    pub transfer: String,
    pub matrix: String,
    pub range: String,
    pub static_hdr: Vec<Value>,
}

pub fn pixel_depth(format: &str) -> u8 {
    match format {
        "yuv420p" | "yuv422p" | "yuv444p" | "yuvj420p" | "yuvj422p" | "yuvj444p" | "nv12"
        | "nv21" | "rgb24" | "bgr24" | "rgba" | "bgra" | "pal8" | "gray" => 8,
        _ => {
            let format = format.trim_end_matches("le").trim_end_matches("be");
            format
                .rsplit_once('p')
                .and_then(|(_, n)| n.parse().ok())
                .unwrap_or(0)
        }
    }
}

impl VideoFormat {
    /// Reject unsupported preservation paths. Deliberate HDR-to-SDR conversion
    /// is selected by the caller and verified separately.
    pub fn hdr10_fallback_error(&self, dv: Option<u8>, compat: Option<u8>) -> Option<String> {
        dv?;
        if dv != Some(8)
            || compat != Some(1)
            || self.bit_depth != 10
            || self.primaries != "bt2020"
            || self.transfer != "smpte2084"
            || !matches!(self.matrix.as_str(), "bt2020nc" | "bt2020c")
        {
            return Some("No verified HDR10-compatible Dolby Vision base layer. Re-encoding blocked to avoid incorrect colors; keep the original or use stream copy.".into());
        }
        None
    }

    pub fn preservation_error(
        &self,
        codec: &str,
        compatibility: bool,
        hdr: bool,
        dv: Option<u8>,
        compat: Option<u8>,
        tone_mapped: bool,
    ) -> Option<String> {
        if compatibility && !tone_mapped && (self.bit_depth != 8 || hdr || dv.is_some()) {
            return Some("Compatibility mode cannot preserve this source. Disable compatibility mode; its H.264/SDR conversion is blocked.".into());
        }
        if let Some(reason) = self.hdr10_fallback_error(dv, compat) {
            return Some(reason);
        }
        if !tone_mapped && self.bit_depth > 10 && codec != "hevc" {
            return Some("12-bit preservation requires software HEVC. Select HEVC or Auto.".into());
        }
        self.software_format().err()
    }
    pub fn from_stream(stream: &Value) -> Self {
        let text = |name| stream[name].as_str().unwrap_or("").to_string();
        let pixel_format = text("pix_fmt");
        let bit_depth = text("bits_per_raw_sample")
            .parse::<u8>()
            .ok()
            .filter(|v| *v > 0)
            .unwrap_or_else(|| pixel_depth(&pixel_format));
        // A stale raw-sample hint must not override a higher-precision decoded
        // pixel format. Inconsistent metadata is handled conservatively.
        let bit_depth = bit_depth.max(pixel_depth(&pixel_format));
        let mut result = Self {
            pixel_format,
            bit_depth,
            primaries: text("color_primaries"),
            transfer: text("color_transfer"),
            matrix: text("color_space"),
            range: text("color_range"),
            static_hdr: Vec::new(),
        };
        result.absorb_side_data(&stream["side_data_list"]);
        result
    }

    pub fn absorb_side_data(&mut self, data: &Value) {
        if let Some(entries) = data.as_array() {
            for entry in entries {
                let kind = entry["side_data_type"].as_str().unwrap_or("");
                if matches!(
                    kind,
                    "Mastering display metadata" | "Content light level metadata"
                ) && !self.static_hdr.contains(entry)
                {
                    self.static_hdr.push(entry.clone());
                }
            }
        }
    }

    pub fn software_format(&self) -> Result<String, String> {
        if !self.pixel_format.starts_with("yuv420p")
            && !self.pixel_format.starts_with("yuv422p")
            && !self.pixel_format.starts_with("yuv444p")
            && !matches!(
                self.pixel_format.as_str(),
                "nv12" | "p010le" | "p012le" | "yuvj420p"
            )
        {
            return Err(format!("Unsupported source pixel layout ({}); automatic chroma/color conversion is blocked",self.pixel_format));
        }
        if matches!(self.bit_depth, 8 | 10 | 12)
            && (self.pixel_format.starts_with("yuv422p")
                || self.pixel_format.starts_with("yuv444p"))
        {
            return Ok(self.pixel_format.clone());
        }
        match self.bit_depth {
            8 => Ok("yuv420p".into()),
            10 => Ok("yuv420p10le".into()),
            12 => Ok("yuv420p12le".into()),
            n => Err(format!("Unsupported or unknown source bit depth ({n}, {}). Conversion rejected to avoid reducing precision.", self.pixel_format)),
        }
    }

    pub fn gpu_decode_eligible(
        &self,
        codec: &str,
        encoder: &str,
        enabled: bool,
        filters: bool,
        _special_hdr: bool,
    ) -> bool {
        cfg!(target_os = "linux")
            && enabled
            && encoder.ends_with("_vaapi")
            && matches!(codec, "h264" | "hevc" | "av1")
            && matches!(
                self.pixel_format.as_str(),
                "yuv420p" | "nv12" | "yuv420p10le" | "p010le"
            )
            && matches!(self.bit_depth, 8 | 10)
            && !filters
    }

    /// Encode static HDR SEI explicitly when falling back to x265. VAAPI's
    /// default HDR SEI handling uses frame side data; final output is verified.
    pub fn x265_hdr_params(&self) -> String {
        let mut params = Vec::new();
        for (name, value) in [
            ("colorprim", &self.primaries),
            ("transfer", &self.transfer),
            ("colormatrix", &self.matrix),
        ] {
            if known(value) {
                params.push(format!("{name}={value}"));
            }
        }
        match self.range.as_str() {
            "pc" => params.push("range=full".into()),
            "tv" => params.push("range=limited".into()),
            _ => {}
        }
        for entry in &self.static_hdr {
            let number = |name: &str, scale: f64| -> Option<u64> {
                numeric(&entry[name]).map(|v| (v * scale).round() as u64)
            };
            match entry["side_data_type"].as_str().unwrap_or("") {
                "Mastering display metadata" => {
                    let values: Option<Vec<_>> = [
                        "green_x",
                        "green_y",
                        "blue_x",
                        "blue_y",
                        "red_x",
                        "red_y",
                        "white_point_x",
                        "white_point_y",
                    ]
                    .iter()
                    .map(|k| number(k, 50000.0))
                    .collect();
                    if let (Some(v), Some(max), Some(min)) = (
                        values,
                        number("max_luminance", 10000.0),
                        number("min_luminance", 10000.0),
                    ) {
                        params.push(format!(
                            "master-display=G({},{})B({},{})R({},{})WP({},{})L({max},{min})",
                            v[0], v[1], v[2], v[3], v[4], v[5], v[6], v[7]
                        ));
                    }
                }
                "Content light level metadata" => {
                    if let (Some(max), Some(avg)) =
                        (number("max_content", 1.0), number("max_average", 1.0))
                    {
                        params.push(format!("max-cll={max},{avg}"));
                    }
                }
                _ => {}
            }
        }
        params.join(":")
    }

    /// Some decoders expose unknown frame color properties even when the
    /// container has valid tags. VAAPI uses frame properties for the SPS VUI.
    pub fn frame_color_filter(&self) -> Option<String> {
        let mut values = Vec::new();
        for (key, value) in [
            ("color_primaries", &self.primaries),
            ("color_trc", &self.transfer),
            ("colorspace", &self.matrix),
        ] {
            if known(value) {
                values.push(format!("{key}={value}"));
            }
        }
        match self.range.as_str() {
            "tv" => values.push("range=limited".into()),
            "pc" => values.push("range=full".into()),
            _ => {}
        }
        if values.is_empty() {
            None
        } else {
            Some(format!("setparams={}", values.join(":")))
        }
    }

    pub fn verify(&self, output: &Self, tone_mapped: bool) -> Result<(), String> {
        let expected = if tone_mapped { 8 } else { self.bit_depth };
        if expected == 0 || output.bit_depth != expected {
            return Err(format!(
                "Bit-depth preservation failed: expected {expected}-bit, got {}-bit",
                output.bit_depth
            ));
        }
        if tone_mapped {
            for (name, value) in [
                ("primaries", &output.primaries),
                ("transfer", &output.transfer),
                ("matrix", &output.matrix),
            ] {
                if value != "bt709" {
                    return Err(format!("SDR {name} was not BT.709: {value}"));
                }
            }
            if !output.static_hdr.is_empty() {
                return Err("SDR output still carries static HDR metadata".into());
            }
        }
        if !tone_mapped {
            let chroma = |pixel: &str| {
                if pixel.contains("422") {
                    422
                } else if pixel.contains("444") {
                    444
                } else {
                    420
                }
            };
            if chroma(&self.pixel_format) != chroma(&output.pixel_format) {
                return Err("Output chroma subsampling changed without consent".into());
            }
            for (name, source, encoded) in [
                ("primaries", &self.primaries, &output.primaries),
                ("transfer", &self.transfer, &output.transfer),
                ("matrix", &self.matrix, &output.matrix),
                ("range", &self.range, &output.range),
            ] {
                if known(source) && source != encoded {
                    return Err(format!("Color {name} changed: {source} -> {encoded}"));
                }
            }
            for metadata in &self.static_hdr {
                let found = output.static_hdr.iter().any(|out| {
                    metadata.as_object().is_some_and(|fields| {
                        fields.iter().all(|(k, v)| {
                            v == &out[k]
                                || match (numeric(v), numeric(&out[k])) {
                                    (Some(a), Some(b)) => (a - b).abs() <= 0.00001,
                                    _ => false,
                                }
                        })
                    })
                });
                if !found {
                    return Err("Required static HDR metadata was lost or changed".into());
                }
            }
        }
        Ok(())
    }
}

pub fn known(value: &str) -> bool {
    !matches!(value, "" | "unknown" | "unspecified" | "reserved")
}

fn numeric(value: &Value) -> Option<f64> {
    value.as_f64().or_else(|| {
        let s = value.as_str()?;
        if let Some((n, d)) = s.split_once('/') {
            let d: f64 = d.parse().ok()?;
            if d == 0.0 {
                return None;
            }
            Some(n.parse::<f64>().ok()? / d)
        } else {
            s.parse().ok()
        }
    })
}

/// Retry only failed hardware-decoding attempts, never cancellation. The
/// caller removes partial output and logs the transition before retrying.
pub async fn decode_retry<T, F, Fut, P, C>(enabled: bool, mut run: F, failed: P, on_retry: C) -> T
where
    F: FnMut(bool) -> Fut,
    Fut: std::future::Future<Output = T>,
    P: Fn(&T) -> bool,
    C: FnOnce(),
{
    let result = run(enabled).await;
    if enabled && failed(&result) {
        on_retry();
        run(false).await
    } else {
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn stale_raw_sample_hint_cannot_reduce_decoded_precision() {
        let source =
            VideoFormat::from_stream(&json!({"pix_fmt":"yuv420p10le","bits_per_raw_sample":"8"}));
        assert_eq!(source.bit_depth, 10);
        let twelve =
            VideoFormat::from_stream(&json!({"pix_fmt":"yuv420p12le","bits_per_raw_sample":"10"}));
        assert_eq!(twelve.bit_depth, 12);
    }
    #[test]
    fn hdr10_fallback_requires_verified_profile_8_1_pq_base_layer() {
        let source = VideoFormat::from_stream(&json!({"pix_fmt":"yuv420p10le",
            "color_transfer":"smpte2084","color_primaries":"bt2020","color_space":"bt2020nc"}));
        assert!(source
            .preservation_error("hevc", false, true, Some(8), Some(1), false)
            .is_none());
        for (profile, compat) in [
            (Some(5), Some(0)),
            (Some(7), Some(6)),
            (Some(8), Some(4)),
            (Some(8), None),
        ] {
            assert!(source.hdr10_fallback_error(profile, compat).is_some());
        }
        let mut invalid = source.clone();
        invalid.bit_depth = 8;
        assert!(invalid.hdr10_fallback_error(Some(8), Some(1)).is_some());
        invalid = source.clone();
        invalid.transfer = "arib-std-b67".into();
        assert!(invalid.hdr10_fallback_error(Some(8), Some(1)).is_some());
        assert!(source
            .preservation_error("h264", true, true, None, None, false)
            .is_some());
        assert!(source
            .verify(
                &VideoFormat::from_stream(&json!({"pix_fmt":"yuv420p"})),
                false
            )
            .is_err());
    }
    #[test]
    fn twelve_bit_hdr_can_explicitly_tonemap_to_compatibility_sdr() {
        let source = VideoFormat::from_stream(&json!({
            "pix_fmt":"yuv420p12le", "color_primaries":"bt2020",
            "color_transfer":"smpte2084", "color_space":"bt2020nc"
        }));
        assert!(source
            .preservation_error("h264", true, true, None, None, true)
            .is_none());
        assert!(source
            .preservation_error("h264", true, true, None, None, false)
            .is_some());
        // SDR conversion does not waive the Dolby Vision base-layer safety check.
        assert!(source
            .preservation_error("h264", true, true, Some(5), Some(0), true)
            .is_some());
    }
    #[test]
    fn depths_sdr_hdr_and_unsupported_formats() {
        for (pix, depth) in [
            ("yuv420p", 8),
            ("yuv420p10le", 10),
            ("p010le", 10),
            ("yuv420p12le", 12),
            ("mystery", 0),
        ] {
            let f = VideoFormat::from_stream(&json!({"pix_fmt":pix}));
            assert_eq!(f.bit_depth, depth);
            assert_eq!(
                f.gpu_decode_eligible("hevc", "hevc_vaapi", true, false, false),
                cfg!(target_os = "linux") && matches!(depth, 8 | 10)
            );
            if depth == 12 {
                assert_eq!(f.software_format().unwrap(), "yuv420p12le");
            }
            if depth == 0 {
                assert!(f.software_format().is_err());
            }
        }
        let f = VideoFormat::from_stream(&json!({"pix_fmt":"yuv422p10le"}));
        assert!(!f.gpu_decode_eligible("hevc", "hevc_vaapi", true, false, false));
    }
    #[test]
    fn preserve_color_depth_and_static_metadata_or_reject() {
        for trc in ["bt709", "smpte2084", "arib-std-b67"] {
            let f = VideoFormat::from_stream(
                &json!({"pix_fmt":"yuv420p10le","color_primaries":"bt2020","color_transfer":trc,"color_space":"bt2020nc","color_range":"pc","side_data_list":[{"side_data_type":"Content light level metadata","max_content":1000,"max_average":400}]}),
            );
            assert!(f.verify(&f, false).is_ok());
            let mut bad = f.clone();
            bad.bit_depth = 8;
            assert!(f.verify(&bad, false).is_err());
            bad = f.clone();
            bad.range = "tv".into();
            assert!(f.verify(&bad, false).is_err());
            bad = f.clone();
            bad.static_hdr.clear();
            assert!(f.verify(&bad, false).is_err());
            assert!(f.x265_hdr_params().contains("max-cll=1000,400"));
            // Compressed metadata extraction is independent of pixel decoding.
            assert_eq!(
                f.gpu_decode_eligible("hevc", "hevc_vaapi", true, false, true),
                cfg!(target_os = "linux")
            );
            assert!(!f.gpu_decode_eligible("hevc", "hevc_vaapi", true, true, false)); // CPU filters
            assert!(!f.gpu_decode_eligible("hevc", "hevc_vaapi", false, false, false));
        }
    }
    #[test]
    fn deliberate_tonemap_requires_eight_bit_sdr_without_hdr_metadata() {
        let source = VideoFormat::from_stream(&json!({
            "pix_fmt":"yuv420p10le", "color_primaries":"bt2020",
            "color_transfer":"smpte2084", "color_space":"bt2020nc"
        }));
        let sdr = VideoFormat::from_stream(&json!({
            "pix_fmt":"yuv420p", "color_primaries":"bt709",
            "color_transfer":"bt709", "color_space":"bt709"
        }));
        assert!(source.verify(&sdr, true).is_ok());
        assert!(source.verify(&sdr, false).is_err());
        let mut bad = sdr.clone();
        bad.transfer = "smpte2084".into();
        assert!(source.verify(&bad, true).is_err());
        bad = sdr.clone();
        bad.bit_depth = 10;
        assert!(source.verify(&bad, true).is_err());
        bad = sdr;
        bad.static_hdr
            .push(json!({"side_data_type":"Content light level metadata"}));
        assert!(source.verify(&bad, true).is_err());
    }
    #[tokio::test]
    async fn failure_retries_once_and_cancellation_never_retries() {
        let mut attempts = Vec::new();
        let dir = tempfile::tempdir().unwrap();
        let partial = dir.path().join("partial.mkv");
        let result = decode_retry(
            true,
            |hw| {
                attempts.push(hw);
                if hw {
                    std::fs::write(&partial, b"partial").unwrap();
                } else {
                    assert!(
                        !partial.exists(),
                        "partial output must be removed before retry"
                    );
                }
                std::future::ready(if hw { 1 } else { 0 })
            },
            |r| *r != 0,
            || {
                std::fs::remove_file(&partial).unwrap();
            },
        )
        .await;
        assert_eq!(attempts, [true, false]);
        assert_eq!(result, 0);
        let mut count = 0;
        decode_retry(
            true,
            |_| {
                count += 1;
                std::future::ready(-1)
            },
            |r| *r > 0,
            || panic!("cancelled"),
        )
        .await;
        assert_eq!(count, 1);
    }
}

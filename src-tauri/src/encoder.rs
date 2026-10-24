use crate::events::{BatchControl, EventSink};
use crate::ffmpeg as ffbin;
use crate::queue::{QueueItem, QueueItemStatus};
use crate::staging::{StagingContext, WaveItem};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::Arc;
use tokio::process::Command;

#[cfg(target_os = "linux")]
#[path = "encoder_devices.rs"]
mod devices;

// ── Rate-control parameters (options struct) ───────────────────

/// Rate control parameters for the encoding decision.
pub struct RateControlParams<'a> {
    pub mode: &'a str,
    pub qp_i: u32,
    pub qp_p: u32,
    pub crf_val: u32,
}

// ── Encoder info & abstraction layer ────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EncoderInfo {
    pub name: String,
    pub codec_family: String, // "hevc", "h264", or "av1"
    pub is_hardware: bool,
    /// DRM device that passed this encoder's probe; absent for other backends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub render_device: Option<String>,
}

/// Encoder priority tables per platform (§8.2).
#[cfg(target_os = "windows")]
const HEVC_PRIORITY: &[&str] = &["hevc_amf", "hevc_nvenc", "hevc_qsv", "libx265"];
#[cfg(target_os = "windows")]
const H264_PRIORITY: &[&str] = &["h264_amf", "h264_nvenc", "h264_qsv", "libx264"];
#[cfg(target_os = "windows")]
const AV1_PRIORITY: &[&str] = &["av1_amf", "av1_nvenc", "av1_qsv", "libsvtav1"];

#[cfg(target_os = "macos")]
const HEVC_PRIORITY: &[&str] = &["hevc_videotoolbox", "libx265"];
#[cfg(target_os = "macos")]
const H264_PRIORITY: &[&str] = &["h264_videotoolbox", "libx264"];
#[cfg(target_os = "macos")]
const AV1_PRIORITY: &[&str] = &["av1_videotoolbox", "libsvtav1"];

#[cfg(target_os = "linux")]
const HEVC_PRIORITY: &[&str] = &["hevc_vaapi", "hevc_nvenc", "hevc_qsv", "libx265"];
#[cfg(target_os = "linux")]
const H264_PRIORITY: &[&str] = &["h264_vaapi", "h264_nvenc", "h264_qsv", "libx264"];
#[cfg(target_os = "linux")]
const AV1_PRIORITY: &[&str] = &["av1_vaapi", "av1_nvenc", "av1_qsv", "libsvtav1"];

// Fallback for other platforms (compilation only)
#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
const HEVC_PRIORITY: &[&str] = &["libx265"];
#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
const H264_PRIORITY: &[&str] = &["libx264"];
#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
const AV1_PRIORITY: &[&str] = &["libsvtav1"];

const AUDIO_ENCODERS: &[&str] = &["ac3", "eac3", "aac"];

/// Fixed tonemap filter chain for HDR→SDR conversion (#1).
/// Defined as a static str to avoid per-file heap allocation.
const TONEMAP_HABLE: &str = concat!(
    "zscale=t=linear,tonemap=hable:desat=0,zscale=t=bt709:p=bt709:m=bt709:r=tv,format=yuv420p",
    ",sidedata=mode=delete:type=MASTERING_DISPLAY_METADATA",
    ",sidedata=mode=delete:type=CONTENT_LIGHT_LEVEL",
    ",sidedata=mode=delete:type=DYNAMIC_HDR_PLUS"
);

/// Files within this fraction above the threshold that are already in the
/// target codec are stream-copied rather than re-encoded. Avoids wasting
/// time on marginal bitrate gains (e.g. 4.5 Mbps vs 4.0 Mbps target).
const COPY_HYSTERESIS: f64 = 1.15;

/// Minimum file duration (seconds) required for CRF viability probing.
/// Files shorter than this skip the probe — not worth the overhead.
const MIN_PROBE_DURATION_SECS: f64 = 120.0;

/// Fractional seek points for CRF viability probe samples (25%, 50%, 75%).
const PROBE_SEEK_POINTS: [f64; 3] = [0.25, 0.50, 0.75];

/// Duration of each CRF probe sample in seconds.
const PROBE_SAMPLE_DURATION_SECS: f64 = 10.0;

/// Per-encoder flag mapping for VBR mode.
/// AV1 hardware encoders use the same flags as their H.264/HEVC counterparts.
pub fn vbr_flags(encoder: &str, target: &str, peak: &str) -> Vec<String> {
    match encoder {
        "hevc_amf" | "h264_amf" | "av1_amf" => vec![
            "-quality".into(),
            "quality".into(),
            "-rc".into(),
            "vbr_peak".into(),
            "-b:v".into(),
            target.into(),
            "-maxrate".into(),
            peak.into(),
        ],
        "hevc_nvenc" | "h264_nvenc" | "av1_nvenc" => vec![
            "-preset".into(),
            "p7".into(),
            "-rc".into(),
            "vbr".into(),
            "-b:v".into(),
            target.into(),
            "-maxrate".into(),
            peak.into(),
        ],
        "hevc_qsv" | "h264_qsv" | "av1_qsv" => vec![
            "-preset".into(),
            "veryslow".into(),
            "-b:v".into(),
            target.into(),
            "-maxrate".into(),
            peak.into(),
        ],
        "hevc_videotoolbox" | "h264_videotoolbox" | "av1_videotoolbox" => {
            vec!["-b:v".into(), target.into(), "-maxrate".into(), peak.into()]
        }
        "hevc_vaapi" | "h264_vaapi" | "av1_vaapi" => vec![
            "-rc_mode".into(),
            "VBR".into(),
            "-b:v".into(),
            target.into(),
            "-maxrate".into(),
            peak.into(),
        ],
        "libx265" | "libx264" => vec![
            "-preset".into(),
            "slow".into(),
            "-b:v".into(),
            target.into(),
            "-maxrate".into(),
            peak.into(),
        ],
        // SVT-AV1 rejects -maxrate in VBR mode (rate_control_mode=1);
        // it only accepts max_bit_rate with CRF (capped-CRF).
        "libsvtav1" => vec!["-preset".into(), "6".into(), "-b:v".into(), target.into()],
        _ => vec!["-b:v".into(), target.into(), "-maxrate".into(), peak.into()],
    }
}

/// Per-encoder flag mapping for CQP mode.
/// AV1 hardware encoders use the same flags as their H.264/HEVC counterparts.
pub fn cqp_flags(encoder: &str, qi: &str, qp: &str) -> Vec<String> {
    match encoder {
        "hevc_amf" | "h264_amf" | "av1_amf" => vec![
            "-quality".into(),
            "quality".into(),
            "-rc".into(),
            "cqp".into(),
            "-qp_i".into(),
            qi.into(),
            "-qp_p".into(),
            qp.into(),
        ],
        "hevc_nvenc" | "h264_nvenc" | "av1_nvenc" => vec![
            "-preset".into(),
            "p7".into(),
            "-rc".into(),
            "constqp".into(),
            "-qp".into(),
            qi.into(),
        ],
        "hevc_qsv" | "h264_qsv" | "av1_qsv" => vec![
            "-preset".into(),
            "veryslow".into(),
            "-global_quality".into(),
            qi.into(),
        ],
        "hevc_videotoolbox" | "h264_videotoolbox" | "av1_videotoolbox" => {
            vec!["-q:v".into(), qi.into()]
        }
        "hevc_vaapi" | "h264_vaapi" | "av1_vaapi" => {
            vec!["-rc_mode".into(), "CQP".into(), "-qp".into(), qi.into()]
        }
        "libx265" | "libx264" => vec!["-preset".into(), "slow".into(), "-qp".into(), qi.into()],
        "libsvtav1" => vec![
            "-preset".into(),
            "6".into(),
            "-rc".into(),
            "0".into(),
            "-qp".into(),
            qi.into(),
        ],
        _ => vec!["-qp".into(), qi.into()],
    }
}

/// Per-encoder flag mapping for CRF mode - only valid for software encoders.
/// Hardware encoders do not support CRF; fall back to CQP for them.
/// Accepts pre-formatted strings to avoid re-allocating per file.
pub fn crf_flags(encoder: &str, crf_str: &str, qi: &str, qp: &str) -> Vec<String> {
    match encoder {
        "libx265" | "libx264" => vec![
            "-preset".into(),
            "slow".into(),
            "-crf".into(),
            crf_str.into(),
        ],
        "libsvtav1" => vec!["-preset".into(), "6".into(), "-crf".into(), crf_str.into()],
        // HW encoders don't support CRF - fall back to CQP
        _ => cqp_flags(encoder, qi, qp),
    }
}

/// Software fallback encoder for a given codec family.
pub fn software_fallback(codec_family: &str) -> &'static str {
    match codec_family {
        "h264" => "libx264",
        "av1" => "libsvtav1",
        _ => "libx265",
    }
}

/// Human-readable codec family name for log messages.
pub fn display_codec_family(family: &str) -> &'static str {
    match family {
        "h264" => "H.264",
        "av1" => "AV1",
        _ => "HEVC",
    }
}

/// Whether the encoder name refers to a VAAPI hardware encoder.
pub fn is_vaapi_encoder(name: &str) -> bool {
    name.ends_with("_vaapi")
}

/// Whether the encoder name refers to a QSV hardware encoder.
pub fn is_qsv_encoder(name: &str) -> bool {
    name.ends_with("_qsv")
}

// ── Encoding strategy decision ─────────────────────────────────

/// The encoding decision for a single file, determined purely from its
/// probed metadata and the batch settings. Used by both the dry-run plan
/// display and the encoding loop to ensure consistent decisions.
#[derive(Debug, Clone)]
pub enum EncodeDecision {
    /// Stream-copy: already the target codec and at/below the bitrate threshold.
    Copy,
    /// VBR transcode: source exceeds the bitrate threshold.
    Vbr { target_bps: u64, peak_bps: u64 },
    /// CQP transcode: at/below threshold, cross-codec or zero-bitrate probe.
    Cqp { qi: u32, qp: u32 },
    /// CRF transcode: at/below threshold, CRF rate-control selected.
    Crf {
        crf: u32,
        qi_fallback: u32,
        qp_fallback: u32,
    },
}

/// Determine the encoding strategy for a single file.
///
/// This is a pure function with no I/O - it uses only the probed metadata
/// and batch settings to produce a decision. Both the GUI's encoding loop
/// and the CLI's dry-run call this to avoid duplicating the decision logic.
///
/// `video_codec` is the source file's codec name (e.g. "hevc", "h264").
/// `target_codec` is the target codec name (e.g. "hevc", "h264") - note
/// this is the lowercase ffmpeg name, not the display name ("HEVC"/"H.264").
pub fn decide_encode_strategy(
    bitrate_mbps: f64,
    threshold: f64,
    video_codec: &str,
    target_codec: &str,
    rc: &RateControlParams,
    peak_multiplier: f64,
) -> EncodeDecision {
    // Codecs that cannot be stream-copied into MKV/MP4 - always transcode.
    let is_copyable = !matches!(video_codec, "gif" | "apng" | "mjpeg" | "webp");
    let is_already_target = video_codec == target_codec;

    if bitrate_mbps <= threshold || bitrate_mbps <= 0.0 {
        // Below threshold with a valid bitrate: copy as-is. The file is
        // already small enough - transcoding to a different codec doesn't
        // make it smaller and risks making it bigger.
        if bitrate_mbps > 0.0 && is_copyable {
            EncodeDecision::Copy
        } else if rc.mode == "CRF" || rc.mode == "crf" {
            EncodeDecision::Crf {
                crf: rc.crf_val,
                qi_fallback: rc.qp_i,
                qp_fallback: rc.qp_p,
            }
        } else {
            EncodeDecision::Cqp {
                qi: rc.qp_i,
                qp: rc.qp_p,
            }
        }
    } else if is_already_target && is_copyable && bitrate_mbps <= threshold * COPY_HYSTERESIS {
        EncodeDecision::Copy
    } else {
        let target_bps = (threshold * 1_000_000.0) as u64;
        let mult = if peak_multiplier > 1.0 {
            peak_multiplier
        } else {
            1.5
        };
        let peak_bps = (target_bps as f64 * mult) as u64;
        EncodeDecision::Vbr {
            target_bps,
            peak_bps,
        }
    }
}

/// An HDR-to-SDR filter cannot run with stream copy. Keep the plan and the
/// encode loop on the same quality-based fallback when a copy was selected.
pub fn require_tonemap_encode(
    decision: EncodeDecision,
    tone_mapped: bool,
    rc: &RateControlParams<'_>,
) -> EncodeDecision {
    if !tone_mapped || !matches!(decision, EncodeDecision::Copy) {
        return decision;
    }
    if rc.mode.eq_ignore_ascii_case("CRF") {
        EncodeDecision::Crf {
            crf: rc.crf_val,
            qi_fallback: rc.qp_i,
            qp_fallback: rc.qp_p,
        }
    } else {
        EncodeDecision::Cqp {
            qi: rc.qp_i,
            qp: rc.qp_p,
        }
    }
}

// ── Encoder detection (§8) ──────────────────────────────────────

/// Detect available encoders by running `ffmpeg -encoders`, then verifying
/// each hardware encoder with a single-frame test encode.
/// Software encoders (libx265/libx264) skip the test - they always work.
pub async fn detect_encoders(sink: &dyn EventSink) -> (Vec<EncoderInfo>, Vec<String>) {
    sink.log("[detect] Starting encoder detection...");

    let output = match ffbin::ffmpeg_command()
        .args(["-encoders"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(child) => match child.wait_with_output().await {
            Ok(o) => o,
            Err(e) => {
                sink.log(&format!("[detect] ffmpeg wait error: {e}"));
                return fallback_encoders();
            }
        },
        Err(e) => {
            sink.log(&format!("[detect] ffmpeg not found: {e}"));
            return fallback_encoders();
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut video_encoders: Vec<EncoderInfo> = Vec::new();
    let mut audio_encoders: Vec<String> = Vec::new();

    // Parse the full encoder list once into a HashSet for O(1) lookups
    let available: HashSet<&str> = stdout
        .lines()
        .flat_map(|line| line.split_whitespace())
        .collect();

    #[cfg(target_os = "linux")]
    let render_nodes = find_dri_render_nodes_in(std::path::Path::new("/dev/dri"));
    #[cfg(not(target_os = "linux"))]
    let render_nodes: Vec<String> = Vec::new();

    // Classify each video encoder: software encoders are added immediately,
    // hardware candidates are collected for sequential verification.
    struct HwCandidate {
        name: &'static str,
        family: &'static str,
    }
    let mut hw_candidates: Vec<HwCandidate> = Vec::new();

    for &enc_name in HEVC_PRIORITY
        .iter()
        .chain(H264_PRIORITY.iter())
        .chain(AV1_PRIORITY.iter())
    {
        if !available.contains(enc_name) {
            sink.log(&format!("[detect] {enc_name} not listed in ffmpeg"));
            continue;
        }

        let family = if enc_name.starts_with("hevc") || enc_name == "libx265" {
            "hevc"
        } else if enc_name.starts_with("av1") || enc_name == "libsvtav1" {
            "av1"
        } else {
            "h264"
        };
        let is_hw = !enc_name.starts_with("lib");

        // Software encoders always work - skip test encode
        if !is_hw {
            sink.log(&format!("[detect] {enc_name} (SW) - available"));
            video_encoders.push(EncoderInfo {
                name: enc_name.to_string(),
                codec_family: family.to_string(),
                is_hardware: false,
                render_device: None,
            });
            continue;
        }

        sink.log(&format!("[detect] {enc_name} - queued for testing..."));
        hw_candidates.push(HwCandidate {
            name: enc_name,
            family,
        });
    }

    // Test hardware encoder candidates sequentially. Concurrent test
    // encodes can fail on GPUs that don't support multiple simultaneous
    // encoding sessions (e.g. AMD AMF), causing all candidates to be
    // rejected even though they work individually.
    for candidate in &hw_candidates {
        #[cfg(target_os = "linux")]
        let render_nodes = devices::preferred_render_nodes(
            candidate.name,
            &render_nodes,
            std::path::Path::new("/sys/class/drm"),
        );
        #[cfg(target_os = "linux")]
        if is_vaapi_encoder(candidate.name) || is_qsv_encoder(candidate.name) {
            sink.log(&format!(
                "[detect] {} device preference: {}",
                candidate.name,
                render_nodes.join(", ")
            ));
        }
        let devices = hardware_probe_devices(candidate.name, &render_nodes);
        let verified = first_working_device(devices, |device| async move {
            let result = test_encode(candidate.name, device.as_deref()).await;
            let label = device.as_deref().unwrap_or("default device");
            match result {
                Ok(()) => {
                    sink.log(&format!("[detect] {} on {label} - works", candidate.name));
                    true
                }
                Err(error) => {
                    sink.log(&format!(
                        "[detect] {} on {label} - failed: {error}",
                        candidate.name
                    ));
                    false
                }
            }
        })
        .await;
        if let Some(render_device) = verified {
            video_encoders.push(EncoderInfo {
                name: candidate.name.to_string(),
                codec_family: candidate.family.to_string(),
                is_hardware: true,
                render_device,
            });
        } else {
            sink.log(&format!(
                "[detect] {} (HW) - no working device",
                candidate.name
            ));
        }
    }

    // Check audio encoders
    for &aenc in AUDIO_ENCODERS {
        if available.contains(aenc) {
            audio_encoders.push(aenc.to_string());
        }
    }

    // Ensure software fallbacks are always present
    ensure_fallback(&mut video_encoders, "libx265", "hevc");
    ensure_fallback(&mut video_encoders, "libx264", "h264");
    ensure_fallback(&mut video_encoders, "libsvtav1", "av1");
    if !audio_encoders.iter().any(|e| e == "ac3") {
        audio_encoders.push("ac3".to_string());
    }

    let total = video_encoders.len() + audio_encoders.len();
    sink.log(&format!(
        "[detect] Detection complete: {} encoders ({} video, {} audio)",
        total,
        video_encoders.len(),
        audio_encoders.len()
    ));

    (video_encoders, audio_encoders)
}

/// Use HD rather than tiny frames: modern AMD hardware can require widths
/// of at least 384 pixels. This is a capability probe, not a guarantee that
/// every input resolution, profile, or pixel format is supported.
const DETECTION_SOURCE: &str = "color=black:s=1920x1080:d=0.04:r=25";

fn hardware_probe_devices(encoder: &str, render_nodes: &[String]) -> Vec<Option<String>> {
    if cfg!(target_os = "linux") && (is_vaapi_encoder(encoder) || is_qsv_encoder(encoder)) {
        render_nodes.iter().cloned().map(Some).collect()
    } else {
        vec![None]
    }
}

/// Stop at the first successful device, preserving its identity. Failures on
/// one device (including permissions or unsupported codecs) do not hide others.
async fn first_working_device<F, Fut>(
    devices: Vec<Option<String>>,
    mut probe: F,
) -> Option<Option<String>>
where
    F: FnMut(Option<String>) -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for device in devices {
        if probe(device.clone()).await {
            return Some(device);
        }
    }
    None
}

/// Derive QSV from an explicitly selected VAAPI device. Initializing QSV
/// before selecting its child can create a session on a different GPU.
#[cfg(target_os = "linux")]
fn qsv_device_args(render_device: &str) -> Vec<String> {
    vec![
        "-init_hw_device".into(),
        format!("vaapi=va:{render_device}"),
        "-init_hw_device".into(),
        "qsv=qs@va".into(),
        "-filter_hw_device".into(),
        "qs".into(),
    ]
}

fn test_encode_args(encoder: &str, render_device: Option<&str>, output: &str) -> Vec<String> {
    let mut args = vec!["-y".into(), "-loglevel".into(), "error".into()];
    #[cfg(target_os = "linux")]
    if let Some(device) = render_device {
        if is_vaapi_encoder(encoder) {
            args.extend(["-vaapi_device".into(), device.into()]);
        } else if is_qsv_encoder(encoder) {
            args.extend(qsv_device_args(device));
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = render_device;
    args.extend([
        "-f".into(),
        "lavfi".into(),
        "-i".into(),
        DETECTION_SOURCE.into(),
        "-frames:v".into(),
        "1".into(),
    ]);
    #[cfg(target_os = "linux")]
    if is_vaapi_encoder(encoder) {
        args.extend(["-vf".into(), "format=nv12,hwupload".into()]);
    } else if is_qsv_encoder(encoder) {
        args.extend([
            "-vf".into(),
            "format=nv12,hwupload=extra_hw_frames=64".into(),
        ]);
    }
    if !is_vaapi_encoder(encoder) && !is_qsv_encoder(encoder) {
        args.extend(["-pix_fmt".into(), "nv12".into()]);
    }
    args.extend(["-c:v".into(), encoder.into(), output.into()]);
    args
}

/// Test with a real output file: some hardware encoders reject the null muxer.
async fn test_encode(encoder: &str, render_device: Option<&str>) -> Result<(), String> {
    let tmp_file = tempfile::Builder::new()
        .prefix("histv_test_")
        .suffix(".mp4")
        .tempfile()
        .map_err(|e| e.to_string())?;
    let args = test_encode_args(encoder, render_device, &tmp_file.path().to_string_lossy());
    let child = ffbin::ffmpeg_command()
        .args(&args)
        .kill_on_drop(true)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let output = tokio::time::timeout(std::time::Duration::from_secs(15), child.wait_with_output())
        .await
        .map_err(|_| "probe timed out after 15 seconds".to_string())?
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(stderr.lines().take(8).collect::<Vec<_>>().join(" | "));
    }
    if tmp_file
        .as_file()
        .metadata()
        .map_err(|e| e.to_string())?
        .len()
        == 0
    {
        return Err("probe produced empty output".into());
    }
    Ok(())
}

/// Enumerate exact DRM render-node names in deterministic numeric order.
/// Let FFmpeg open each candidate so permissions/driver failures are reported
/// by the same operation that verifies codec support.
#[cfg(target_os = "linux")]
fn find_dri_render_nodes_in(dir: &std::path::Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut nodes = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(suffix) = name.to_str().and_then(|n| n.strip_prefix("renderD")) else {
            continue;
        };
        if suffix.is_empty() || !suffix.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(true) {
            continue;
        }
        if let Ok(index) = suffix.parse::<u32>() {
            if let Some(path) = entry.path().to_str() {
                nodes.push((index, path.to_string()));
            }
        }
    }
    nodes.sort();
    nodes.into_iter().map(|(_, path)| path).collect()
}

fn encoder_render_device<'a>(encoders: &'a [EncoderInfo], name: &str) -> Option<&'a str> {
    encoders
        .iter()
        .find(|e| e.name == name)
        .and_then(|e| e.render_device.as_deref())
}

fn ensure_fallback(encoders: &mut Vec<EncoderInfo>, name: &str, family: &str) {
    if !encoders.iter().any(|e| e.name == name) {
        encoders.push(EncoderInfo {
            name: name.to_string(),
            codec_family: family.to_string(),
            is_hardware: false,
            render_device: None,
        });
    }
}

fn fallback_encoders() -> (Vec<EncoderInfo>, Vec<String>) {
    (
        vec![
            EncoderInfo {
                name: "libx265".into(),
                codec_family: "hevc".into(),
                is_hardware: false,
                render_device: None,
            },
            EncoderInfo {
                name: "libx264".into(),
                codec_family: "h264".into(),
                is_hardware: false,
                render_device: None,
            },
            EncoderInfo {
                name: "libsvtav1".into(),
                codec_family: "av1".into(),
                is_hardware: false,
                render_device: None,
            },
        ],
        vec!["ac3".into()],
    )
}

// ── Per-file codec/container/audio resolution (§2.4.0) ──────────

/// Audio handling strategy for a file.
#[derive(Debug, Clone)]
pub enum AudioStrategy {
    /// Explicit passthrough: never apply the automatic bitrate cap.
    CopyAll,
    /// Default: copy if below cap; re-encode to same codec at cap;
    /// codecs with no ffmpeg encoder fall back to EAC3.
    CopyCapped { cap_kbps: u32 },
    /// Compatibility: copy if below cap and already AAC;
    /// otherwise re-encode to AAC at cap.
    CompatCapped { cap_kbps: u32 },
}

/// Resolved per-file encoding settings.
#[derive(Debug, Clone)]
pub struct ResolvedFileSettings {
    pub codec_family: String,
    pub encoder_name: String,
    pub container_ext: String,
    pub audio_strategy: AudioStrategy,
}

/// Resolve the output container extension for a single file.
///
/// Auto derives from the source extension; explicit MKV/MP4 settings win.
/// The legacy DV flag is ignored: metadata must never force a container change.
///
/// Deterministic, no I/O - safe to call from display code.
pub fn resolve_container(
    source_path: &str,
    container_setting: &str,
    _is_dovi_tier1: bool,
) -> &'static str {
    match container_setting {
        "mp4" => "mp4",
        "mkv" => "mkv",
        _ => {
            // "auto" or any other value: derive from source extension
            let ext = std::path::Path::new(source_path)
                .extension()
                .map(|e| e.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            match ext.as_str() {
                "mp4" | "m4v" => "mp4",
                "mkv" => "mkv",
                _ => "mkv", // AVI, TS, WMV, MOV, WebM, FLV, VOB, etc. → MKV
            }
        }
    }
}

/// Resolve the final output container extension for a file, applying the exact
/// rules `encode_single_file` uses: compatibility mode forces MP4;
/// otherwise the container is derived from the configured
/// setting / source extension. Centralised so the encode path and every
/// remote-staging back-copy path resolve the extension identically and cannot
/// drift apart (the back-copy must look for the file the encode actually wrote).
pub fn resolve_output_ext(
    source_path: &str,
    _dovi_profile: Option<u8>,
    settings: &BatchSettings,
    _preserve_hdr: bool,
) -> &'static str {
    if settings.compatibility_mode {
        return "mp4";
    }
    resolve_container(source_path, &settings.output_container, false)
}

/// Determine codec, encoder, container, and audio strategy for a single file
/// based on its source properties and the user's batch settings.
///
/// `source_video_codec` is the lowercase ffmpeg codec name (e.g. "hevc", "h264", "av1", "mpeg2video").
/// `source_container` is the file extension without the dot (e.g. "mkv", "mp4", "avi").
pub fn resolve_file_settings(
    source_video_codec: &str,
    source_container: &str,
    settings: &BatchSettings,
    detected_encoders: &[EncoderInfo],
) -> ResolvedFileSettings {
    // 1. Codec family — explicit --codec override takes priority, then
    //    compatibility mode, then per-file detection from the source codec.
    let codec_family = match settings.codec_family.as_str() {
        "h264" => "h264",
        "hevc" => "hevc",
        _ if settings.compatibility_mode => "h264",
        _ if source_video_codec == "h264" => "h264",
        _ if source_video_codec == "hevc" || source_video_codec == "h265" => "hevc",
        _ if settings.preserve_av1 && source_video_codec == "av1" => "av1",
        _ => "hevc", // default conversion target
    };

    // 2. Encoder — explicit --encoder override takes priority, then
    //    precision mode forces software, then auto-detect from detected list.
    let encoder_name = match settings.video_encoder.as_str() {
        "" | "auto" => {
            if settings.precision_mode {
                software_fallback(codec_family).to_string()
            } else {
                // Prefer HW encoders over SW for the same codec family.
                // The detection list may contain both (HW added after test
                // encodes, SW added as immediate fallbacks).
                detected_encoders
                    .iter()
                    .filter(|e| e.codec_family == codec_family)
                    .min_by_key(|e| !e.is_hardware)
                    .map(|e| e.name.clone())
                    .unwrap_or_else(|| software_fallback(codec_family).to_string())
            }
        }
        forced => forced.to_string(),
    };

    // 3. Container - use the shared resolver
    let container_ext = if settings.compatibility_mode {
        "mp4".to_string()
    } else {
        // source_container is just the extension; build a fake path for resolve_container
        let fake_path = format!("file.{}", source_container);
        resolve_container(&fake_path, &settings.output_container, false).to_string()
    };

    // 4. Audio strategy — compat mode always forces AAC (overrides --audio),
    //    then explicit --audio takes priority, then default copy-capped.
    let cap = settings.audio_cap;
    let audio_strategy = if settings.compatibility_mode {
        AudioStrategy::CompatCapped { cap_kbps: cap }
    } else {
        match settings.audio_encoder.as_str() {
            "copy" => AudioStrategy::CopyAll,
            "aac" => AudioStrategy::CompatCapped { cap_kbps: cap },
            // ac3, eac3, or any other codec: use CopyCapped so the
            // fallback codec is resolved per-stream inside
            // build_audio_args_from_probe.
            _ => AudioStrategy::CopyCapped { cap_kbps: cap },
        }
    };

    ResolvedFileSettings {
        codec_family: codec_family.to_string(),
        encoder_name,
        container_ext,
        audio_strategy,
    }
}

// ── Batch encoding (§10) ────────────────────────────────────────

// ── Pre-flight check (Phase DV) ────────────────────────────────

/// Warning about a file that won't get its best-possible encode.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreflightWarning {
    pub file_name: String,
    pub source_type: String,
    pub best_possible: String,
    pub actual_outcome: String,
    pub missing_tool: Option<String>,
}

/// Scan the queue and report files that will be degraded due to missing tools.
pub fn preflight_scan(queue: &[QueueItem]) -> Vec<PreflightWarning> {
    queue
        .iter()
        .filter(|item| item.status == QueueItemStatus::Pending)
        .filter_map(|item| {
            let reason = item
                .probe
                .video_format
                .hdr10_fallback_error(item.probe.dovi_profile, item.probe.dovi_bl_compat_id);
            reason.map(|reason| PreflightWarning {
                file_name: item.file_name.clone(),
                source_type: format!("Dolby Vision Profile {}", item.probe.dovi_profile.unwrap()),
                best_possible: "Keep original without re-encoding".into(),
                actual_outcome: reason,
                missing_tool: None,
            })
        })
        .collect()
}

/// Typed deserialization target for the frontend's `startBatch` JSON payload.
///
/// Field names use `camelCase` to match the frontend keys.  Each field has a
/// serde default that mirrors the old `unwrap_or()` fallbacks so that missing
/// keys are handled identically.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchRequest {
    #[serde(default = "batch_req_default_output_folder")]
    pub output_folder: String,
    #[serde(default = "batch_req_default_output_mode")]
    pub output_mode: String,
    #[serde(default = "batch_req_default_target_bitrate")]
    pub target_bitrate: f64,
    #[serde(default = "batch_req_default_qp_i")]
    pub qp_i: u32,
    #[serde(default = "batch_req_default_qp_p")]
    pub qp_p: u32,
    #[serde(default = "batch_req_default_crf")]
    pub crf: u32,
    #[serde(default = "batch_req_default_rate_control_mode")]
    pub rate_control_mode: String,
    #[serde(default = "batch_req_default_pix_fmt")]
    pub pix_fmt: String,
    #[serde(default)]
    pub delete_source: bool,
    #[serde(default)]
    pub save_log: bool,
    #[serde(default = "batch_req_default_peak_multiplier")]
    pub peak_multiplier: f64,
    #[serde(default)]
    pub threads: u32,
    #[serde(default)]
    pub low_priority: bool,
    #[serde(default)]
    pub precision_mode: bool,
    #[serde(default)]
    pub hardware_decoding: bool,
    #[serde(default = "batch_req_default_preserve_hdr")]
    pub preserve_hdr: bool,
    #[serde(default)]
    pub preserve_hdr10plus: bool,
    #[serde(default)]
    pub compatibility_mode: bool,
    #[serde(default)]
    pub preserve_av1: bool,
    #[serde(default)]
    pub force_local: bool,
    // GUI-only post-batch fields (not part of BatchSettings)
    #[serde(default)]
    pub show_toast: bool,
    #[serde(default = "batch_req_default_post_action")]
    pub post_action: String,
    #[serde(default)]
    pub post_countdown: u32,
    #[serde(default)]
    pub overwrite: bool,
}

fn batch_req_default_output_folder() -> String {
    "output".to_string()
}
fn batch_req_default_output_mode() -> String {
    "folder".to_string()
}
fn batch_req_default_target_bitrate() -> f64 {
    5.0
}
fn batch_req_default_qp_i() -> u32 {
    20
}
fn batch_req_default_qp_p() -> u32 {
    22
}
fn batch_req_default_crf() -> u32 {
    20
}
fn batch_req_default_rate_control_mode() -> String {
    "QP".to_string()
}
fn batch_req_default_pix_fmt() -> String {
    "yuv420p".to_string()
}
fn batch_req_default_preserve_hdr() -> bool {
    true
}
fn batch_req_default_peak_multiplier() -> f64 {
    1.5
}
fn batch_req_default_post_action() -> String {
    "None".to_string()
}

impl BatchRequest {
    /// Convert the validated frontend request into the internal `BatchSettings`
    /// used by the encode loop, applying any clamping rules.
    pub fn into_batch_settings(self) -> BatchSettings {
        BatchSettings {
            output_folder: self.output_folder,
            output_container: "auto".to_string(),
            output_mode: self.output_mode,
            threshold: self.target_bitrate,
            qp_i: self.qp_i.min(51),
            qp_p: self.qp_p.min(51),
            crf_val: self.crf.min(51),
            rate_control_mode: self.rate_control_mode,
            video_encoder: "auto".to_string(),
            codec_family: "auto".to_string(),
            // GUI defaults to passthrough so Atmos and other source audio are preserved.
            // Compatibility mode still enforces MP4-compatible AAC where necessary.
            audio_encoder: "copy".to_string(),
            audio_cap: 640,
            pix_fmt: self.pix_fmt,
            delete_source: self.delete_source,
            save_log: self.save_log,
            post_command: None,
            peak_multiplier: self.peak_multiplier,
            threads: self.threads.min(64),
            low_priority: self.low_priority,
            precision_mode: self.precision_mode,
            hardware_decoding: self.hardware_decoding,
            preserve_hdr: self.preserve_hdr,
            preserve_hdr10plus: self.preserve_hdr10plus,
            compatibility_mode: self.compatibility_mode,
            preserve_av1: self.preserve_av1,
            force_local: self.force_local,
        }
    }
}

/// Batch encoding settings, extracted from either CLI args or GUI settings JSON.
#[derive(Debug, Clone)]
pub struct BatchSettings {
    pub output_folder: String,
    pub output_mode: String, // "folder" | "beside" | "replace"
    pub threshold: f64,
    pub qp_i: u32,
    pub qp_p: u32,
    pub crf_val: u32,
    pub rate_control_mode: String,
    pub pix_fmt: String,
    pub delete_source: bool,
    pub save_log: bool,
    pub post_command: Option<String>,
    pub peak_multiplier: f64,
    pub threads: u32,
    pub low_priority: bool,
    pub precision_mode: bool,
    pub hardware_decoding: bool,
    pub preserve_hdr: bool,
    pub preserve_hdr10plus: bool,
    pub compatibility_mode: bool,
    pub preserve_av1: bool,
    pub force_local: bool,
    pub video_encoder: String,
    pub codec_family: String,
    pub audio_encoder: String,
    pub audio_cap: u32,
    pub output_container: String,
}

/// Result of encoding a single file, returned by `encode_single_file`.
/// The caller accumulates counters and decides whether to break the outer loop.
enum EncodeFileResult {
    /// Encode completed successfully.
    Done,
    /// Encode failed (ffmpeg error, missing output, etc.).
    Failed,
    /// File was skipped (output exists and user chose to skip).
    Skipped,
    /// User cancelled this file only (cancel-current).
    CancelledCurrent,
    /// User cancelled the entire batch (cancel-all / break 'outer).
    CancelledAll,
}

/// A work unit in the wave-aware encode loop: either a single local file
/// or a wave of remote files to stage together.
struct WaveWork {
    indices: Vec<usize>,
    /// Staging directory for this wave (None = local, no staging)
    staging_dir: Option<std::path::PathBuf>,
    total_stage_bytes: u64,
    wave_number: u32, // 0 for local items
    total_waves: u32,
}

fn metadata_failure(
    idx: usize,
    queue: &mut [QueueItem],
    output: Option<&std::path::Path>,
    reason: &str,
    sink: &dyn EventSink,
) -> bool {
    if let Some(output) = output {
        // FFmpeg already completed the video encode. A later HDR10+ packaging
        // failure must not throw away that work. Retain it as a recovery file,
        // without labeling it as HDR10+-verified or touching the source.
        if output.is_file() {
            let filename = output
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "encoded.mkv".to_string());
            let mut preserved = None;
            for suffix in 0..1000 {
                let recovery_name = if suffix == 0 {
                    format!("{filename}.hdr10plus-failed-encoded")
                } else {
                    format!("{filename}.hdr10plus-failed-encoded-{suffix}")
                };
                let candidate = output.with_file_name(recovery_name);
                if candidate.exists() {
                    continue;
                }
                match std::fs::rename(output, &candidate) {
                    Ok(()) => {
                        preserved = Some(candidate);
                        break;
                    }
                    Err(error) => {
                        sink.log(&format!(
                            "  WARNING: Could not move encoded video to a recovery file: {error}. Original encoded file remains at {}",
                            output.display()
                        ));
                        break;
                    }
                }
            }
            if let Some(path) = preserved {
                sink.log(&format!(
                    "  RECOVERY: Completed encode preserved at {} (HDR10+ finalization was unsuccessful; HDR10+ metadata is not verified).",
                    path.display()
                ));
            } else if output.exists() {
                sink.log(&format!(
                    "  RECOVERY: Encoded file retained at {} (HDR10+ not verified).",
                    output.display()
                ));
            }
        }
    }
    if !sink.cancellation_requested() {
        sink.log(&format!("  ERROR: HDR10+ preservation failed: {reason}. Source retained. Disable Preserve HDR10+ metadata for the fast HDR10 path."));
        queue[idx].status = QueueItemStatus::Failed;
        sink.queue_item_updated(idx, "Failed");
    }
    false
}

/// Post-encode processing: size check, HDR10 validation, MKV tag
/// patching, and source replacement/deletion.
///
/// Returns `true` if the file was completed successfully, `false` on failure.
/// On failure the queue item is marked as `Failed`.
#[allow(clippy::too_many_arguments)]
async fn handle_post_encode(
    idx: usize,
    queue: &mut [QueueItem],
    decision: &EncodeDecision,
    temp_output_file: &std::path::Path,
    output_file: &std::path::Path,
    output_str: &str,
    final_output_str: &str,
    ext: &str,
    item_full_path: &str,
    is_image_source: bool,
    is_replace_mode: bool,
    tone_mapped: bool,
    item_duration_secs: f64,
    final_frame_count: Option<u64>,
    metadata: &Option<crate::hdr10plus_pipeline::ExtractedHdr10Plus>,
    settings: &BatchSettings,
    audio_strategy: &AudioStrategy,
    log_writer: &mut Option<std::io::BufWriter<std::fs::File>>,
    write_log: &(dyn Fn(&mut Option<std::io::BufWriter<std::fs::File>>, &str) + Send + Sync),
    sink: &dyn EventSink,
) -> bool {
    if sink.cancellation_requested() {
        return false;
    }
    if let Some(metadata) = metadata {
        if !matches!(decision, EncodeDecision::Copy) {
            if let Err(reason) =
                crate::hdr10plus_pipeline::inject_pending(temp_output_file, metadata, sink).await
            {
                return metadata_failure(idx, queue, Some(temp_output_file), &reason, sink);
            }
        }
    }
    sink.processing_stage("finalizing", "Validating and finalizing output...", None);
    // Post-encode size check
    // In replace mode, ffmpeg wrote to temp_output_file; in other modes temp == final.
    // Skip for Copy decisions: the output is just a remux of the source, so
    // a "remux source instead" fallback would produce the same file again.
    let is_copy = matches!(decision, EncodeDecision::Copy);
    if temp_output_file.exists() {
        let src_size = std::fs::metadata(item_full_path)
            .map(|m| m.len())
            .unwrap_or(0);
        let dst_size = std::fs::metadata(temp_output_file)
            .map(|m| m.len())
            .unwrap_or(0);

        // Container overhead deadband: MP4 moov atoms store per-frame index
        // entries (~16 bytes each for stsz + stco + ctts) plus audio sample
        // tables (~500 bytes/sec per stream). Outputs that exceed the source
        // by less than this + 20% margin are within expected container
        // overhead and should not trigger the remux fallback.
        let fps = queue[idx].probe.video_fps.max(1.0);
        let num_audio = queue[idx].probe.audio_streams.len() as f64;
        let video_overhead = item_duration_secs * fps * 16.0;
        let audio_overhead = item_duration_secs * 500.0 * num_audio;
        let deadband = ((video_overhead + audio_overhead + 10_240.0) * 1.2) as u64;

        if dst_size > src_size + deadband
            && src_size > 0
            && !is_image_source
            && !is_copy
            && !tone_mapped
        {
            sink.log(&format!(
                "  WARNING: Output ({:.1}MB) larger than source ({:.1}MB) - remuxing source instead",
                dst_size as f64 / 1_000_000.0, src_size as f64 / 1_000_000.0,
            ));
            write_log(log_writer, "  Output larger than source - remuxing");
            let _ = std::fs::remove_file(temp_output_file);

            // Remux with audio-strategy-aware stream mapping: use the same
            // audio strategy as the main encode path so compat mode re-encodes
            // incompatible audio (e.g. DTS/TrueHD → AAC for MP4).
            let (audio_map, audio_codec) = build_audio_args_from_probe(
                &queue[idx].probe.audio_streams,
                audio_strategy,
                ext,
                sink,
            );

            let mut remux_args: Vec<String> = vec![
                "-y".into(),
                "-i".into(),
                item_full_path.into(),
                "-map".into(),
                "0:v".into(),
            ];
            remux_args.extend(audio_map);

            if ext == "mp4" {
                // MP4: convert text subs to mov_text; ffmpeg silently
                // skips bitmap subs (PGS, DVB) that can't be converted.
                remux_args.extend([
                    "-map".into(),
                    "0:s?".into(),
                    "-c:v".into(),
                    "copy".into(),
                    "-c:s".into(),
                    "mov_text".into(),
                ]);
            } else {
                remux_args.extend([
                    "-map".into(),
                    "0:s?".into(),
                    "-c:v".into(),
                    "copy".into(),
                    "-c:s".into(),
                    "copy".into(),
                ]);
            }
            remux_args.extend(audio_codec);
            if ext == "mp4" {
                remux_args.extend(["-strict".into(), "unofficial".into()]);
            }
            remux_args.push(output_str.into());

            let remux_refs: Vec<&str> = remux_args.iter().map(|s| s.as_str()).collect();
            let remux_output = ffbin::ffmpeg_command()
                .args(&remux_refs)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .map(|child| child.wait_with_output())
                .ok();
            let remux_status = match remux_output {
                Some(fut) => fut.await.ok(),
                None => None,
            };

            match remux_status {
                Some(ref o) if o.status.success() => {
                    sink.log(&format!(
                        "  Remuxed source to {} → {}",
                        ext.to_uppercase(),
                        output_str
                    ));
                    write_log(log_writer, &format!("  Remuxed to {}", ext.to_uppercase()));
                }
                _ => {
                    if let Some(ref o) = remux_status {
                        let stderr = String::from_utf8_lossy(&o.stderr);
                        sink.log(&format!("  Remux stderr: {stderr}"));
                        write_log(log_writer, &format!("  Remux stderr: {stderr}"));
                    }
                    sink.log("  ERROR: Remux failed");
                    write_log(log_writer, "  ERROR: Remux failed");
                    if temp_output_file.exists() {
                        let _ = std::fs::remove_file(temp_output_file);
                    }
                    queue[idx].status = QueueItemStatus::Failed;
                    sink.queue_item_updated(idx, "Failed");
                    return false;
                }
            }
        } else {
            sink.log(&format!(
                "  Done → {} ({:.1}MB from {:.1}MB)",
                final_output_str,
                dst_size as f64 / 1_000_000.0,
                src_size as f64 / 1_000_000.0,
            ));
            write_log(
                log_writer,
                &format!(
                    "  Done: {:.1}MB from {:.1}MB",
                    dst_size as f64 / 1_000_000.0,
                    src_size as f64 / 1_000_000.0,
                ),
            );
        }

        // Patch stale MKV stream statistics tags with the actual values.
        // Only needed for MKV encodes (not copies, not MP4). Source muxers
        // write BPS/NUMBER_OF_BYTES/DURATION tags that ffmpeg copies
        // verbatim; after re-encoding these are wrong and mislead
        // subsequent probes.
        if ext == "mkv" && !matches!(decision, EncodeDecision::Copy) {
            let audio_total_bps: u64 = queue[idx]
                .probe
                .audio_streams
                .iter()
                .map(|s| s.bitrate_kbps as u64 * 1000)
                .sum();

            let frames = final_frame_count.filter(|&f| f > 0);

            match crate::mkv_tags::lightweight_repair(
                temp_output_file,
                dst_size,
                item_duration_secs,
                audio_total_bps,
                frames,
            ) {
                Ok((n, bps)) if n > 0 => {
                    let mbps = bps as f64 / 1_000_000.0;
                    sink.log(&format!(
                        "  Updated {} MKV tag{} (video: {:.2}Mbps)",
                        n,
                        if n == 1 { "" } else { "s" },
                        mbps
                    ));
                }
                Ok(_) => {
                    sink.log("  No MKV statistics tags to update");
                }
                Err(e) => {
                    sink.log(&format!("  WARNING: Could not update MKV tags: {e}"));
                }
            }
        }

        if let Some(metadata) = metadata {
            if let Err(reason) =
                crate::hdr10plus_pipeline::verify_output(temp_output_file, metadata, sink).await
            {
                return metadata_failure(idx, queue, Some(temp_output_file), &reason, sink);
            }
        }

        if !is_image_source {
            let validation =
                validate_encoded_output(&queue[idx].probe, output_str, tone_mapped, sink).await;
            if let Err(reason) = validation {
                sink.log(&format!(
                    "  ERROR: Output rejected: {reason}. Source retained."
                ));
                write_log(log_writer, &reason);
                let _ = std::fs::remove_file(temp_output_file);
                queue[idx].status = QueueItemStatus::Failed;
                sink.queue_item_updated(idx, "Failed");
                return false;
            }
        }

        if sink.cancellation_requested() {
            return false;
        }
        if sink.cancellation_requested() {
            return false;
        }
        // Once the source replacement starts, finish the atomic commit before
        // accepting cancellation. The completed output must never be removed.
        // Replace mode: safely swap source with encoded output.
        // Rename source to .bak first so we can recover if the final
        // rename fails (avoids TOCTOU data loss).
        if is_replace_mode && temp_output_file.exists() {
            let backup = std::path::PathBuf::from(format!("{}.histv-bak", item_full_path));
            let source_backed_up = match std::fs::rename(item_full_path, &backup) {
                Ok(_) => true,
                Err(e) => {
                    let warn = format!("  WARNING: Could not back up source for replacement: {e}");
                    sink.log(&warn);
                    write_log(log_writer, &warn);
                    false
                }
            };
            // Rename temp to final
            if temp_output_file != output_file {
                if let Err(e) = std::fs::rename(temp_output_file, output_file) {
                    let warn = format!("  WARNING: Could not rename temp to final path: {e}");
                    sink.log(&warn);
                    write_log(log_writer, &warn);
                    // Restore the backup so the source is not lost
                    if source_backed_up {
                        let _ = std::fs::rename(&backup, item_full_path);
                    }
                } else {
                    // Success — remove the backup
                    if source_backed_up {
                        let _ = std::fs::remove_file(&backup);
                    }
                    let msg = format!("  Replaced source → {}", final_output_str);
                    sink.log(&msg);
                    write_log(log_writer, &msg);
                }
            } else if source_backed_up {
                // temp_output_file == output_file, just clean up backup
                let _ = std::fs::remove_file(&backup);
            }
        } else if settings.delete_source && temp_output_file.exists() {
            // Normal delete-source mode
            match std::fs::remove_file(item_full_path) {
                Ok(_) => {
                    sink.log("  Source file deleted");
                    write_log(log_writer, "  Source deleted");
                }
                Err(e) => {
                    let warn = format!("  WARNING: Could not delete source: {e}");
                    sink.log(&warn);
                    write_log(log_writer, &warn);
                }
            }
        }

        queue[idx].status = QueueItemStatus::Done;
        sink.queue_item_updated(idx, "Done");
        true
    } else {
        sink.log("  ERROR: Output file not found after encode");
        write_log(log_writer, "  ERROR: Output file not found");
        queue[idx].status = QueueItemStatus::Failed;
        sink.queue_item_updated(idx, "Failed");
        false
    }
}

/// Encode a single file from the queue. Handles field extraction, MKV tag
/// repair, encode decision, HDR10 fallback checks, FFmpeg arg
/// assembly, precision mode CRF probe, two-pass or single-pass encode,
/// hardware fallback on failure, and post-encode processing.
#[allow(clippy::too_many_arguments)]
async fn encode_single_file(
    idx: usize,
    queue: &mut [QueueItem],
    settings: &BatchSettings,
    output_dir_override: Option<&std::path::Path>,
    detected_encoders: &[EncoderInfo],
    preserve_hdr: bool,
    cached_ram_gb: u64,
    qi_str: &str,
    qp_str: &str,
    crf_str: &str,
    file_counter: u32,
    total: usize,
    log_writer: &mut Option<std::io::BufWriter<std::fs::File>>,
    write_log: &(dyn Fn(&mut Option<std::io::BufWriter<std::fs::File>>, &str) + Send + Sync),
    sink: &dyn EventSink,
    batch_control: &dyn BatchControl,
    batch_stderr_log: Option<SharedStderrLog>,
) -> EncodeFileResult {
    let pipeline_sink = crate::pipeline_progress::PipelineSink {
        inner: sink,
        control: batch_control,
    };
    let sink: &dyn EventSink = &pipeline_sink;
    sink.processing_stage("preparing", "Preparing video...", None);
    // Extract read-only data from the queue item. Cloning individual
    // fields avoids a full QueueItem clone (which includes audio_streams
    // Vec, all Strings, and fields unused in the loop body).
    let item_full_path = queue[idx].full_path.clone();
    let item_file_name = queue[idx].file_name.clone();
    let item_base_name = queue[idx].base_name.clone();
    let item_video_codec = queue[idx].probe.video_codec.clone();
    let item_video_width = queue[idx].probe.video_width;
    let item_video_height = queue[idx].probe.video_height;
    let item_video_bitrate_mbps = queue[idx].probe.video_bitrate_mbps;
    let item_duration_secs = queue[idx].probe.duration_secs;
    let item_is_hdr = queue[idx].probe.is_hdr;
    let is_image_source = matches!(item_video_codec.as_str(), "gif" | "apng" | "mjpeg" | "webp");
    let is_animated_webp =
        item_video_codec == "webp" && item_full_path.to_lowercase().ends_with(".webp");

    let item_dovi_profile = queue[idx].probe.dovi_profile;

    // Per-file codec/encoder/container resolution (§2.4.0)
    let source_ext = std::path::Path::new(&item_full_path)
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let mut resolved =
        resolve_file_settings(&item_video_codec, &source_ext, settings, detected_encoders);
    let source_format = queue[idx].probe.video_format.clone();
    let source_format = &source_format;
    // 12-bit and non-4:2:0 material must not be sent to an 8/10-bit 4:2:0
    // hardware surface. Keep the source precision on a compatible SW encoder.
    if source_format.bit_depth > 10
        || source_format.pixel_format.starts_with("yuv422")
        || source_format.pixel_format.starts_with("yuv444")
        || (source_format.bit_depth == 10 && resolved.codec_family == "h264")
    {
        resolved.encoder_name = software_fallback(&resolved.codec_family).into();
        sink.log("  Source format requires software encoding to preserve precision/chroma");
    }
    let file_encoder = &resolved.encoder_name;
    let file_codec_family = &resolved.codec_family;
    let file_ext =
        resolve_output_ext(&item_full_path, item_dovi_profile, settings, preserve_hdr).to_string();
    let sw_fallback = software_fallback(file_codec_family).to_string();
    let is_sw = file_encoder.starts_with("lib");
    let target_codec = file_codec_family.as_str();

    // SDR sources keep their precision. An explicit HDR opt-out converts HDR
    // to 8-bit BT.709; HDR10+ selection includes its required HDR10 base.
    let tonemap_filter = if item_is_hdr && !preserve_hdr {
        Some(TONEMAP_HABLE)
    } else {
        None
    };
    let file_pix_fmt = if tonemap_filter.is_some() {
        "yuv420p"
    } else {
        match source_format.bit_depth {
            10 => "p010le",
            12 => "yuv420p12le",
            _ => "yuv420p",
        }
    };

    let software_pixel_format = source_format
        .software_format()
        .unwrap_or_else(|_| "yuv420p".into());
    let file_pix_fmt = if is_sw && tonemap_filter.is_none() {
        software_pixel_format.as_str()
    } else {
        file_pix_fmt
    };
    sink.batch_status(&format!("[{}/{}] {}", file_counter, total, item_file_name));

    queue[idx].status = QueueItemStatus::Encoding;
    sink.queue_item_updated(idx, "Encoding");

    let ext = file_ext.as_str();
    let is_replace_mode = settings.output_mode == "replace";

    // Determine output path based on output mode (#8 - eliminate clone)
    let (output_file, temp_output_file) = match settings.output_mode.as_str() {
        "beside" => {
            // Create an "output" subfolder next to the input file
            let input_dir = std::path::Path::new(&item_full_path)
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."));
            let out_dir = input_dir.join("output");
            if !out_dir.exists() {
                if let Err(e) = std::fs::create_dir_all(&out_dir) {
                    let err_msg = format!("  ERROR: Could not create output folder: {e}");
                    sink.log(&err_msg);
                    write_log(log_writer, &err_msg);
                    queue[idx].status = QueueItemStatus::Failed;
                    sink.queue_item_updated(idx, "Failed");
                    return EncodeFileResult::Failed;
                }
            }
            let out = out_dir.join(format!("{}.{}", item_base_name, ext));
            (out.clone(), out)
        }
        "replace" => {
            // Encode to a temp file in the same directory, then replace the source
            let input_path = std::path::Path::new(&item_full_path);
            let input_dir = input_path
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."));
            let final_path = input_dir.join(format!("{}.{}", item_base_name, ext));
            let temp_path = input_dir.join(format!("{}.histv-tmp.{}", item_base_name, ext));
            (final_path, temp_path)
        }
        _ => {
            // "folder" - default: use the configured output folder. When that
            // folder is on a remote mount, the wave loop passes a local override
            // dir so the encode writes locally and is copied back afterwards
            // (mirrors replace/beside: keeps the network to a single burst).
            let folder: &std::path::Path = output_dir_override
                .unwrap_or_else(|| std::path::Path::new(&settings.output_folder));
            let out = folder.join(format!("{}.{}", item_base_name, ext));
            (out.clone(), out)
        }
    };
    // On Windows, prepend the extended-length path prefix when the path
    // exceeds 240 characters to avoid MAX_PATH (260) failures.
    #[cfg(target_os = "windows")]
    let output_str = {
        let raw = temp_output_file.to_string_lossy();
        if raw.len() > 240 && !raw.starts_with("\\\\?\\") {
            format!("\\\\?\\{}", raw)
        } else {
            raw.to_string()
        }
    };
    #[cfg(not(target_os = "windows"))]
    let output_str = temp_output_file.to_string_lossy().to_string();

    #[cfg(target_os = "windows")]
    let final_output_str = {
        let raw = output_file.to_string_lossy();
        if raw.len() > 240 && !raw.starts_with("\\\\?\\") {
            format!("\\\\?\\{}", raw)
        } else {
            raw.to_string()
        }
    };
    #[cfg(not(target_os = "windows"))]
    let final_output_str = output_file.to_string_lossy().to_string();

    let log_msg = format!("[{}/{}] {}", file_counter, total, item_file_name);
    sink.log(&log_msg);
    write_log(log_writer, &log_msg);

    if tonemap_filter.is_some() {
        sink.log("  HDR → SDR: tonemapping with Hable curve");
    }

    // Overwrite check - in replace mode, we're replacing the source so
    // no overwrite prompt is needed. In other modes, check the final output.
    if !is_replace_mode && output_file.exists() && !batch_control.overwrite_always() {
        let response = batch_control.overwrite_prompt(&final_output_str);
        match response.as_str() {
            "no" | "skip" => {
                sink.log("  Skipped (output exists)");
                write_log(log_writer, "  Skipped (output exists)");
                queue[idx].status = QueueItemStatus::Skipped;
                sink.queue_item_updated(idx, "Skipped");
                return EncodeFileResult::Skipped;
            }
            "always" => {
                batch_control.set_overwrite_always();
            }
            "cancel" => {
                return EncodeFileResult::CancelledAll;
            }
            _ => {} // "yes" - proceed
        }
    }

    // Correct stale queue bitrate estimates using file size, without writing
    // source tags. Explicit Repair remains available separately.
    let mut item_video_bitrate_mbps = item_video_bitrate_mbps;
    if let Some(bps) = crate::mkv_tags::estimate_bitrate_after_probe(
        &item_full_path,
        item_duration_secs,
        &queue[idx].probe.audio_streams,
    ) {
        let corrected_mbps = bps as f64 / 1_000_000.0;
        if (corrected_mbps - item_video_bitrate_mbps).abs() > 0.1 {
            sink.log(&format!(
                "  Bitrate estimate: {:.2}Mbps → {:.2}Mbps",
                item_video_bitrate_mbps, corrected_mbps
            ));
            item_video_bitrate_mbps = corrected_mbps;
            queue[idx].probe.video_bitrate_mbps = corrected_mbps;
            queue[idx].probe.video_bitrate_bps = bps as f64;
        }
    }

    // Determine encoding strategy
    let decision = decide_encode_strategy(
        item_video_bitrate_mbps,
        settings.threshold,
        &item_video_codec,
        target_codec,
        &RateControlParams {
            mode: &settings.rate_control_mode,
            qp_i: settings.qp_i,
            qp_p: settings.qp_p,
            crf_val: settings.crf_val,
        },
        settings.peak_multiplier,
    );

    let decision = require_tonemap_encode(
        decision,
        tonemap_filter.is_some(),
        &RateControlParams {
            mode: &settings.rate_control_mode,
            qp_i: settings.qp_i,
            qp_p: settings.qp_p,
            crf_val: settings.crf_val,
        },
    );

    if !matches!(decision, EncodeDecision::Copy) && !is_image_source {
        let invalid = source_format.preservation_error(
            target_codec,
            settings.compatibility_mode,
            item_is_hdr,
            item_dovi_profile,
            queue[idx].probe.dovi_bl_compat_id,
            tonemap_filter.is_some(),
        );
        if let Some(reason) = invalid {
            sink.log(&format!("  ERROR: {reason}"));
            queue[idx].status = QueueItemStatus::Failed;
            sink.queue_item_updated(idx, "Failed");
            return EncodeFileResult::Failed;
        }
    }

    if !matches!(decision, EncodeDecision::Copy) {
        if item_dovi_profile.is_some() {
            sink.log(if tonemap_filter.is_some() {
                "  Dolby Vision is not retained: converting the verified HDR10-compatible base layer to SDR."
            } else {
                "  Dolby Vision is not retained: encoding the verified HDR10-compatible base layer."
            });
        }
        if queue[idx].probe.has_hdr10plus && !settings.preserve_hdr10plus {
            sink.log(if tonemap_filter.is_some() {
                "  HDR10+ dynamic metadata is discarded during the requested SDR conversion. No metadata extraction passes."
            } else {
                "  HDR10+ dynamic metadata is intentionally discarded; standard HDR10 is preserved. No metadata extraction passes."
            });
        }
    }

    if settings.preserve_hdr10plus
        && queue[idx].probe.has_hdr10plus
        && !crate::hdr10plus_pipeline::preservation_requested(
            true,
            &item_video_codec,
            source_format.bit_depth,
        )
    {
        metadata_failure(
            idx,
            queue,
            None,
            "HDR10+ preservation requires a 10-bit HEVC source",
            sink,
        );
        return EncodeFileResult::Failed;
    }
    let metadata = if crate::hdr10plus_pipeline::preservation_requested(
        settings.preserve_hdr10plus,
        &item_video_codec,
        source_format.bit_depth,
    ) {
        sink.log("  HDR10+ preservation enabled: extra extraction, alignment, reinsertion and full metadata verification passes. Dolby Vision restoration remains disabled.");
        match crate::hdr10plus_pipeline::prepare(
            std::path::Path::new(&item_full_path),
            temp_output_file.parent(),
            target_codec,
            sink,
        )
        .await
        {
            Ok(result) => {
                if queue[idx].probe.has_hdr10plus && result.is_none() {
                    metadata_failure(
                        idx,
                        queue,
                        None,
                        "Advertised HDR10+ metadata could not be recovered",
                        sink,
                    );
                    return EncodeFileResult::Failed;
                }
                result
            }
            Err(_) if sink.cancellation_requested() => None,
            Err(reason) => {
                metadata_failure(idx, queue, None, &reason, sink);
                return EncodeFileResult::Failed;
            }
        }
    } else {
        None
    };

    if sink.cancellation_requested() {
        queue[idx].status = QueueItemStatus::Cancelled;
        sink.queue_item_updated(idx, "Cancelled");
        return if batch_control.should_cancel_all() {
            EncodeFileResult::CancelledAll
        } else {
            EncodeFileResult::CancelledCurrent
        };
    }
    // Build video args from the decision
    let mut video_args: Vec<String> = if matches!(decision, EncodeDecision::Copy) {
        vec!["-c:v".into(), "copy".into()]
    } else {
        vec!["-c:v".into(), file_encoder.to_string()]
    };
    let mut mode_desc;

    match &decision {
        EncodeDecision::Copy => {
            mode_desc = format!(
                "  Already {} at {:.2}Mbps (at/below target) - copying video",
                display_codec_family(file_codec_family),
                item_video_bitrate_mbps
            );
        }
        EncodeDecision::Vbr {
            target_bps,
            peak_bps,
        } => {
            let target_str = target_bps.to_string();
            let peak_str = peak_bps.to_string();
            video_args.extend(vbr_flags(file_encoder, &target_str, &peak_str));
            let peak_mbps = *peak_bps as f64 / 1_000_000.0;
            mode_desc = format!(
                "  Video: {:.2}Mbps [{}] {}x{} - VBR target {}Mbps peak {:.2}Mbps",
                item_video_bitrate_mbps,
                item_video_codec,
                item_video_width,
                item_video_height,
                settings.threshold,
                peak_mbps
            );
        }
        EncodeDecision::Cqp { .. } => {
            video_args.extend(cqp_flags(file_encoder, qi_str, qp_str));
            mode_desc = format!(
                "  Video: {:.2}Mbps [{}] {}x{} - CQP ({}/{})",
                item_video_bitrate_mbps,
                item_video_codec,
                item_video_width,
                item_video_height,
                settings.qp_i,
                settings.qp_p
            );
        }
        EncodeDecision::Crf { crf, .. } => {
            video_args.extend(crf_flags(file_encoder, crf_str, qi_str, qp_str));
            mode_desc = format!(
                "  Video: {:.2}Mbps [{}] {}x{} - CRF {}",
                item_video_bitrate_mbps, item_video_codec, item_video_width, item_video_height, crf
            );
        }
    }

    // ── Precision mode: CRF viability probe + lookahead + maxrate ──
    // Only applies to CRF decisions with software encoders.
    let is_crf_decision = matches!(decision, EncodeDecision::Crf { .. });

    if settings.precision_mode && is_sw && is_crf_decision && !is_image_source {
        // Probe CRF viability with 3 x 10-second samples
        sink.log("  Precision mode: probing CRF viability...");

        let probe_result = probe_crf_viability(
            &CrfProbeConfig {
                input_path: &item_full_path,
                duration_secs: item_duration_secs,
                video_args: &video_args,
                pix_fmt: file_pix_fmt,
                threads: settings.threads,
                low_priority: settings.low_priority,
                stderr_log: batch_stderr_log.clone(),
            },
            sink,
            batch_control,
        )
        .await;

        // Check for cancellation during probe
        if batch_control.should_cancel_all() {
            queue[idx].status = QueueItemStatus::Cancelled;
            sink.queue_item_updated(idx, "Cancelled");
            return EncodeFileResult::CancelledAll;
        }
        if batch_control.should_cancel_current() {
            queue[idx].status = QueueItemStatus::Cancelled;
            sink.queue_item_updated(idx, "Cancelled");
            return EncodeFileResult::CancelledCurrent;
        }

        match probe_result {
            Some(avg_mbps)
                if item_video_bitrate_mbps > 0.0 && avg_mbps > item_video_bitrate_mbps =>
            {
                // CRF would produce a larger file than source - fall back to CQP
                sink.log(&format!(
                    "  Precision mode: CRF estimate {:.2}Mbps exceeds source {:.2}Mbps - falling back to CQP",
                    avg_mbps, item_video_bitrate_mbps
                ));
                write_log(
                    log_writer,
                    &format!(
                        "  Precision: CRF {:.2}Mbps > source {:.2}Mbps - CQP fallback",
                        avg_mbps, item_video_bitrate_mbps
                    ),
                );
                // Rebuild video args as CQP
                video_args = vec!["-c:v".into(), file_encoder.clone()];
                video_args.extend(cqp_flags(file_encoder, qi_str, qp_str));
                mode_desc = format!(
                    "  Video: {:.2}Mbps [{}] {}x{} - CQP ({}/{}) (precision fallback)",
                    item_video_bitrate_mbps,
                    item_video_codec,
                    item_video_width,
                    item_video_height,
                    settings.qp_i,
                    settings.qp_p
                );
                sink.log(&mode_desc);
                write_log(log_writer, &mode_desc);
            }
            Some(_) | None => {
                // CRF is viable (or file was too short to probe) - enhance
                // based on available system RAM (#3 - use cached value).
                let use_lookahead = !precision_needs_two_pass_with_ram(cached_ram_gb);
                let lookahead = if use_lookahead {
                    lookahead_for_ram_with_cache(cached_ram_gb)
                } else {
                    0
                };

                if lookahead > 0 {
                    video_args.extend(vec!["-rc-lookahead".into(), lookahead.to_string()]);
                    sink.log(&format!(
                        "  Precision mode: rc-lookahead {} ({}GB RAM)",
                        lookahead, cached_ram_gb
                    ));
                } else if use_lookahead {
                    sink.log(&format!(
                        "  Precision mode: default lookahead ({}GB RAM)",
                        cached_ram_gb
                    ));
                } else {
                    sink.log(&format!("  Precision mode: two-pass ({}GB RAM - insufficient for extended lookahead)", cached_ram_gb));
                }

                // Cap with maxrate based on the user's target and peak multiplier
                if settings.threshold > 0.0 {
                    let maxrate_bps =
                        (settings.threshold * 1_000_000.0 * settings.peak_multiplier) as u64;
                    video_args.extend(vec![
                        "-maxrate".into(),
                        maxrate_bps.to_string(),
                        "-bufsize".into(),
                        (maxrate_bps * 2).to_string(),
                    ]);
                    sink.log(&format!(
                        "  Precision mode: maxrate {:.1}Mbps ({}Mbps x {}x)",
                        maxrate_bps as f64 / 1_000_000.0,
                        settings.threshold,
                        settings.peak_multiplier
                    ));
                }

                let precision_note = if lookahead > 0 {
                    format!(" (precision: lookahead {})", lookahead)
                } else if !use_lookahead {
                    " (precision: 2-pass)".to_string()
                } else {
                    " (precision)".to_string()
                };
                mode_desc = format!("{}{}", mode_desc, precision_note);
                sink.log(&mode_desc);
                write_log(log_writer, &mode_desc);
            }
        }
    } else {
        sink.log(&mode_desc);
        write_log(log_writer, &mode_desc);
    }

    // Build audio args (skipped for image sources like GIF)
    let (audio_map_args, audio_codec_args) = if is_image_source {
        (Vec::new(), Vec::new())
    } else {
        build_audio_args_from_probe(
            &queue[idx].probe.audio_streams,
            &resolved.audio_strategy,
            &file_ext,
            sink,
        )
    };

    let exit_code: i32;
    let mut final_frame_count: Option<u64>;

    // ── Animated WebP: dedicated decode + encode pipeline ──
    if is_animated_webp {
        sink.log("  Animated WebP: using RIFF decode pipeline");
        let webp_result = crate::webp_decode::transcode_animated_webp(
            &item_full_path,
            &output_str,
            &video_args,
            settings.threads,
            settings.low_priority,
            sink,
            batch_control,
        )
        .await;

        match webp_result {
            Ok(r) if r.was_cancelled => {
                if batch_control.should_cancel_all() {
                    queue[idx].status = QueueItemStatus::Cancelled;
                    sink.queue_item_updated(idx, "Cancelled");
                    return EncodeFileResult::CancelledAll;
                }
                queue[idx].status = QueueItemStatus::Cancelled;
                sink.queue_item_updated(idx, "Cancelled");
                return EncodeFileResult::CancelledCurrent;
            }
            Ok(r) if r.exit_code != 0 => {
                let err_msg = format!("  ERROR: WebP encode failed (exit code {})", r.exit_code);
                sink.log(&err_msg);
                write_log(log_writer, &err_msg);
                if temp_output_file.exists() {
                    let _ = std::fs::remove_file(&temp_output_file);
                }
                queue[idx].status = QueueItemStatus::Failed;
                sink.queue_item_updated(idx, "Failed");
                return EncodeFileResult::Failed;
            }
            Ok(r) => {
                final_frame_count = Some(r.frame_count);
            }

            Err(e) => {
                sink.log(&format!("  ERROR: {e}"));
                write_log(log_writer, &format!("  ERROR: {e}"));
                queue[idx].status = QueueItemStatus::Failed;
                sink.queue_item_updated(idx, "Failed");
                return EncodeFileResult::Failed;
            }
        }

        // Skip the normal ffmpeg path - jump to post-encode
    } else {
        // Assemble ffmpeg command
        let video_arg_refs: Vec<&str> = video_args.iter().map(|s| s.as_str()).collect();

        let hardware_decode = !matches!(decision, EncodeDecision::Copy)
            && source_format.gpu_decode_eligible(
                &item_video_codec,
                file_encoder,
                settings.hardware_decoding,
                tonemap_filter.is_some(),
                item_dovi_profile.is_some() || queue[idx].probe.has_hdr10plus,
            );
        if settings.hardware_decoding {
            sink.log(if hardware_decode { "  Hardware decoding active: VA-API, same verified GPU as encoder" }
                else { "  Hardware decoding bypassed: filters, codec, format or backend require software decoding" });
        }
        let colour_metadata =
            if tonemap_filter.is_none() && !matches!(decision, EncodeDecision::Copy) {
                Some(ColourMetadata {
                    primaries: &source_format.primaries,
                    trc: &source_format.transfer,
                    matrix: &source_format.matrix,
                    range: &source_format.range,
                })
            } else {
                None
            };
        // Do not crop every HEVC frame by a fixed amount. Modern drivers set
        // the conformance window themselves; validate output dimensions below.
        let vaapi_hevc_bsf = None;

        let ffmpeg_args = assemble_ffmpeg_args(&FfmpegAssemblyParts {
            input_path: &item_full_path,
            output_path: &output_str,
            video_args: &video_arg_refs,
            render_device: encoder_render_device(detected_encoders, file_encoder),
            hardware_decode,
            source_format: Some(source_format),
            pix_fmt: file_pix_fmt,
            audio_map_args: &audio_map_args,
            audio_codec_args: &audio_codec_args,
            is_image_source,
            threads: settings.threads,
            tonemap_filter,
            codec_family: target_codec,
            colour_metadata,
            vaapi_hevc_bsf,
        });

        let retry_args = assemble_ffmpeg_args(&FfmpegAssemblyParts {
            input_path: &item_full_path,
            output_path: &output_str,
            video_args: &video_arg_refs,
            render_device: encoder_render_device(detected_encoders, file_encoder),
            hardware_decode: false,
            source_format: Some(source_format),
            pix_fmt: if file_encoder.starts_with("lib") && tonemap_filter.is_none() {
                &software_pixel_format
            } else {
                file_pix_fmt
            },
            audio_map_args: &audio_map_args,
            audio_codec_args: &audio_codec_args,
            is_image_source,
            threads: settings.threads,
            tonemap_filter,
            codec_family: target_codec,
            colour_metadata,
            vaapi_hevc_bsf: None,
        });
        let cmd_line = format!("ffmpeg {}", ffmpeg_args.join(" "));
        sink.batch_command(&cmd_line);
        let cmd_log = format!("  CMD: {cmd_line}");
        sink.log(&cmd_log);
        write_log(log_writer, &cmd_log);

        // Shared context for run_ffmpeg_with_progress calls within this file.
        let ffmpeg_ctx = FfmpegRunContext {
            low_priority: settings.low_priority,
            stderr_log: batch_stderr_log.clone(),
            stderr_label: &item_file_name,
            sink,
            batch_control,
        };

        // Spawn ffmpeg - optionally as a two-pass encode.
        // Precision mode on low-RAM systems (<8GB) uses two-pass CRF instead
        // of extended lookahead, since the lookahead would consume too much memory.
        let precision_two_pass = settings.precision_mode
            && is_sw
            && is_crf_decision
            && precision_needs_two_pass_with_ram(cached_ram_gb);
        let use_two_pass = precision_two_pass;

        let proc_start = std::time::Instant::now();

        if use_two_pass {
            // ── Two-pass VBR (software encoders only) (#5) ──
            // TempDir auto-cleans passlog files on drop (including break 'outer).
            let passlog_dir = tempfile::Builder::new()
                .prefix("histv_2pass_")
                .tempdir()
                .map_err(|e| format!("Could not create passlog temp dir: {e}"));
            let passlog_prefix = match passlog_dir {
                Ok(ref d) => d.path().join("passlog").to_string_lossy().to_string(),
                Err(ref e) => {
                    sink.log(&format!("  ERROR: {e}"));
                    queue[idx].status = QueueItemStatus::Failed;
                    sink.queue_item_updated(idx, "Failed");
                    return EncodeFileResult::Failed;
                }
            };

            let fmt = if ext == "mp4" { "mp4" } else { "matroska" };

            // Pass 1: analysis only - build args fresh from components
            let pass1_args = build_two_pass_args(&ffmpeg_args, 1, &passlog_prefix, fmt);

            let pass1_cmd = format!("ffmpeg {}", pass1_args.join(" "));
            sink.batch_command(&pass1_cmd);
            write_log(log_writer, &format!("  CMD (pass 1): {pass1_cmd}"));

            match run_ffmpeg_with_progress(
                &pass1_args,
                item_duration_secs,
                Some((1, 2)),
                &ffmpeg_ctx,
            )
            .await
            {
                Ok(r) if r.was_cancelled_all => {
                    sink.log("  Cancelled (batch cancel)");
                    write_log(log_writer, "  Cancelled (batch cancel)");
                    queue[idx].status = QueueItemStatus::Cancelled;
                    sink.queue_item_updated(idx, "Cancelled");
                    return EncodeFileResult::CancelledAll;
                }
                Ok(r) if r.was_cancelled_current => {
                    sink.log("  Cancelled current file");
                    write_log(log_writer, "  Cancelled");
                    queue[idx].status = QueueItemStatus::Cancelled;
                    sink.queue_item_updated(idx, "Cancelled");
                    return EncodeFileResult::CancelledCurrent;
                }
                Ok(r) if r.exit_code != 0 => {
                    sink.log(&format!(
                        "  ERROR: Pass 1 failed (exit code {})",
                        r.exit_code
                    ));
                    write_log(log_writer, "  ERROR: Pass 1 failed");
                    queue[idx].status = QueueItemStatus::Failed;
                    sink.queue_item_updated(idx, "Failed");
                    return EncodeFileResult::Failed;
                }
                Err(e) => {
                    sink.log(&format!("  ERROR: {e}"));
                    write_log(log_writer, &format!("  ERROR: {e}"));
                    queue[idx].status = QueueItemStatus::Failed;
                    sink.queue_item_updated(idx, "Failed");
                    return EncodeFileResult::Failed;
                }
                _ => {} // pass 1 OK
            }

            // Pass 2: actual encode with stats from pass 1
            let pass2_args = build_two_pass_args(&ffmpeg_args, 2, &passlog_prefix, fmt);

            let pass2_cmd = format!("ffmpeg {}", pass2_args.join(" "));
            sink.batch_command(&pass2_cmd);
            write_log(log_writer, &format!("  CMD (pass 2): {pass2_cmd}"));

            match run_ffmpeg_with_progress(
                &pass2_args,
                item_duration_secs,
                Some((2, 2)),
                &ffmpeg_ctx,
            )
            .await
            {
                Ok(r) if r.was_cancelled_all => {
                    sink.log("  Cancelled (batch cancel)");
                    write_log(log_writer, "  Cancelled (batch cancel)");
                    if temp_output_file.exists() {
                        let _ = std::fs::remove_file(&temp_output_file);
                    }
                    queue[idx].status = QueueItemStatus::Cancelled;
                    sink.queue_item_updated(idx, "Cancelled");
                    return EncodeFileResult::CancelledAll;
                }
                Ok(r) if r.was_cancelled_current => {
                    sink.log("  Cancelled current file");
                    write_log(log_writer, "  Cancelled");
                    if temp_output_file.exists() {
                        let _ = std::fs::remove_file(&temp_output_file);
                        sink.log("  Partial output removed");
                    }
                    queue[idx].status = QueueItemStatus::Cancelled;
                    sink.queue_item_updated(idx, "Cancelled");
                    return EncodeFileResult::CancelledCurrent;
                }
                Ok(r) => {
                    exit_code = r.exit_code;
                    final_frame_count = Some(r.frame_count);
                }
                Err(e) => {
                    sink.log(&format!("  ERROR: {e}"));
                    write_log(log_writer, &format!("  ERROR: {e}"));
                    queue[idx].status = QueueItemStatus::Failed;
                    sink.queue_item_updated(idx, "Failed");
                    return EncodeFileResult::Failed;
                }
            }
            // passlog_dir dropped here — auto-cleans passlog files
        } else {
            // ── Single-pass encode ──
            match run_with_decode_retry(
                &ffmpeg_args,
                &retry_args,
                hardware_decode,
                source_format,
                item_duration_secs,
                tonemap_filter.is_some(),
                &ffmpeg_ctx,
            )
            .await
            {
                Ok(r) if r.was_cancelled_all => {
                    sink.log("  Cancelled (batch cancel)");
                    write_log(log_writer, "  Cancelled (batch cancel)");
                    if temp_output_file.exists() {
                        let _ = std::fs::remove_file(&temp_output_file);
                    }
                    queue[idx].status = QueueItemStatus::Cancelled;
                    sink.queue_item_updated(idx, "Cancelled");
                    return EncodeFileResult::CancelledAll;
                }
                Ok(r) if r.was_cancelled_current => {
                    sink.log("  Cancelled current file");
                    write_log(log_writer, "  Cancelled");
                    if temp_output_file.exists() {
                        let _ = std::fs::remove_file(&temp_output_file);
                        sink.log("  Partial output removed");
                    }
                    queue[idx].status = QueueItemStatus::Cancelled;
                    sink.queue_item_updated(idx, "Cancelled");
                    return EncodeFileResult::CancelledCurrent;
                }
                Ok(r) => {
                    exit_code = r.exit_code;
                    final_frame_count = Some(r.frame_count);
                }
                Err(e) => {
                    sink.log(&format!("  ERROR: {e}"));
                    write_log(log_writer, &format!("  ERROR: {e}"));
                    queue[idx].status = QueueItemStatus::Failed;
                    sink.queue_item_updated(idx, "Failed");
                    return EncodeFileResult::Failed;
                }
            }
        }

        let proc_duration = proc_start.elapsed();
        // Encoder failure fallback (#6 - use run_ffmpeg_with_progress)
        if exit_code != 0
            && proc_duration.as_secs() < 30
            && !matches!(decision, EncodeDecision::Copy)
            && *file_encoder != sw_fallback
        {
            if !batch_control.hw_fallback_offered() {
                batch_control.set_hw_fallback_offered();
                sink.log(&format!("  HW encoder failed for {}", item_file_name));
                let response = batch_control.fallback_prompt(&item_file_name);

                if response == "yes" {
                    sink.log(&format!(
                        "  Falling back to software encoder ({})...",
                        sw_fallback
                    ));
                    write_log(log_writer, &format!("  Fallback to {}", sw_fallback));

                    if temp_output_file.exists() {
                        let _ = std::fs::remove_file(&temp_output_file);
                    }

                    // Rebuild with software encoder
                    let mut sw_video_args: Vec<String>;
                    match &decision {
                        EncodeDecision::Vbr {
                            target_bps,
                            peak_bps,
                        } => {
                            sw_video_args = vec!["-c:v".into(), sw_fallback.clone()];
                            sw_video_args.extend(vbr_flags(
                                &sw_fallback,
                                &target_bps.to_string(),
                                &peak_bps.to_string(),
                            ));
                        }
                        _ => {
                            sw_video_args = vec!["-c:v".into(), sw_fallback.clone()];
                            sw_video_args.extend(cqp_flags(&sw_fallback, qi_str, qp_str));
                        }
                    }

                    // Reuse audio args from the primary encode - inputs haven't changed
                    let sw_ref: Vec<&str> = sw_video_args.iter().map(|s| s.as_str()).collect();
                    let sw_args = assemble_ffmpeg_args(&FfmpegAssemblyParts {
                        input_path: &item_full_path,
                        output_path: &output_str,
                        video_args: &sw_ref,
                        render_device: None,
                        hardware_decode: false,
                        source_format: Some(source_format),
                        pix_fmt: if tonemap_filter.is_none() {
                            &software_pixel_format
                        } else {
                            file_pix_fmt
                        },
                        audio_map_args: &audio_map_args,
                        audio_codec_args: &audio_codec_args,
                        is_image_source,
                        threads: settings.threads,
                        tonemap_filter,
                        codec_family: target_codec,
                        colour_metadata, // Preserve the source tags on software fallback
                        vaapi_hevc_bsf: None, // SW encode doesn't need VAAPI crop fix
                    });

                    let sw_cmd = format!("ffmpeg {}", sw_args.join(" "));
                    sink.batch_command(&sw_cmd);
                    let sw_cmd_log = format!("  CMD (fallback): {sw_cmd}");
                    sink.log(&sw_cmd_log);
                    write_log(log_writer, &sw_cmd_log);

                    // Use shared helper for progress, pause/cancel, low-priority (#6)
                    match run_ffmpeg_with_progress(&sw_args, item_duration_secs, None, &ffmpeg_ctx)
                        .await
                    {
                        Ok(r) if r.was_cancelled_all => {
                            sink.log("  Cancelled (batch cancel during fallback)");
                            write_log(log_writer, "  Cancelled (batch cancel during fallback)");
                            if temp_output_file.exists() {
                                let _ = std::fs::remove_file(&temp_output_file);
                            }
                            queue[idx].status = QueueItemStatus::Cancelled;
                            sink.queue_item_updated(idx, "Cancelled");
                            return EncodeFileResult::CancelledAll;
                        }
                        Ok(r) if r.was_cancelled_current => {
                            sink.log("  Cancelled current file (during fallback)");
                            write_log(log_writer, "  Cancelled (during fallback)");
                            if temp_output_file.exists() {
                                let _ = std::fs::remove_file(&temp_output_file);
                                sink.log("  Partial output removed");
                            }
                            queue[idx].status = QueueItemStatus::Cancelled;
                            sink.queue_item_updated(idx, "Cancelled");
                            return EncodeFileResult::CancelledCurrent;
                        }
                        Ok(r) if r.exit_code != 0 => {
                            sink.log("  ERROR: Software encoder also failed - stopping batch");
                            write_log(log_writer, "  ERROR: Software encoder also failed");
                            if temp_output_file.exists() {
                                let _ = std::fs::remove_file(&temp_output_file);
                            }
                            queue[idx].status = QueueItemStatus::Failed;
                            sink.queue_item_updated(idx, "Failed");
                            // Original code: break 'outer after SW fallback failure.
                            // Return CancelledAll to trigger the same outer break.
                            return EncodeFileResult::CancelledAll;
                        }
                        Err(e) => {
                            sink.log(&format!("  ERROR: Could not launch fallback: {e}"));
                            write_log(log_writer, &format!("  ERROR: Fallback launch failed: {e}"));
                            queue[idx].status = QueueItemStatus::Failed;
                            sink.queue_item_updated(idx, "Failed");
                            // Original code: break 'outer after fallback launch failure.
                            return EncodeFileResult::CancelledAll;
                        }
                        Ok(r) => {
                            final_frame_count = Some(r.frame_count);
                        } // fallback encode OK
                    }
                } else {
                    sink.log("  Stopping batch due to encoder failure");
                    write_log(log_writer, "  Batch stopped (encoder failure)");
                    if temp_output_file.exists() {
                        let _ = std::fs::remove_file(&temp_output_file);
                    }
                    queue[idx].status = QueueItemStatus::Failed;
                    sink.queue_item_updated(idx, "Failed");
                    // Original code: break 'outer when user declines fallback.
                    return EncodeFileResult::CancelledAll;
                }
            }
        } else if exit_code != 0 {
            if batch_control.hw_fallback_offered() && *file_encoder != sw_fallback {
                sink.log("  HW encoder failed (fallback already offered for this batch)");
                write_log(log_writer, "  HW encoder failed (fallback already offered)");
            }
            let err_msg = format!("  ERROR: ffmpeg exited with code {}", exit_code);
            sink.log(&err_msg);
            write_log(log_writer, &err_msg);
            if temp_output_file.exists() {
                let _ = std::fs::remove_file(&temp_output_file);
                sink.log("  Failed output removed");
            }
            queue[idx].status = QueueItemStatus::Failed;
            sink.queue_item_updated(idx, "Failed");
            return EncodeFileResult::Failed;
        }
    }

    // ── Post-encode processing ──
    let success = handle_post_encode(
        idx,
        queue,
        &decision,
        &temp_output_file,
        &output_file,
        &output_str,
        &final_output_str,
        ext,
        &item_full_path,
        is_image_source,
        is_replace_mode,
        tonemap_filter.is_some(),
        item_duration_secs,
        final_frame_count,
        &metadata,
        settings,
        &resolved.audio_strategy,
        log_writer,
        write_log,
        sink,
    )
    .await;

    if !success && sink.cancellation_requested() {
        let _ = std::fs::remove_file(&temp_output_file);
        queue[idx].status = QueueItemStatus::Cancelled;
        sink.queue_item_updated(idx, "Cancelled");
        return if batch_control.should_cancel_all() {
            EncodeFileResult::CancelledAll
        } else {
            EncodeFileResult::CancelledCurrent
        };
    }
    if success {
        EncodeFileResult::Done
    } else {
        EncodeFileResult::Failed
    }
}

/// Shared encoding loop used by the CLI. Takes trait objects for output and
/// control, and a mutable queue of items to encode.
///
/// Returns (done_count, fail_count, skip_count, was_cancelled).
pub async fn run_encode_loop(
    sink: &dyn EventSink,
    batch_control: &dyn BatchControl,
    queue: &mut [QueueItem],
    settings: &BatchSettings,
    detected_encoders: &[EncoderInfo],
    wave_plan: Option<Vec<WaveItem>>,
    disk_monitor: Option<&crate::disk_monitor::DiskMonitor>,
) -> (u32, u32, u32, bool) {
    let qi_str = settings.qp_i.to_string();
    let qp_str = settings.qp_p.to_string();
    let crf_str = settings.crf_val.to_string();

    // ── Pre-loop invariant computation (#2, #3, #4) ──
    let preserve_hdr = settings.preserve_hdr || settings.preserve_hdr10plus;
    let cached_ram_gb = get_system_ram_gb();

    // Collect pending indices
    let pending_indices: Vec<usize> = queue
        .iter()
        .enumerate()
        .filter(|(_, item)| item.status == QueueItemStatus::Pending)
        .map(|(i, _)| i)
        .collect();

    let total = pending_indices.len();
    let mut done_count: u32 = 0;
    let mut fail_count: u32 = 0;
    let mut skip_count: u32 = 0;
    let save_log = settings.save_log;
    let output_dir = std::path::Path::new(&settings.output_folder);
    let batch_start = std::time::Instant::now();
    let mut file_counter: u32 = 0;
    let mut was_cancelled = false;

    // ── Folder-mode remote-output staging (politeness) ──
    // When output mode is "folder" and the configured output folder lives on a
    // remote mount, folder-mode encodes inside a staging wave write to a local
    // override dir and are bulk-copied back afterwards, exactly like the
    // replace/beside back-copy. Computed once: a single mount lookup.
    let stage_folder_output = settings.output_mode == "folder" && {
        let mut mc = crate::remote::MountCache::new();
        mc.is_remote(std::path::Path::new(&settings.output_folder))
    };

    // ── Incremental log file (#7) ──
    // Open the log file at batch start and append each line as it's produced,
    // instead of buffering all lines in a Vec<String>.
    let mut log_writer: Option<std::io::BufWriter<std::fs::File>> = if save_log {
        let log_filename = format!(
            "encode_log_{}.txt",
            chrono::Local::now().format("%Y%m%d_%H%M%S")
        );
        let log_path = std::path::Path::new(&settings.output_folder).join(&log_filename);
        match std::fs::File::create(&log_path) {
            Ok(f) => {
                sink.log(&format!("  Log file: {}", log_path.display()));
                Some(std::io::BufWriter::new(f))
            }
            Err(e) => {
                sink.log(&format!("  WARNING: Could not create log file: {e}"));
                None
            }
        }
    } else {
        None
    };

    // Helper closure: append a line to the log file if open
    let write_log = |writer: &mut Option<std::io::BufWriter<std::fs::File>>, line: &str| {
        if let Some(ref mut w) = writer {
            use std::io::Write;
            let _ = writeln!(w, "{}", line);
        }
    };

    // ── Per-batch ffmpeg stderr log ──
    // One shared log file for all ffmpeg invocations in this batch.
    let batch_stderr_log: Option<SharedStderrLog> = if save_log {
        open_stderr_log(output_dir)
    } else {
        None
    };
    // Remember the log filename so we can check if it's empty after the batch
    let stderr_log_filename: Option<std::path::PathBuf> = if batch_stderr_log.is_some() {
        let log_dir = output_dir.join("ffmpeg_logs");
        // The file was just created by open_stderr_log — find the newest .log
        std::fs::read_dir(&log_dir).ok().and_then(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter(|e| {
                    e.path()
                        .extension()
                        .map(|ext| ext == "log")
                        .unwrap_or(false)
                })
                .max_by_key(|e| {
                    e.metadata()
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
                })
                .map(|e| e.path())
        })
    } else {
        None
    };

    sink.batch_started();

    // ── Wave-aware iteration (Phase 3) ────────────────────────
    //
    // Build work units from the wave plan. Each unit is either a single
    // local file or a wave of remote files to stage together.
    let work_units: Vec<WaveWork> = if let Some(plan) = wave_plan {
        let total_waves = plan
            .iter()
            .filter(|item| matches!(item, WaveItem::Wave { .. }))
            .count() as u32;
        let mut wave_num: u32 = 0;
        plan.into_iter()
            .map(|item| match item {
                WaveItem::Local { queue_index } => WaveWork {
                    indices: vec![queue_index],
                    staging_dir: None,
                    total_stage_bytes: 0,
                    wave_number: 0,
                    total_waves,
                },
                WaveItem::Wave {
                    indices,
                    total_stage_bytes,
                } => {
                    wave_num += 1;
                    WaveWork {
                        indices,
                        staging_dir: Some(crate::staging::resolve_staging_dir(None)),
                        total_stage_bytes,
                        wave_number: wave_num,
                        total_waves,
                    }
                }
            })
            .collect()
    } else {
        // No wave plan: flat iteration (backward compat for GUI)
        pending_indices
            .iter()
            .map(|&idx| WaveWork {
                indices: vec![idx],
                staging_dir: None,
                total_stage_bytes: 0,
                wave_number: 0,
                total_waves: 0,
            })
            .collect()
    };

    // Declared outside the loop so cleanup runs even if `break 'outer` exits early.
    let mut wave_staging_contexts: Vec<(usize, StagingContext, String)> = Vec::new();

    'outer: for work in &work_units {
        // ── Wave staging: stage all files before encoding ──────
        wave_staging_contexts.clear();

        if let Some(ref staging_dir) = work.staging_dir {
            let wave_size = work.indices.len();
            sink.wave_status(&format!(
                "Staging wave {}/{} ({} file{}, {})",
                work.wave_number,
                work.total_waves,
                wave_size,
                if wave_size == 1 { "" } else { "s" },
                crate::disk_monitor::format_bytes(work.total_stage_bytes),
            ));

            for (file_in_wave, &idx) in work.indices.iter().enumerate() {
                if batch_control.should_cancel_all() {
                    was_cancelled = true;
                    break 'outer;
                }

                sink.wave_progress(
                    work.wave_number,
                    work.total_waves,
                    (file_in_wave + 1) as u32,
                    wave_size as u32,
                );

                let original_path = queue[idx].full_path.clone();
                if let Some(ctx) = StagingContext::stage_file(
                    std::path::Path::new(&queue[idx].full_path),
                    staging_dir,
                    idx,
                    sink,
                )
                .await
                {
                    // Rewrite queue item path to the staged local copy
                    queue[idx].full_path = ctx.local_path().to_string_lossy().to_string();
                    wave_staging_contexts.push((idx, ctx, original_path));
                }
                // If staging fails, the file keeps its remote path and will be encoded in-place
            }
        }

        // Local override dir for folder-mode output when the output folder is
        // remote: the encode writes here, then wave cleanup copies it back to
        // the remote output folder. None unless this wave stages and the output
        // folder is remote, so the common path is unchanged.
        let folder_output_override: Option<std::path::PathBuf> =
            match (work.staging_dir.as_ref(), stage_folder_output) {
                (Some(sd), true) => {
                    let d = sd.join("folder-output");
                    let _ = tokio::fs::create_dir_all(&d).await;
                    Some(d)
                }
                _ => None,
            };

        // ── Encode each file in this work unit ────────────────
        for &idx in &work.indices {
            // Pause / cancel check - wait while paused before starting next file
            while batch_control.is_paused() {
                if batch_control.should_cancel_all() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
            if batch_control.should_cancel_all() {
                was_cancelled = true;
                break 'outer;
            }
            batch_control.clear_cancel_current();

            // Disk-aware mode: check partition usage and wait if over the limit
            if let Some(dm) = disk_monitor {
                if !dm.check_and_wait(sink, batch_control).await {
                    was_cancelled = true;
                    break 'outer;
                }
            }

            file_counter += 1;
            sink.batch_progress(file_counter, total);

            let result = encode_single_file(
                idx,
                queue,
                settings,
                folder_output_override.as_deref(),
                detected_encoders,
                preserve_hdr,
                cached_ram_gb,
                &qi_str,
                &qp_str,
                &crf_str,
                file_counter,
                total,
                &mut log_writer,
                &write_log,
                sink,
                batch_control,
                batch_stderr_log.clone(),
            )
            .await;

            match result {
                EncodeFileResult::Done => done_count += 1,
                EncodeFileResult::Failed => fail_count += 1,
                EncodeFileResult::Skipped => skip_count += 1,
                EncodeFileResult::CancelledCurrent => {}
                EncodeFileResult::CancelledAll => {
                    was_cancelled = true;
                    break 'outer;
                }
            }
        } // end per-file loop

        // ── Wave cleanup: restore original paths and clean up staged files ──
        let is_replace_or_beside =
            settings.output_mode == "replace" || settings.output_mode == "beside";
        for (idx, mut ctx, original_path) in wave_staging_contexts.drain(..) {
            // Copy output back to remote mount for replace/beside modes
            if is_replace_or_beside && queue[idx].status == QueueItemStatus::Done {
                let remote = std::path::Path::new(&original_path);
                let remote_dir = remote.parent().unwrap_or(std::path::Path::new("."));
                let base_name = remote.file_stem().unwrap_or_default().to_string_lossy();
                // Resolve the extension with the shared helper so the back-copy
                // looks for exactly the file the encode wrote, including the DV
                // Tier-1 / compatibility-mode MP4 override.
                let ext = resolve_output_ext(
                    &original_path,
                    queue[idx].probe.dovi_profile,
                    settings,
                    preserve_hdr,
                );
                let is_replace = settings.output_mode == "replace";

                let (local_output, final_remote) = if is_replace {
                    let staging_dir = work.staging_dir.as_ref().unwrap();
                    let local = staging_dir.join(format!("{}.{}", base_name, ext));
                    let remote_dest = remote_dir.join(format!("{}.{}", base_name, ext));
                    (local, remote_dest)
                } else {
                    let staging_dir = work.staging_dir.as_ref().unwrap();
                    let local = staging_dir
                        .join("output")
                        .join(format!("{}.{}", base_name, ext));
                    let remote_dest_dir = remote_dir.join("output");
                    let remote_dest = remote_dest_dir.join(format!("{}.{}", base_name, ext));
                    let _ = std::fs::create_dir_all(&remote_dest_dir);
                    (local, remote_dest)
                };

                if local_output.exists() {
                    sink.log(&format!(
                        "  Moving staged output to remote: {} → {}",
                        local_output.display(),
                        final_remote.display()
                    ));

                    if is_replace {
                        let temp_remote =
                            remote_dir.join(format!("{}.histv-tmp.{}", base_name, ext));
                        match tokio::fs::copy(&local_output, &temp_remote).await {
                            Ok(_) => {
                                // Backup-rename-delete pattern (mirrors local replace mode)
                                let backup =
                                    remote_dir.join(format!("{}.histv-bak.{}", base_name, ext));
                                let backed_up = if remote.exists() {
                                    std::fs::rename(remote, &backup).is_ok()
                                } else {
                                    false
                                };
                                if let Err(e) = std::fs::rename(&temp_remote, &final_remote) {
                                    sink.log(&format!(
                                        "  WARNING: Could not rename temp to final on remote: {e}"
                                    ));
                                    // Restore the backup so the original is not lost
                                    if backed_up {
                                        let _ = std::fs::rename(&backup, remote);
                                    }
                                } else {
                                    // Success — remove backup
                                    if backed_up {
                                        let _ = std::fs::remove_file(&backup);
                                    }
                                    sink.log(&format!(
                                        "  Replaced remote source → {}",
                                        final_remote.display()
                                    ));
                                }
                                let _ = std::fs::remove_file(&local_output);
                            }
                            Err(e) => {
                                sink.log(&format!(
                                    "  ERROR: Could not copy output to remote mount: {e}"
                                ));
                                let _ = std::fs::remove_file(&temp_remote);
                                sink.log(&format!(
                                    "  Encoded file preserved at: {}",
                                    local_output.display()
                                ));
                            }
                        }
                    } else {
                        match tokio::fs::copy(&local_output, &final_remote).await {
                            Ok(_) => {
                                let _ = std::fs::remove_file(&local_output);
                                sink.log(&format!(
                                    "  Copied output to remote → {}",
                                    final_remote.display()
                                ));
                            }
                            Err(e) => {
                                sink.log(&format!(
                                    "  ERROR: Could not copy output to remote mount: {e}"
                                ));
                                sink.log(&format!(
                                    "  Encoded file preserved at: {}",
                                    local_output.display()
                                ));
                            }
                        }
                    }
                }
            }

            // Folder mode: copy the locally-staged output back to the remote
            // output folder (the encode wrote to folder_output_override). Uses
            // the same extension-resolution as the beside back-copy above.
            if let Some(ref local_dir) = folder_output_override {
                if queue[idx].status == QueueItemStatus::Done {
                    let remote = std::path::Path::new(&original_path);
                    // Shared helper: identical extension rule to the encode path
                    // and the replace/beside back-copy (DV Tier-1 / compat → MP4).
                    let ext = resolve_output_ext(
                        &original_path,
                        queue[idx].probe.dovi_profile,
                        settings,
                        preserve_hdr,
                    );
                    let base_name = remote.file_stem().unwrap_or_default().to_string_lossy();
                    let local_output = local_dir.join(format!("{}.{}", base_name, ext));
                    let remote_dir = std::path::Path::new(&settings.output_folder);
                    let remote_dest = remote_dir.join(format!("{}.{}", base_name, ext));
                    if local_output.exists() {
                        let _ = std::fs::create_dir_all(remote_dir);
                        sink.log(&format!(
                            "  Moving staged output to remote: {} → {}",
                            local_output.display(),
                            remote_dest.display()
                        ));
                        match tokio::fs::copy(&local_output, &remote_dest).await {
                            Ok(_) => {
                                let _ = std::fs::remove_file(&local_output);
                                sink.log(&format!(
                                    "  Copied output to remote → {}",
                                    remote_dest.display()
                                ));
                            }
                            Err(e) => {
                                sink.log(&format!(
                                    "  ERROR: Could not copy output to remote mount: {e}"
                                ));
                                sink.log(&format!(
                                    "  Encoded file preserved at: {}",
                                    local_output.display()
                                ));
                            }
                        }
                    } else {
                        sink.log(&format!(
                            "  WARNING: expected output {} not found - remote folder not updated",
                            local_output.display()
                        ));
                    }
                }
            }

            // Restore original remote path
            queue[idx].full_path = original_path;
            // Clean up the staged input copy
            ctx.cleanup(sink);
        }

        if was_cancelled {
            break;
        }
    } // end 'outer wave loop

    // Clean up any wave staging contexts orphaned by `break 'outer`.
    // On normal loop exit these are already drained, so this is a no-op.
    for (idx, mut ctx, original_path) in wave_staging_contexts.drain(..) {
        queue[idx].full_path = original_path;
        ctx.cleanup(sink);
    }

    // Best-effort: remove the folder-mode output staging dir. Empty on the
    // success path (outputs are copied back and removed individually); may
    // hold a partial output after a mid-wave cancel. Covers both.
    if stage_folder_output {
        let _ = std::fs::remove_dir_all(
            crate::staging::resolve_staging_dir(None).join("folder-output"),
        );
    }

    // Batch completion
    let batch_duration = batch_start.elapsed();
    let dur_string = format!(
        "{:02}:{:02}:{:02}",
        batch_duration.as_secs() / 3600,
        (batch_duration.as_secs() % 3600) / 60,
        batch_duration.as_secs() % 60
    );

    let status_msg = if was_cancelled { "cancelled" } else { "done" };
    let summary = format!(
        "Batch {}. Done: {}, Failed: {}, Skipped: {}. Duration: {}",
        status_msg, done_count, fail_count, skip_count, dur_string
    );
    sink.log("");
    sink.log(&summary);
    write_log(&mut log_writer, "");
    write_log(&mut log_writer, &summary);

    // Flush and close the log file (#7)
    if let Some(ref mut w) = log_writer {
        use std::io::Write;
        let _ = w.flush();
    }
    // Log file path was already printed at batch start; no need to repeat here.

    // Post-command
    if let Some(ref cmd) = settings.post_command {
        if !cmd.is_empty() {
            sink.log(&format!("Running post-command: {}", cmd));
            #[cfg(unix)]
            {
                let _ = tokio::process::Command::new("sh")
                    .args(["-c", cmd])
                    .status()
                    .await;
            }
            #[cfg(windows)]
            {
                let mut proc = tokio::process::Command::new("cmd");
                proc.args(["/C", cmd]);
                ffbin::hide_window(&mut proc);
                let _ = proc.status().await;
            }
        }
    }

    sink.batch_finished(done_count, fail_count, skip_count, &dur_string);

    // ── Stderr log cleanup ──
    // Drop the shared handle so the file is fully flushed/closed.
    drop(batch_stderr_log);
    // Delete if empty, and enforce the 10-file cap.
    if save_log {
        cleanup_stderr_logs(output_dir, stderr_log_filename.as_deref(), 10);
    }

    (done_count, fail_count, skip_count, was_cancelled)
}

// ── Shared helpers ──────────────────────────────────────────────

/// Null device path for two-pass first-pass output.
#[cfg(windows)]
const NULL_DEVICE: &str = "NUL";
#[cfg(not(windows))]
const NULL_DEVICE: &str = "/dev/null";

/// Result from running an ffmpeg child with progress tracking.
struct FfmpegRunResult {
    exit_code: i32,
    was_cancelled_current: bool,
    was_cancelled_all: bool,
    frame_count: u64,
}

async fn validate_encoded_output(
    source: &crate::probe::ProbeResult,
    path: &str,
    tone_mapped: bool,
    sink: &dyn EventSink,
) -> Result<(), String> {
    let output = crate::probe::probe_file_for_validation(path, sink).await?;
    if tone_mapped {
        if output.is_hdr {
            return Err("HDR-to-SDR conversion still advertises HDR; output rejected".into());
        }
    } else if source.is_hdr != output.is_hdr {
        return Err("HDR/SDR classification changed; conversion rejected".into());
    }
    let tolerance = (2.0 / source.video_fps.max(1.0)).max(0.1);
    if !durations_match(
        source.duration_secs,
        output.duration_secs,
        source.start_time_secs,
        output.start_time_secs,
        tolerance,
    ) {
        return Err("Output duration changed; frame timing preservation failed".into());
    }
    source
        .video_format
        .verify(&output.video_format, tone_mapped)?;
    if (source.video_width, source.video_height) != (output.video_width, output.video_height) {
        return Err(format!(
            "Frame dimensions changed: {}x{} -> {}x{}",
            source.video_width, source.video_height, output.video_width, output.video_height
        ));
    }
    Ok(())
}

fn durations_match(
    source: f64,
    output: f64,
    source_start: f64,
    output_start: f64,
    tolerance: f64,
) -> bool {
    let difference = source - output;
    let rebase = source_start - output_start;
    // Matroska may report the end timestamp as duration. FFmpeg rebases small
    // initial codec delays when re-encoding. Permit that measured shift, while
    // bounding it so large timestamp offsets cannot hide truncated output.
    difference.abs() <= tolerance
        || (rebase.abs() <= 1.0 && (difference - rebase).abs() <= tolerance)
}

#[allow(clippy::too_many_arguments)]
async fn run_with_decode_retry(
    primary: &[String],
    software_decode: &[String],
    hardware_decode: bool,
    format: &crate::video_format::VideoFormat,
    duration: f64,
    tone_mapped: bool,
    ctx: &FfmpegRunContext<'_>,
) -> Result<FfmpegRunResult, String> {
    ctx.sink
        .processing_stage("encoding", "Starting video encoding...", None);
    let result = crate::video_format::decode_retry(hardware_decode, |hardware| {
        let args = if hardware { primary } else if hardware_decode { software_decode } else { primary };
        async move {
            let mut result = run_ffmpeg_with_progress(args,duration,None,ctx).await?;
            if result.exit_code == 0 && !result.was_cancelled_all && !result.was_cancelled_current {
                let output = crate::probe::probe_file_for_validation(args.last().unwrap(),ctx.sink).await?;
                if let Err(reason) = format.verify(&output.video_format,tone_mapped) {
                    ctx.sink.log(&format!("  Encoded output preservation check failed: {reason}"));
                    result.exit_code = 1;
                }
            }
            Ok(result)
        }
    }, |result: &Result<FfmpegRunResult,String>| {
        if ctx.batch_control.should_cancel_all() || ctx.batch_control.should_cancel_current() { return false; }
        match result {
        Ok(r) => r.exit_code != 0 && !r.was_cancelled_all && !r.was_cancelled_current,
        Err(error) => {
            ctx.sink.log(&format!("  Hardware decode attempt failed: {error}"));
            true
        },
    }}, || {
        ctx.sink.processing_stage("encoding", "GPU decoding unavailable; retrying with CPU decoding and the same GPU encoder...", None);
        ctx.sink.log("  Hardware decode/processing failed (see FFmpeg stderr); retrying with software decoding and preserved bit depth/color. The same verified hardware encoder and device are retained.");
        if let Some(path) = primary.last() { let _ = std::fs::remove_file(path); }
        let command = format!("ffmpeg {}",software_decode.join(" "));
        ctx.sink.batch_command(&command);
        ctx.sink.log(&format!("  CMD (decode fallback): {command}"));
    }).await;
    if result.is_err() {
        if let Some(path) = primary.last() {
            let _ = std::fs::remove_file(path);
        }
    }
    result
}

/// Build two-pass argument lists directly from components (#5).
/// Instead of cloning the base args and inserting at arbitrary positions,
/// this reconstructs the arg list with pass flags in the correct place.
///
/// For pass 1: strips the output path and replaces it with -f <fmt> NUL,
/// adds -an -sn (no audio/subs for analysis pass), and inserts -pass 1.
/// For pass 2: inserts -pass 2 and -passlogfile before the output path.
fn build_two_pass_args(
    base_args: &[String],
    pass_num: u8,
    passlog_prefix: &str,
    container_fmt: &str,
) -> Vec<String> {
    // The output path is always the last element in the base args
    let output_path = base_args.last().map(|s| s.as_str()).unwrap_or("");
    let args_without_output = &base_args[..base_args.len().saturating_sub(1)];

    let mut args: Vec<String> = Vec::with_capacity(base_args.len() + 8);
    args.extend(args_without_output.iter().cloned());

    // Insert pass flags before the output
    args.extend([
        "-pass".into(),
        pass_num.to_string(),
        "-passlogfile".into(),
        passlog_prefix.to_string(),
    ]);

    if pass_num == 1 {
        // Pass 1: analysis only - no audio/subs, output to null device
        args.extend([
            "-an".into(),
            "-sn".into(),
            "-f".into(),
            container_fmt.into(),
            NULL_DEVICE.into(),
        ]);
    } else {
        // Pass 2: full encode to the real output path
        args.push(output_path.to_string());
    }

    args
}

/// Context for a single `run_ffmpeg_with_progress` invocation, grouping
/// the fields that stay constant across passes within one file.
struct FfmpegRunContext<'a> {
    low_priority: bool,
    stderr_log: Option<SharedStderrLog>,
    stderr_label: &'a str,
    sink: &'a dyn EventSink,
    batch_control: &'a dyn BatchControl,
}

/// Spawn ffmpeg with the given args, stream stderr for progress, handle
/// pause/cancel, and return the result. Extracted to avoid duplicating the
/// poll loop for two-pass encoding and software fallback.
///
/// `ctx.stderr_log` is an optional shared batch log file. When provided, all
/// ffmpeg stderr output is appended there. `ctx.stderr_label` is written into
/// the separator header (typically the filename being encoded).
async fn run_ffmpeg_with_progress(
    args: &[String],
    file_duration: f64,
    pass: Option<(u8, u8)>,
    ctx: &FfmpegRunContext<'_>,
) -> Result<FfmpegRunResult, String> {
    let phase_start = std::time::Instant::now();
    let mut cmd = ffbin::ffmpeg_command();
    cmd.args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    // Set below-normal priority on the ffmpeg child process
    if ctx.low_priority {
        #[cfg(target_os = "windows")]
        {
            // BELOW_NORMAL_PRIORITY_CLASS | CREATE_NO_WINDOW
            const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x00004000;
            const CREATE_NO_WINDOW: u32 = 0x08000000;
            cmd.creation_flags(BELOW_NORMAL_PRIORITY_CLASS | CREATE_NO_WINDOW);
        }
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Failed to launch ffmpeg: {e}"))?;

    // On Unix, renice the child after spawn (avoids libc dependency)
    #[cfg(unix)]
    if ctx.low_priority {
        if let Some(pid) = child.id() {
            let _ = std::process::Command::new("renice")
                .args(["-n", "10", "-p", &pid.to_string()])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
    }

    ctx.sink
        .log(&format!("  ffmpeg PID: {}", child.id().unwrap_or(0)));

    // Stream stderr for progress using a blocking thread, with log capture
    let progress = FfmpegProgress::new();
    let stderr_thread = child.stderr.take().map(|stderr| {
        spawn_stderr_reader(stderr, &progress, ctx.stderr_log.clone(), ctx.stderr_label)
    });

    // Poll for cancellation/pause while waiting for ffmpeg, and emit progress
    let mut last_progress_emit = std::time::Instant::now() - std::time::Duration::from_millis(500);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {}
            Err(_) => break,
        }
        if ctx.batch_control.should_cancel_current() || ctx.batch_control.should_cancel_all() {
            if let Some(mut stdin) = child.stdin.take() {
                use tokio::io::AsyncWriteExt;
                let _ = stdin.write_all(b"q").await;
                let _ = stdin.flush().await;
            }
            let graceful =
                tokio::time::timeout(std::time::Duration::from_secs(5), child.wait()).await;
            if graceful.is_err() {
                let _ = child.kill().await;
            }
            break;
        }

        while ctx.batch_control.is_paused() {
            if ctx.batch_control.should_cancel_current() || ctx.batch_control.should_cancel_all() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }

        if file_duration > 0.0 {
            let now = std::time::Instant::now();
            if now.duration_since(last_progress_emit).as_millis() >= 250 {
                let secs = progress.secs();
                if secs > 0.0 {
                    let pct = (secs / file_duration * 100.0).min(100.0);
                    ctx.sink.file_progress(pct, secs, file_duration, pass);
                    last_progress_emit = now;
                }
            }
        }

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }

    let exit_status = child.wait().await;

    // Join stderr thread and emit final progress
    if let Some(handle) = stderr_thread {
        let _ = handle.join();
    }
    if file_duration > 0.0 {
        ctx.sink
            .file_progress(100.0, file_duration, file_duration, pass);
    }

    let exit_code = exit_status.map(|s| s.code().unwrap_or(-1)).unwrap_or(-1);
    let final_frames = progress.frames();

    ctx.sink.log(&format!(
        "  PERF FFmpeg video encoding: {:.3}s",
        phase_start.elapsed().as_secs_f64()
    ));
    Ok(FfmpegRunResult {
        exit_code,
        was_cancelled_current: ctx.batch_control.should_cancel_current(),
        was_cancelled_all: ctx.batch_control.should_cancel_all(),
        frame_count: final_frames,
    })
}

// ── Precision mode helpers ─────────────────────────────────────

/// Detect total system RAM in gigabytes (approximate, zero-dependency).
/// Result is cached after the first call to avoid spawning subprocesses
/// (macOS uses `sysctl`) on every invocation.
pub fn get_system_ram_gb() -> u64 {
    static CACHE: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *CACHE.get_or_init(get_system_ram_gb_inner)
}

fn get_system_ram_gb_inner() -> u64 {
    #[cfg(target_os = "windows")]
    {
        use std::mem;
        #[repr(C)]
        struct MemoryStatusEx {
            dw_length: u32,
            dw_memory_load: u32,
            ull_total_phys: u64,
            ull_avail_phys: u64,
            ull_total_page_file: u64,
            ull_avail_page_file: u64,
            ull_total_virtual: u64,
            ull_avail_virtual: u64,
            ull_avail_extended_virtual: u64,
        }
        extern "system" {
            fn GlobalMemoryStatusEx(lp_buffer: *mut MemoryStatusEx) -> i32;
        }
        // SAFETY: `status` is zero-initialised with the correct `dw_length`,
        // and `GlobalMemoryStatusEx` only writes within that fixed-size struct.
        // Called on the main thread with no concurrent mutation of `status`.
        unsafe {
            let mut status: MemoryStatusEx = mem::zeroed();
            status.dw_length = mem::size_of::<MemoryStatusEx>() as u32;
            if GlobalMemoryStatusEx(&mut status) != 0 {
                return status.ull_total_phys / (1024 * 1024 * 1024);
            }
        }
        0
    }
    #[cfg(target_os = "linux")]
    {
        if let Ok(contents) = std::fs::read_to_string("/proc/meminfo") {
            for line in contents.lines() {
                if line.starts_with("MemTotal:") {
                    let kb: u64 = line
                        .split_whitespace()
                        .nth(1)
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0);
                    return kb / (1024 * 1024);
                }
            }
        }
        0
    }
    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("sysctl")
            .args(["-n", "hw.memsize"])
            .output();
        if let Ok(o) = output {
            let s = String::from_utf8_lossy(&o.stdout);
            let bytes: u64 = s.trim().parse().unwrap_or(0);
            return bytes / (1024 * 1024 * 1024);
        }
        0
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
    {
        0
    }
}

/// Determine the optimal rc-lookahead value based on available system RAM (#3).
/// Accepts a pre-cached RAM value to avoid re-querying the OS.
/// Returns 0 to leave ffmpeg's default, or a value up to 250.
/// Only meaningful for single-pass CRF - two-pass already has full-file data.
pub fn lookahead_for_ram_with_cache(ram_gb: u64) -> u32 {
    if ram_gb >= 16 {
        250
    } else if ram_gb >= 8 {
        120
    } else {
        0 // low RAM - precision mode uses two-pass instead of lookahead
    }
}

/// Returns true if the system has insufficient RAM for extended lookahead (#3).
/// Accepts a pre-cached RAM value.
pub fn precision_needs_two_pass_with_ram(ram_gb: u64) -> bool {
    ram_gb < 8
}

/// Configuration for a CRF viability probe.
pub struct CrfProbeConfig<'a> {
    pub input_path: &'a str,
    pub duration_secs: f64,
    pub video_args: &'a [String],
    pub pix_fmt: &'a str,
    pub threads: u32,
    pub low_priority: bool,
    pub stderr_log: Option<SharedStderrLog>,
}

/// Probe CRF viability by encoding three 10-second samples from 25%, 50%,
/// and 75% through the file and returning the average output bitrate in Mbps.
///
/// The sink receives progress updates as "Probing 1/3", "Probing 2/3", etc.
/// via batch_status. Returns None if the file is too short to sample (<120s)
/// or if any sample encode fails.
pub async fn probe_crf_viability(
    config: &CrfProbeConfig<'_>,
    sink: &dyn EventSink,
    batch_control: &dyn BatchControl,
) -> Option<f64> {
    if config.duration_secs < MIN_PROBE_DURATION_SECS {
        sink.log("  Precision probe: file too short, skipping viability check");
        return None;
    }

    let sample_duration = PROBE_SAMPLE_DURATION_SECS;
    let seek_points = PROBE_SEEK_POINTS;
    let mut total_bits: f64 = 0.0;
    let mut total_sample_secs: f64 = 0.0;

    for (i, &fraction) in seek_points.iter().enumerate() {
        if batch_control.should_cancel_current() || batch_control.should_cancel_all() {
            return None;
        }

        let seek_secs = config.duration_secs * fraction;
        let sample_num = i + 1;
        sink.batch_status(&format!(
            "Probing CRF viability ({}/{})",
            sample_num,
            seek_points.len()
        ));
        sink.file_progress(
            (sample_num as f64 - 1.0) / seek_points.len() as f64 * 100.0,
            0.0,
            0.0,
            Some((sample_num as u8, seek_points.len() as u8)),
        );

        // Build sample encode args: seek, duration limit, video only, temp output.
        // NamedTempFile auto-deletes on drop; we keep it alive for stat then let it drop.
        let tmp_file = match tempfile::Builder::new()
            .prefix("histv_crf_probe_")
            .suffix(".mkv")
            .tempfile()
        {
            Ok(f) => f,
            Err(e) => {
                sink.log(&format!("  Precision probe: temp file error: {e}"));
                return None;
            }
        };
        let tmp_str = tmp_file.path().to_string_lossy().to_string();

        let mut args: Vec<String> = vec![
            "-y".into(),
            "-ss".into(),
            format!("{:.2}", seek_secs),
            "-t".into(),
            format!("{:.2}", sample_duration),
            "-i".into(),
            config.input_path.to_string(),
            "-map".into(),
            "0:v:0".into(),
            "-an".into(),
            "-sn".into(),
        ];
        if config.threads > 0 {
            args.extend(vec!["-threads".into(), config.threads.to_string()]);
        }
        args.extend(config.video_args.iter().cloned());
        args.extend(vec!["-pix_fmt".into(), config.pix_fmt.to_string()]);
        args.push(tmp_str.clone());

        let probe_label = format!("probe_{}", sample_num);
        let probe_ctx = FfmpegRunContext {
            low_priority: config.low_priority,
            stderr_log: config.stderr_log.clone(),
            stderr_label: &probe_label,
            sink,
            batch_control,
        };
        let result = run_ffmpeg_with_progress(
            &args,
            sample_duration,
            Some((sample_num as u8, seek_points.len() as u8)),
            &probe_ctx,
        )
        .await;

        match result {
            Ok(r) if r.was_cancelled_current || r.was_cancelled_all => {
                return None;
            }
            Ok(r) if r.exit_code != 0 => {
                sink.log(&format!(
                    "  Precision probe: sample {} failed (exit code {})",
                    sample_num, r.exit_code
                ));
                return None;
            }
            Err(e) => {
                sink.log(&format!(
                    "  Precision probe: sample {} error: {}",
                    sample_num, e
                ));
                return None;
            }
            _ => {}
        }

        // Stat the sample output to get its size (file auto-cleaned on drop)
        let sample_size = std::fs::metadata(tmp_file.path())
            .map(|m| m.len())
            .unwrap_or(0);

        if sample_size == 0 {
            sink.log(&format!(
                "  Precision probe: sample {} produced empty output",
                sample_num
            ));
            return None;
        }

        total_bits += sample_size as f64 * 8.0;
        total_sample_secs += sample_duration;
    }

    sink.file_progress(100.0, 0.0, 0.0, None);

    if total_sample_secs > 0.0 {
        let avg_bps = total_bits / total_sample_secs;
        let avg_mbps = avg_bps / 1_000_000.0;
        sink.log(&format!(
            "  Precision probe: estimated CRF bitrate {:.2}Mbps",
            avg_mbps
        ));
        Some(avg_mbps)
    } else {
        None
    }
}

/// Component parts for assembling an ffmpeg command line.
struct FfmpegAssemblyParts<'a> {
    input_path: &'a str,
    output_path: &'a str,
    video_args: &'a [&'a str],
    render_device: Option<&'a str>,
    hardware_decode: bool,
    source_format: Option<&'a crate::video_format::VideoFormat>,
    pix_fmt: &'a str,
    audio_map_args: &'a [String],
    audio_codec_args: &'a [String],
    is_image_source: bool,
    threads: u32,
    tonemap_filter: Option<&'a str>,
    codec_family: &'a str,
    /// Explicit colour metadata for VAAPI hwaccel decode which does not
    /// propagate colour primaries/transfer/matrix from input. Only set
    /// for the VAAPI hwaccel decode path; other paths propagate
    /// automatically and explicit flags cause haloing on PQ content.
    colour_metadata: Option<ColourMetadata<'a>>,
    /// HEVC bitstream filter for VAAPI 10-bit encode. AMD VCN pads 1080-line
    /// content to 1088 lines without setting a conformance window crop in the
    /// SPS, producing a visible green bar at the bottom. The hevc_metadata BSF
    /// with crop_bottom=8 patches the SPS to declare the correct crop.
    /// Only set for hevc_vaapi on Linux; other encoders don't have this issue.
    vaapi_hevc_bsf: Option<&'a str>,
}

/// Colour metadata to embed in the ffmpeg output. Only needed for
/// VAAPI hwaccel decode, which does not propagate colour information
/// from the input. For all other paths, ffmpeg propagates automatically
/// and explicit flags cause haloing on PQ content.
#[derive(Clone, Copy)]
struct ColourMetadata<'a> {
    primaries: &'a str,
    trc: &'a str,
    matrix: &'a str,
    range: &'a str,
}

/// Assemble the full ffmpeg argument list from its component parts.
/// Used by both the primary encode and the software-fallback path
/// to avoid duplicating the invocation pattern.
fn assemble_ffmpeg_args(parts: &FfmpegAssemblyParts<'_>) -> Vec<String> {
    // A tonemap filter requires an encode — ffmpeg cannot apply `-vf`
    // when the video stream is being copied (`-c:v copy -vf ...` fails
    // with EINVAL, exit 234). The caller must rewrite the decision to
    // a transcode before assembling. Guarded with debug_assert so
    // future call sites can't reintroduce the contradiction silently.
    debug_assert!(
        !(parts.tonemap_filter.is_some()
            && parts
                .video_args
                .windows(2)
                .any(|w| w[0] == "-c:v" && w[1] == "copy")),
        "assemble_ffmpeg_args: tonemap_filter is incompatible with -c:v copy"
    );

    // Detect encoder type from video_args for hardware device init.
    // video_args layout: ["-c:v", "<encoder_name>", ...]
    let encoder_name = parts
        .video_args
        .windows(2)
        .find(|w| w[0] == "-c:v")
        .map(|w| w[1]);

    let mut hw_device_init: Vec<String> = Vec::new();
    let mut hw_upload_filter: Option<String> = None;
    // When true, keep decoded frames on the verified VAAPI encoding device.
    let mut vaapi_hwaccel_decode = false;

    if let Some(enc) = encoder_name {
        #[cfg(target_os = "linux")]
        if is_vaapi_encoder(enc) {
            if let Some(render_node) = parts.render_device {
                // Opt-in GPU decoding keeps NV12/P010 frames on the verified
                // device. CPU filters use software decoding and an explicit
                // upload; compressed metadata processing is independent.
                if parts.hardware_decode && parts.tonemap_filter.is_none() {
                    hw_device_init = vec![
                        "-hwaccel".into(),
                        "vaapi".into(),
                        "-hwaccel_device".into(),
                        render_node.into(),
                        "-hwaccel_output_format".into(),
                        "vaapi".into(),
                    ];
                    vaapi_hwaccel_decode = true;
                    // AMD VAAPI requires an explicit scale_vaapi filter to
                    // force the P010 surface format between decoder and
                    // encoder. Without it, the encoder may receive surfaces
                    // in an unexpected format and produce green/corrupt
                    // frames despite the container reporting Main 10.
                    hw_upload_filter = Some(
                        if crate::video_format::pixel_depth(parts.pix_fmt) == 10 {
                            "scale_vaapi=format=p010"
                        } else {
                            "scale_vaapi=format=nv12"
                        }
                        .into(),
                    );
                } else {
                    hw_device_init = vec!["-vaapi_device".into(), render_node.into()];
                    hw_upload_filter = Some(
                        if crate::video_format::pixel_depth(parts.pix_fmt) == 10 {
                            "format=p010le,hwupload"
                        } else {
                            "format=nv12,hwupload"
                        }
                        .into(),
                    );
                }
            } else {
                // An unverified forced encoder must fail explicitly so the
                // existing software fallback can handle it. Never silently
                // initialize a default GPU different from the tested one.
                hw_device_init = vec!["-vaapi_device".into(), "/dev/dri/renderD_missing".into()];
            }
        }
        #[cfg(target_os = "linux")]
        if is_qsv_encoder(enc) {
            // Use QSV's own verified device, independently of VAAPI selection.
            if let Some(render_node) = parts.render_device {
                hw_device_init = qsv_device_args(render_node);
            } else {
                hw_device_init = qsv_device_args("/dev/dri/renderD_missing");
            }
            let upload_fmt = if parts.pix_fmt == "p010le" {
                "format=p010le,hwupload=extra_hw_frames=64"
            } else {
                "format=nv12,hwupload=extra_hw_frames=64"
            };
            hw_upload_filter = Some(upload_fmt.into());
        }
    }

    if parts.tonemap_filter.is_none() && encoder_name != Some("copy") {
        if let Some(color_filter) = parts.source_format.and_then(|f| f.frame_color_filter()) {
            hw_upload_filter = Some(match hw_upload_filter {
                Some(filter) if vaapi_hwaccel_decode => format!("{filter},{color_filter}"),
                Some(filter) => format!("{color_filter},{filter}"),
                None => color_filter,
            });
        }
    }
    let mut args: Vec<String> = vec![
        "-err_detect".into(),
        "ignore_err".into(),
        "-probesize".into(),
        "100M".into(),
        "-analyzeduration".into(),
        "100M".into(),
        "-y".into(),
    ];

    // Hardware device init must come before -i
    args.extend(hw_device_init);

    args.extend(vec![
        "-i".into(),
        parts.input_path.to_string(),
        "-map".into(),
        "0:v:0".into(),
    ]);

    // Thread limit (0 = ffmpeg default / auto)
    if parts.threads > 0 {
        args.extend(vec!["-threads".into(), parts.threads.to_string()]);
    }

    if parts.is_image_source {
        // GIF/APNG: no audio or subtitle streams, pad odd dimensions,
        // force yuv420p (GIF decodes to rgb24/rgba).
        args.extend(vec!["-an".into(), "-sn".into()]);
        args.extend(parts.video_args.iter().map(|s| s.to_string()));
        args.extend(vec![
            "-vf".into(),
            "pad=ceil(iw/2)*2:ceil(ih/2)*2".into(),
            "-pix_fmt".into(),
            "yuv420p".into(),
        ]);
    } else {
        // Normal video: per-stream audio maps + subtitle passthrough
        args.extend_from_slice(parts.audio_map_args);
        args.extend(parts.video_args.iter().map(|s| s.to_string()));
        // Build the video filter chain. For VAAPI/QSV encoders the frames
        // must be uploaded to the hardware device; chain hwupload after any
        // tonemap filter so the tonemap runs on CPU and hwupload moves the
        // result to the GPU.
        //
        // When VAAPI hwaccel decode is used (10-bit HDR/DV without tonemap),
        // frames are already on the GPU as VAAPI surfaces. An explicit
        // scale_vaapi filter forces P010 surface format between decoder and
        // encoder (AMD VAAPI produces green frames without it). -pix_fmt must
        // not be set (the encoder auto-negotiates from the VAAPI surface).
        if let Some(filter) = parts.tonemap_filter {
            if let Some(hw_filter) = &hw_upload_filter {
                args.extend(vec!["-vf".into(), format!("{filter},{hw_filter}")]);
            } else {
                args.extend(vec!["-vf".into(), filter.to_string()]);
            }
        } else if let Some(hw_filter) = &hw_upload_filter {
            args.extend(vec!["-vf".into(), hw_filter.clone()]);
        }
        if !vaapi_hwaccel_decode
            && !encoder_name.is_some_and(|e| is_vaapi_encoder(e) || is_qsv_encoder(e))
        {
            // When using hwaccel decode, the encoder receives frames in
            // VAAPI surface format and auto-negotiates; setting -pix_fmt
            // would force an incorrect software→hw format conversion.
            args.extend(vec!["-pix_fmt".into(), parts.pix_fmt.to_string()]);
        }
        args.extend_from_slice(parts.audio_codec_args);
        // Subtitle handling: MP4 only supports mov_text (timed text).
        // Using `-c:s mov_text` converts text-based subs (SRT, ASS) and
        // causes ffmpeg to silently skip bitmap subs (PGS, DVB) that
        // cannot be converted. MKV supports all subtitle codecs as-is.
        if parts.output_path.ends_with(".mp4") {
            args.extend(vec![
                "-map".into(),
                "0:s?".into(),
                "-c:s".into(),
                "mov_text".into(),
            ]);
        } else {
            args.extend(vec![
                "-map".into(),
                "0:s?".into(),
                "-c:s".into(),
                "copy".into(),
                "-disposition:s:0".into(),
                "default".into(),
            ]);
        }
    }

    // MP4-specific muxing flags
    if parts.output_path.ends_with(".mp4") {
        args.extend(vec!["-movflags".into(), "+faststart".into()]);
        // VAAPI can update picture parameter sets during rate control. hvc1
        // strips in-band parameter sets, leaving stale headers and corrupt
        // pictures despite a successful encode and valid HDR flags. Keep hev1
        // for this encoder to retain the headers required by every picture.
        if parts.codec_family == "hevc" {
            // Stream copies must retain parameter sets too, including files
            // previously produced by VAAPI. Re-tagging them hvc1 can corrupt
            // otherwise clean input without re-encoding a single picture.
            let tag = if matches!(encoder_name, Some("hevc_vaapi" | "copy")) {
                "hev1"
            } else {
                "hvc1"
            };
            args.extend(vec!["-tag:v".into(), tag.into()]);
        }
    }

    // Signal the source tags without inventing defaults or changing pixel
    // values. Hardware frame negotiation can otherwise lose these tags.
    if let Some(cm) = &parts.colour_metadata {
        for (flag, value) in [
            ("-color_primaries", cm.primaries),
            ("-color_trc", cm.trc),
            ("-colorspace", cm.matrix),
            ("-color_range", cm.range),
        ] {
            if crate::video_format::known(value) {
                args.extend([flag.into(), value.into()]);
            }
        }
    }
    if encoder_name == Some("libx265") && parts.tonemap_filter.is_none() {
        if let Some(format) = parts.source_format {
            let params = format.x265_hdr_params();
            if !params.is_empty() {
                args.extend(["-x265-params".into(), params]);
            }
        }
    }

    // VAAPI 10-bit HEVC: patch the conformance window in the SPS to crop
    // the 8-pixel green bar that AMD VCN produces at the bottom of 1080-line
    // content (it encodes to 1088 lines without declaring a crop window).
    if let Some(bsf) = parts.vaapi_hevc_bsf {
        args.extend(vec!["-bsf:v".into(), bsf.into()]);
    }

    if parts.output_path.to_ascii_lowercase().ends_with(".mp4") {
        // FFmpeg otherwise silently omits Dolby Vision configuration boxes
        // even when the input stream has valid DOVI side data.
        args.extend(["-strict".into(), "unofficial".into()]);
    }
    args.push(parts.output_path.to_string());
    args
}

/// Returns true if ffmpeg has an encoder for the given audio codec name.
/// Codecs like DTS and TrueHD are decode-only in ffmpeg.
fn has_ffmpeg_encoder(audio_codec: &str) -> bool {
    matches!(
        audio_codec,
        "ac3"
            | "eac3"
            | "aac"
            | "opus"
            | "vorbis"
            | "flac"
            | "mp3"
            | "mp2"
            | "pcm_s16le"
            | "pcm_s24le"
            | "pcm_s32le"
            | "pcm_f32le"
            | "pcm_f64le"
            | "pcm_s16be"
            | "pcm_s24be"
            | "pcm_s32be"
            | "pcm_mulaw"
            | "pcm_alaw"
    )
}

/// Map a codec name to its ffmpeg encoder name and optional max bitrate (kbps).
/// Returns (encoder_name, max_kbps) where max_kbps is None for unlimited.
fn ffmpeg_encoder_for_codec(codec: &str) -> (String, Option<u32>) {
    match codec {
        "opus" => ("libopus".to_string(), Some(512)),
        "vorbis" => ("libvorbis".to_string(), Some(500)),
        other => (other.to_string(), None),
    }
}

/// Returns true if the given audio codec can be muxed into an MP4 container.
fn is_mp4_compatible_audio(codec: &str) -> bool {
    matches!(
        codec,
        "aac" | "ac3" | "eac3" | "mp3" | "mp2" | "alac" | "pcm_s16le" | "pcm_s24le"
    )
}

/// Build per-stream audio arguments from pre-probed audio stream data (§6.2).
/// Uses AudioStrategy to determine copy/re-encode behaviour per stream.
/// `container_ext` is used to prevent MP4-incompatible codecs from being
/// copied or re-encoded into their original format (e.g. Opus in MP4).
fn build_audio_args_from_probe(
    audio_streams: &[crate::queue::AudioStreamInfo],
    strategy: &AudioStrategy,
    container_ext: &str,
    sink: &dyn EventSink,
) -> (Vec<String>, Vec<String>) {
    if audio_streams.is_empty() {
        sink.log("  WARNING: No audio streams found");
        return (Vec::new(), Vec::new());
    }

    let mut map_args = Vec::new();
    let mut codec_args = Vec::new();
    let mut output_idx: u32 = 0;

    for stream in audio_streams {
        // Unknown codecs can't be decoded or muxed - skip entirely.
        // Do NOT increment output_idx for skipped streams.
        if stream.codec == "unknown" {
            sink.log(&format!(
                "  WARNING: Audio {} has an unrecognised codec and will be excluded from the output",
                stream.index
            ));
            continue;
        }

        map_args.extend(["-map".into(), format!("0:a:{}", stream.index)]);

        // Check if copying this codec into the output container would be valid.
        let is_mp4 = container_ext == "mp4";
        let copy_safe = !is_mp4 || is_mp4_compatible_audio(&stream.codec);

        match strategy {
            AudioStrategy::CopyAll => {
                if !copy_safe {
                    sink.log(&format!("  WARNING: Audio {} ({}) cannot be copied to MP4. Choose MKV; FFmpeg will report the container incompatibility.", stream.index, stream.codec));
                }
                codec_args.extend([format!("-c:a:{output_idx}"), "copy".into()]);
                sink.log(&format!(
                    "  Audio {} : {} @ {}kbps - copying without bitrate conversion",
                    stream.index, stream.codec, stream.bitrate_kbps
                ));
            }
            AudioStrategy::CopyCapped { cap_kbps } => {
                if stream.bitrate_kbps <= *cap_kbps && copy_safe {
                    codec_args.extend([format!("-c:a:{output_idx}"), "copy".into()]);
                    sink.log(&format!(
                        "  Audio {} : {} @ {}kbps - copying",
                        stream.index, stream.codec, stream.bitrate_kbps
                    ));
                } else {
                    let (target_codec, target_br) =
                        if copy_safe && has_ffmpeg_encoder(&stream.codec) {
                            let (enc_name, max_kbps) = ffmpeg_encoder_for_codec(&stream.codec);
                            let br = match max_kbps {
                                Some(max) if *cap_kbps > max => max,
                                _ => *cap_kbps,
                            };
                            (enc_name, br)
                        } else {
                            // Codec is either MP4-incompatible or has no ffmpeg encoder.
                            let fallback = "eac3";
                            if !copy_safe {
                                sink.log(&format!(
                                    "  Audio {} : {} is not supported in MP4, falling back to EAC3",
                                    stream.index, stream.codec
                                ));
                            } else {
                                sink.log(&format!(
                                    "  Audio {} : {} has no ffmpeg encoder, falling back to EAC3",
                                    stream.index, stream.codec
                                ));
                            }
                            (fallback.to_string(), *cap_kbps)
                        };
                    codec_args.extend([
                        format!("-c:a:{output_idx}"),
                        target_codec.clone(),
                        format!("-b:a:{output_idx}"),
                        format!("{target_br}k"),
                    ]);
                    sink.log(&format!(
                        "  Audio {} : {} @ {}kbps - encoding to {} @ {}kbps",
                        stream.index, stream.codec, stream.bitrate_kbps, target_codec, target_br
                    ));
                }
            }
            AudioStrategy::CompatCapped { cap_kbps } => {
                if stream.codec == "aac" && stream.bitrate_kbps <= *cap_kbps {
                    codec_args.extend([format!("-c:a:{output_idx}"), "copy".into()]);
                    sink.log(&format!(
                        "  Audio {} : AAC @ {}kbps - copying",
                        stream.index, stream.bitrate_kbps
                    ));
                } else {
                    let target_br = if stream.bitrate_kbps == 0 {
                        *cap_kbps
                    } else {
                        stream.bitrate_kbps.min(*cap_kbps)
                    };
                    codec_args.extend([
                        format!("-c:a:{output_idx}"),
                        "aac".to_string(),
                        format!("-b:a:{output_idx}"),
                        format!("{target_br}k"),
                    ]);
                    sink.log(&format!(
                        "  Audio {} : {} @ {}kbps - encoding to AAC @ {}kbps",
                        stream.index, stream.codec, stream.bitrate_kbps, target_br
                    ));
                }
            }
        }

        // Only increment after a stream is actually mapped (not skipped)
        output_idx += 1;
    }

    (map_args, codec_args)
}

/// Execute a post-batch action (§7.6).
pub async fn execute_post_action(action: &str) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    let linux_user = std::env::var("USER").unwrap_or_default();

    let (cmd, args): (&str, Vec<&str>) = match action {
        #[cfg(target_os = "windows")]
        "Shutdown" => ("shutdown", vec!["/s", "/t", "0"]),
        #[cfg(target_os = "windows")]
        "Sleep" => ("rundll32", vec!["powrprof.dll,SetSuspendState", "0,1,0"]),
        #[cfg(target_os = "windows")]
        "Log Out" => ("shutdown", vec!["/l"]),

        #[cfg(target_os = "macos")]
        "Shutdown" => (
            "osascript",
            vec!["-e", "tell app \"System Events\" to shut down"],
        ),
        #[cfg(target_os = "macos")]
        "Sleep" => ("pmset", vec!["sleepnow"]),
        #[cfg(target_os = "macos")]
        "Log Out" => (
            "osascript",
            vec!["-e", "tell app \"System Events\" to log out"],
        ),

        #[cfg(target_os = "linux")]
        "Shutdown" => ("systemctl", vec!["poweroff"]),
        #[cfg(target_os = "linux")]
        "Sleep" => ("systemctl", vec!["suspend"]),
        #[cfg(target_os = "linux")]
        "Log Out" => ("loginctl", vec!["terminate-user", &linux_user]),

        _ => return Ok(()),
    };

    let mut proc = Command::new(cmd);
    proc.args(&args);
    ffbin::hide_window(&mut proc);
    proc.spawn()
        .map_err(|e| format!("Failed to execute {}: {}", action, e))?;

    Ok(())
}

// ── Shared ffmpeg stderr progress parser ──────────────────────

/// Atomic counters updated by the stderr reader thread.
/// `progress_secs` stores the bits of an f64 (via `f64::to_bits`).
/// `frame_count` stores the raw frame count.
pub struct FfmpegProgress {
    pub progress_secs: Arc<std::sync::atomic::AtomicU64>,
    pub frame_count: Arc<std::sync::atomic::AtomicU64>,
}

impl Default for FfmpegProgress {
    fn default() -> Self {
        Self::new()
    }
}

impl FfmpegProgress {
    pub fn new() -> Self {
        Self {
            progress_secs: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            frame_count: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    /// Read the current progress as seconds.
    pub fn secs(&self) -> f64 {
        f64::from_bits(
            self.progress_secs
                .load(std::sync::atomic::Ordering::Relaxed),
        )
    }

    /// Read the current frame count.
    pub fn frames(&self) -> u64 {
        self.frame_count.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Convert a tokio async stderr to a std blocking stderr.
fn tokio_stderr_to_std(
    tokio_stderr: tokio::process::ChildStderr,
) -> Result<std::process::ChildStderr, String> {
    #[cfg(windows)]
    {
        let owned = tokio_stderr
            .into_owned_handle()
            .map_err(|e| format!("Failed to take stderr handle: {e}"))?;
        Ok(std::process::ChildStderr::from(owned))
    }
    #[cfg(unix)]
    {
        let owned = tokio_stderr
            .into_owned_fd()
            .map_err(|e| format!("Failed to take stderr fd: {e}"))?;
        Ok(std::process::ChildStderr::from(owned))
    }
}

/// Shared type for the per-batch stderr log that multiple stderr reader
/// threads can write to concurrently.
pub type SharedStderrLog = Arc<std::sync::Mutex<std::io::BufWriter<std::fs::File>>>;

/// Open (or create) a single timestamped ffmpeg stderr log file in
/// `output_dir/ffmpeg_logs/`. Intended to be called once per batch and
/// shared across all ffmpeg invocations in that batch via `Arc<Mutex<…>>`.
///
/// Returns `None` if the directory can't be created or the file can't be
/// opened.
pub fn open_stderr_log(output_dir: &std::path::Path) -> Option<SharedStderrLog> {
    let log_dir = output_dir.join("ffmpeg_logs");
    let _ = std::fs::create_dir_all(&log_dir);
    let filename = format!(
        "ffmpeg_stderr_{}.log",
        chrono::Local::now().format("%Y%m%d_%H%M%S")
    );
    let file = std::fs::File::create(log_dir.join(filename)).ok()?;
    Some(Arc::new(std::sync::Mutex::new(std::io::BufWriter::new(
        file,
    ))))
}

/// Clean up the `ffmpeg_logs` directory under `output_dir`:
///   1. Delete the file at `log_path` if it is empty (no stderr was captured).
///   2. Keep at most `max_logs` log files — oldest are deleted first.
pub fn cleanup_stderr_logs(
    output_dir: &std::path::Path,
    log_path: Option<&std::path::Path>,
    max_logs: usize,
) {
    // Remove the file if it is empty
    if let Some(p) = log_path {
        if let Ok(meta) = std::fs::metadata(p) {
            if meta.len() == 0 {
                let _ = std::fs::remove_file(p);
            }
        }
    }

    let log_dir = output_dir.join("ffmpeg_logs");
    if !log_dir.is_dir() {
        return;
    }

    // Collect .log files sorted oldest-first by modified time
    let mut entries: Vec<(std::time::SystemTime, std::path::PathBuf)> = std::fs::read_dir(&log_dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .map(|ext| ext == "log")
                .unwrap_or(false)
        })
        .filter_map(|e| {
            let modified = e.metadata().ok()?.modified().ok()?;
            Some((modified, e.path()))
        })
        .collect();
    entries.sort_by_key(|(t, _)| *t);

    // Delete oldest until at most max_logs remain
    while entries.len() > max_logs {
        let (_, path) = entries.remove(0);
        let _ = std::fs::remove_file(path);
    }
}

/// Spawn a blocking thread that reads ffmpeg stderr, updates the
/// progress counters, and appends all output to the shared batch log
/// file. A separator header is written before the first byte so that
/// each invocation's output is distinguishable in the combined log.
///
/// `label` is included in the separator (typically the filename being
/// encoded).
pub fn spawn_stderr_reader(
    tokio_stderr: tokio::process::ChildStderr,
    progress: &FfmpegProgress,
    stderr_log: Option<SharedStderrLog>,
    label: &str,
) -> std::thread::JoinHandle<()> {
    let std_stderr = match tokio_stderr_to_std(tokio_stderr) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("WARNING: Could not convert stderr for {label}: {e}");
            return std::thread::spawn(|| {});
        }
    };
    let progress_secs = Arc::clone(&progress.progress_secs);
    let frames = Arc::clone(&progress.frame_count);
    let label = label.to_owned();

    std::thread::spawn(move || {
        use std::io::{Read, Write};
        let mut reader = std::io::BufReader::new(std_stderr);
        let mut buf = [0u8; 4096];
        let mut line_buf = Vec::<u8>::with_capacity(256);

        // Write separator header into shared log
        if let Some(ref log) = stderr_log {
            if let Ok(mut w) = log.lock() {
                let _ = writeln!(
                    w,
                    "\n=== ffmpeg stderr [{}] [{}] ===",
                    label,
                    chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
                );
            }
        }

        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    // Write raw bytes to shared log file
                    if let Some(ref log) = stderr_log {
                        if let Ok(mut w) = log.lock() {
                            let _ = w.write_all(&buf[..n]);
                        }
                    }
                    for &byte in &buf[..n] {
                        if byte == b'\r' || byte == b'\n' {
                            if !line_buf.is_empty() {
                                if let Some(pos) = line_buf.windows(5).position(|w| w == b"time=") {
                                    let start = pos + 5;
                                    let end = line_buf[start..]
                                        .iter()
                                        .position(|&b| b == b' ' || b == b'\t')
                                        .map(|p| start + p)
                                        .unwrap_or(line_buf.len());
                                    if let Ok(s) = std::str::from_utf8(&line_buf[start..end]) {
                                        if let Some(secs) = parse_ffmpeg_time(s) {
                                            progress_secs.store(
                                                secs.to_bits(),
                                                std::sync::atomic::Ordering::Relaxed,
                                            );
                                        }
                                    }
                                }
                                if let Some(pos) = line_buf.windows(6).position(|w| w == b"frame=")
                                {
                                    let start = pos + 6;
                                    let end = line_buf[start..]
                                        .iter()
                                        .position(|&b| b == b' ' || b == b'\t')
                                        .map(|p| start + p)
                                        .unwrap_or(line_buf.len());
                                    if let Ok(s) = std::str::from_utf8(&line_buf[start..end]) {
                                        if let Ok(fc) = s.trim().parse::<u64>() {
                                            frames.store(fc, std::sync::atomic::Ordering::Relaxed);
                                        }
                                    }
                                }
                                line_buf.clear();
                            }
                        } else {
                            line_buf.push(byte);
                        }
                    }
                }
                Err(_) => break,
            }
        }
        // Flush on exit
        if let Some(ref log) = stderr_log {
            if let Ok(mut w) = log.lock() {
                let _ = w.flush();
            }
        }
    })
}

/// Parse ffmpeg's time= format "HH:MM:SS.ms" or "HH:MM:SS" into seconds.
pub fn parse_ffmpeg_time(s: &str) -> Option<f64> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 3 {
        return None;
    }
    let h: f64 = parts[0].parse().ok()?;
    let m: f64 = parts[1].parse().ok()?;
    let sec: f64 = parts[2].parse().ok()?;
    Some(h * 3600.0 + m * 60.0 + sec)
}

/// Resolve the base directory for relative output paths.
/// On Linux/macOS: uses $OWD (set by AppImage to the launch directory),
/// then falls back to $HOME, then to the current directory.
/// On Windows: uses the executable's parent directory.
pub fn resolve_base_dir() -> std::path::PathBuf {
    #[cfg(target_os = "windows")]
    {
        // On Windows, CWD is typically the app's folder - but use exe dir to be safe
        if let Ok(exe) = std::env::current_exe() {
            if let Some(parent) = exe.parent() {
                return parent.to_path_buf();
            }
        }
        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
    }

    #[cfg(not(target_os = "windows"))]
    {
        // $OWD is set by AppImage to the directory the user launched from
        if let Ok(owd) = std::env::var("OWD") {
            let p = std::path::PathBuf::from(&owd);
            if p.exists() {
                return p;
            }
        }
        // Fall back to $HOME
        if let Ok(home) = std::env::var("HOME") {
            let p = std::path::PathBuf::from(&home);
            if p.exists() {
                return p;
            }
        }
        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
    }
}

/// Validate that `path` is usable as an output folder. Creates it if
/// it does not exist, then performs a write/remove round-trip to verify
/// the caller has write permission. Returns a human-readable error on
/// failure; callers translate this to their own error channel (CLI
/// eprintln+exit, GUI Tauri `Result<(), String>`).
///
/// Extracted from the duplicated blocks in `gui_commands::start_batch`
/// and `cli_main::run_batch` (#4c).
pub fn validate_output_folder(path: &std::path::Path) -> Result<(), String> {
    if !path.exists() {
        std::fs::create_dir_all(path)
            .map_err(|e| format!("Could not create output folder '{}': {e}", path.display()))?;
    }
    let test_path = path.join(".histv_write_test");
    std::fs::write(&test_path, b"")
        .map_err(|e| format!("Output folder '{}' is not writable: {e}", path.display()))?;
    let _ = std::fs::remove_file(&test_path);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::AudioStreamInfo;
    use crate::test_helpers::{default_encoders, default_settings, NoopSink};

    #[test]
    fn batch_request_hdr10plus_defaults_off_and_keeps_explicit_selection() {
        let request: BatchRequest = serde_json::from_str("{}").unwrap();
        let settings = request.into_batch_settings();
        assert!(settings.preserve_hdr); // Old GUI requests keep the prior HDR behavior.
        assert!(!settings.preserve_hdr10plus);
        let request: BatchRequest = serde_json::from_str(r#"{"preserveHdr10plus":true}"#).unwrap();
        let settings = request.into_batch_settings();
        assert!(settings.preserve_hdr10plus);
        assert!(!settings.hardware_decoding);
        let sdr: BatchRequest = serde_json::from_str(r#"{"preserveHdr":false}"#).unwrap();
        let settings = sdr.into_batch_settings();
        assert!(!settings.preserve_hdr);
        assert!(!settings.preserve_hdr10plus);
        let dynamic: BatchRequest =
            serde_json::from_str(r#"{"preserveHdr":false,"preserveHdr10plus":true}"#).unwrap();
        let settings = dynamic.into_batch_settings();
        assert!(!settings.preserve_hdr);
        assert!(settings.preserve_hdr10plus);
        assert!(settings.preserve_hdr || settings.preserve_hdr10plus);
    }

    #[test]
    fn metadata_preparation_failure_does_not_remove_existing_destination() {
        let root = tempfile::tempdir().unwrap();
        let output = root.path().join("existing.mkv");
        std::fs::write(&output, b"previous successful output").unwrap();
        let mut queue = vec![crate::test_helpers::make_queue_item(
            "source.mkv",
            QueueItemStatus::Pending,
            crate::test_helpers::synthetic_probe("hevc", 8.0, 60.0, true),
        )];
        assert!(!metadata_failure(
            0,
            &mut queue,
            None,
            "preparation failed",
            &NoopSink
        ));
        assert_eq!(
            std::fs::read(output).unwrap(),
            b"previous successful output"
        );
        assert_eq!(queue[0].status, QueueItemStatus::Failed);
    }

    #[test]
    fn batch_request_hardware_decoding_is_opt_in() {
        let request: BatchRequest = serde_json::from_str("{}").unwrap();
        assert!(!request.into_batch_settings().hardware_decoding);
        let request: BatchRequest = serde_json::from_str(r#"{"hardwareDecoding":true}"#).unwrap();
        assert!(request.into_batch_settings().hardware_decoding);
    }

    #[test]
    fn duration_validation_accepts_small_measured_rebase_but_rejects_truncation() {
        assert!(durations_match(10.302, 10.135, 0.167, 0.0, 0.1));
        assert!(durations_match(10.0, 10.0, 0.167, 0.0, 0.1));
        assert!(!durations_match(100.0, 90.0, 0.167, 0.0, 0.1));
        assert!(!durations_match(100.0, 90.0, 10.0, 0.0, 0.1));
    }

    #[test]
    fn explicit_audio_copy_never_reencodes_above_the_cap() {
        let mut settings = crate::test_helpers::default_settings();
        settings.audio_encoder = "copy".into();
        settings.audio_cap = 128;
        let resolved = resolve_file_settings("hevc", "mkv", &settings, &[]);
        assert!(matches!(resolved.audio_strategy, AudioStrategy::CopyAll));
        let streams = vec![
            crate::queue::AudioStreamInfo {
                index: 0,
                codec: "eac3".into(),
                bitrate_kbps: 768,
            },
            crate::queue::AudioStreamInfo {
                index: 1,
                codec: "truehd".into(),
                bitrate_kbps: 4000,
            },
        ];
        let (maps, codecs) =
            build_audio_args_from_probe(&streams, &resolved.audio_strategy, "mkv", &NoopSink);
        assert_eq!(maps, ["-map", "0:a:0", "-map", "0:a:1"]);
        assert_eq!(codecs, ["-c:a:0", "copy", "-c:a:1", "copy"]);
    }

    #[test]
    fn hdr10_preflight_never_requests_tools_or_overrides_mkv() {
        let mut item = crate::test_helpers::make_queue_item(
            "movie.mkv",
            QueueItemStatus::Pending,
            crate::probe::ProbeResult::default(),
        );
        item.probe.dovi_profile = Some(8);
        item.probe.dovi_bl_compat_id = Some(1);
        item.probe.has_hdr10plus = true;
        item.probe.video_format =
            crate::video_format::VideoFormat::from_stream(&serde_json::json!({
                "pix_fmt":"yuv420p10le", "color_primaries":"bt2020",
                "color_transfer":"smpte2084", "color_space":"bt2020nc"
            }));
        assert!(preflight_scan(&[item.clone()]).is_empty());
        item.probe.dovi_profile = Some(5);
        let warnings = preflight_scan(&[item]);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].missing_tool.is_none());
        assert!(warnings[0].actual_outcome.contains("blocked"));
        assert_eq!(resolve_container("movie.mkv", "auto", true), "mkv");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn gpu_decode_preserves_surface_depth_and_verified_device_for_all_source_codecs() {
        for (pixel, expected) in [("yuv420p", "nv12"), ("yuv420p10le", "p010")] {
            for codec in ["h264", "hevc", "av1"] {
                let format = crate::video_format::VideoFormat::from_stream(&serde_json::json!({
                    "pix_fmt":pixel,"color_primaries":"bt709","color_transfer":"bt709","color_space":"bt709","color_range":"tv"
                }));
                assert!(format.gpu_decode_eligible(codec, "hevc_vaapi", true, false, false));
                let args = assemble_ffmpeg_args(&FfmpegAssemblyParts {
                    input_path: "source.mkv",
                    output_path: "output.mkv",
                    video_args: &["-c:v", "hevc_vaapi"],
                    render_device: Some("/test/dri/renderD167"),
                    hardware_decode: true,
                    source_format: Some(&format),
                    pix_fmt: pixel,
                    audio_map_args: &[],
                    audio_codec_args: &[],
                    is_image_source: false,
                    threads: 0,
                    tonemap_filter: None,
                    codec_family: "hevc",
                    colour_metadata: None,
                    vaapi_hevc_bsf: None,
                });
                let pos = args.iter().position(|a| a == "-hwaccel_device").unwrap();
                assert_eq!(args[pos + 1], "/test/dri/renderD167");
                assert!(pos < args.iter().position(|a| a == "-i").unwrap());
                let filter = &args[args.iter().position(|a| a == "-vf").unwrap() + 1];
                assert!(filter
                    .split(',')
                    .any(|f| f == format!("scale_vaapi=format={expected}")));
                assert!(filter.contains("setparams=color_primaries=bt709:color_trc=bt709"));
                assert!(!args.iter().any(|a| a.contains("hwupload")
                    || a.contains("hwdownload")
                    || a == "-pix_fmt"));
            }
        }
    }

    // ══════════════════════════════════════════════════════════════
    //  1. decide_encode_strategy
    // ══════════════════════════════════════════════════════════════

    #[test]
    fn test_decide_strategy_copy_below_threshold() {
        // File at 3.0 Mbps with a 4.0 Mbps threshold → Copy.
        let d = decide_encode_strategy(
            3.0,
            4.0,
            "hevc",
            "hevc",
            &RateControlParams {
                mode: "QP",
                qp_i: 20,
                qp_p: 22,
                crf_val: 20,
            },
            1.5,
        );
        assert!(matches!(d, EncodeDecision::Copy));
    }

    #[test]
    fn test_decide_strategy_copy_at_threshold() {
        // Exactly at the threshold → below-or-equal branch → Copy.
        let d = decide_encode_strategy(
            4.0,
            4.0,
            "hevc",
            "hevc",
            &RateControlParams {
                mode: "QP",
                qp_i: 20,
                qp_p: 22,
                crf_val: 20,
            },
            1.5,
        );
        assert!(matches!(d, EncodeDecision::Copy));
    }

    #[test]
    fn test_decide_strategy_vbr_above_threshold() {
        // 10 Mbps with a 4 Mbps threshold → VBR.
        let d = decide_encode_strategy(
            10.0,
            4.0,
            "hevc",
            "hevc",
            &RateControlParams {
                mode: "QP",
                qp_i: 20,
                qp_p: 22,
                crf_val: 20,
            },
            1.5,
        );
        match d {
            EncodeDecision::Vbr {
                target_bps,
                peak_bps,
            } => {
                assert_eq!(target_bps, 4_000_000);
                assert_eq!(peak_bps, 6_000_000); // 4M * 1.5
            }
            _ => panic!("Expected VBR, got {:?}", d),
        }
    }

    #[test]
    fn test_decide_strategy_vbr_custom_peak_multiplier() {
        let d = decide_encode_strategy(
            10.0,
            4.0,
            "hevc",
            "hevc",
            &RateControlParams {
                mode: "QP",
                qp_i: 20,
                qp_p: 22,
                crf_val: 20,
            },
            2.0,
        );
        match d {
            EncodeDecision::Vbr {
                target_bps,
                peak_bps,
            } => {
                assert_eq!(target_bps, 4_000_000);
                assert_eq!(peak_bps, 8_000_000); // 4M * 2.0
            }
            _ => panic!("Expected VBR, got {:?}", d),
        }
    }

    #[test]
    fn test_decide_strategy_vbr_peak_multiplier_below_one_uses_default() {
        // peak_multiplier <= 1.0 should use the default 1.5.
        let d = decide_encode_strategy(
            10.0,
            4.0,
            "hevc",
            "hevc",
            &RateControlParams {
                mode: "QP",
                qp_i: 20,
                qp_p: 22,
                crf_val: 20,
            },
            0.5,
        );
        match d {
            EncodeDecision::Vbr { peak_bps, .. } => {
                assert_eq!(peak_bps, 6_000_000); // 4M * 1.5 (default)
            }
            _ => panic!("Expected VBR, got {:?}", d),
        }
    }

    #[test]
    fn test_decide_strategy_copy_hysteresis_same_codec() {
        // File at 4.5 Mbps, threshold 4.0 → 4.5 <= 4.0 * 1.15 = 4.6 → Copy.
        let d = decide_encode_strategy(
            4.5,
            4.0,
            "hevc",
            "hevc",
            &RateControlParams {
                mode: "QP",
                qp_i: 20,
                qp_p: 22,
                crf_val: 20,
            },
            1.5,
        );
        assert!(matches!(d, EncodeDecision::Copy));
    }

    #[test]
    fn test_decide_strategy_no_hysteresis_different_codec() {
        // File at 4.5 Mbps, threshold 4.0, but source is h264 and target is hevc.
        // Hysteresis only applies when source == target codec, so this should VBR.
        let d = decide_encode_strategy(
            4.5,
            4.0,
            "h264",
            "hevc",
            &RateControlParams {
                mode: "QP",
                qp_i: 20,
                qp_p: 22,
                crf_val: 20,
            },
            1.5,
        );
        assert!(matches!(d, EncodeDecision::Vbr { .. }));
    }

    #[test]
    fn test_decide_strategy_above_hysteresis_same_codec() {
        // File at 5.0 Mbps, threshold 4.0 → 5.0 > 4.0 * 1.15 = 4.6 → VBR.
        let d = decide_encode_strategy(
            5.0,
            4.0,
            "hevc",
            "hevc",
            &RateControlParams {
                mode: "QP",
                qp_i: 20,
                qp_p: 22,
                crf_val: 20,
            },
            1.5,
        );
        assert!(matches!(d, EncodeDecision::Vbr { .. }));
    }

    #[test]
    fn test_decide_strategy_cqp_zero_bitrate() {
        // bitrate_mbps == 0.0 → falls into below-threshold branch,
        // but bitrate is not > 0.0 so cannot Copy. QP mode → CQP.
        let d = decide_encode_strategy(
            0.0,
            4.0,
            "hevc",
            "hevc",
            &RateControlParams {
                mode: "QP",
                qp_i: 20,
                qp_p: 22,
                crf_val: 20,
            },
            1.5,
        );
        match d {
            EncodeDecision::Cqp { qi, qp } => {
                assert_eq!(qi, 20);
                assert_eq!(qp, 22);
            }
            _ => panic!("Expected CQP for zero bitrate, got {:?}", d),
        }
    }

    #[test]
    fn test_decide_strategy_crf_zero_bitrate() {
        // CRF mode with zero bitrate → CRF decision.
        let d = decide_encode_strategy(
            0.0,
            4.0,
            "hevc",
            "hevc",
            &RateControlParams {
                mode: "CRF",
                qp_i: 20,
                qp_p: 22,
                crf_val: 18,
            },
            1.5,
        );
        match d {
            EncodeDecision::Crf {
                crf,
                qi_fallback,
                qp_fallback,
            } => {
                assert_eq!(crf, 18);
                assert_eq!(qi_fallback, 20);
                assert_eq!(qp_fallback, 22);
            }
            _ => panic!("Expected CRF, got {:?}", d),
        }
    }

    #[test]
    fn test_decide_strategy_crf_lowercase() {
        // "crf" (lowercase) should also trigger CRF mode.
        let d = decide_encode_strategy(
            0.0,
            4.0,
            "hevc",
            "hevc",
            &RateControlParams {
                mode: "crf",
                qp_i: 20,
                qp_p: 22,
                crf_val: 18,
            },
            1.5,
        );
        assert!(matches!(d, EncodeDecision::Crf { .. }));
    }

    #[test]
    fn test_decide_strategy_non_copyable_gif() {
        // GIF cannot be stream-copied even if below threshold.
        let d = decide_encode_strategy(
            1.0,
            4.0,
            "gif",
            "hevc",
            &RateControlParams {
                mode: "QP",
                qp_i: 20,
                qp_p: 22,
                crf_val: 20,
            },
            1.5,
        );
        assert!(matches!(d, EncodeDecision::Cqp { .. }));
    }

    #[test]
    fn test_decide_strategy_non_copyable_webp() {
        let d = decide_encode_strategy(
            2.0,
            4.0,
            "webp",
            "hevc",
            &RateControlParams {
                mode: "QP",
                qp_i: 20,
                qp_p: 22,
                crf_val: 20,
            },
            1.5,
        );
        assert!(matches!(d, EncodeDecision::Cqp { .. }));
    }

    #[test]
    fn test_decide_strategy_non_copyable_apng() {
        let d = decide_encode_strategy(
            2.0,
            4.0,
            "apng",
            "hevc",
            &RateControlParams {
                mode: "QP",
                qp_i: 20,
                qp_p: 22,
                crf_val: 20,
            },
            1.5,
        );
        assert!(matches!(d, EncodeDecision::Cqp { .. }));
    }

    #[test]
    fn test_decide_strategy_non_copyable_mjpeg() {
        let d = decide_encode_strategy(
            2.0,
            4.0,
            "mjpeg",
            "hevc",
            &RateControlParams {
                mode: "QP",
                qp_i: 20,
                qp_p: 22,
                crf_val: 20,
            },
            1.5,
        );
        assert!(matches!(d, EncodeDecision::Cqp { .. }));
    }

    #[test]
    fn test_decide_strategy_copy_below_threshold_different_codec() {
        // Below threshold → Copy, even when source != target.
        // The function copies regardless of codec mismatch when below threshold.
        let d = decide_encode_strategy(
            2.0,
            4.0,
            "h264",
            "hevc",
            &RateControlParams {
                mode: "QP",
                qp_i: 20,
                qp_p: 22,
                crf_val: 20,
            },
            1.5,
        );
        assert!(matches!(d, EncodeDecision::Copy));
    }

    #[test]
    fn test_decide_strategy_negative_bitrate() {
        // Negative bitrate (edge case) → treated as below-or-equal to threshold.
        // Not > 0.0, so goes to CQP.
        let d = decide_encode_strategy(
            -1.0,
            4.0,
            "hevc",
            "hevc",
            &RateControlParams {
                mode: "QP",
                qp_i: 20,
                qp_p: 22,
                crf_val: 20,
            },
            1.5,
        );
        assert!(matches!(d, EncodeDecision::Cqp { .. }));
    }

    // ══════════════════════════════════════════════════════════════
    //  2. build_audio_args_from_probe
    // ══════════════════════════════════════════════════════════════

    #[test]
    fn test_audio_copy_capped_below_cap() {
        let streams = vec![AudioStreamInfo {
            index: 0,
            codec: "ac3".to_string(),
            bitrate_kbps: 448,
        }];
        let strategy = AudioStrategy::CopyCapped { cap_kbps: 640 };
        let sink = NoopSink;
        let (map_args, codec_args) = build_audio_args_from_probe(&streams, &strategy, "mkv", &sink);
        assert_eq!(map_args, vec!["-map", "0:a:0"]);
        assert_eq!(codec_args, vec!["-c:a:0", "copy"]);
    }

    #[test]
    fn test_audio_copy_capped_above_cap() {
        let streams = vec![AudioStreamInfo {
            index: 0,
            codec: "ac3".to_string(),
            bitrate_kbps: 768,
        }];
        let strategy = AudioStrategy::CopyCapped { cap_kbps: 640 };
        let sink = NoopSink;
        let (map_args, codec_args) = build_audio_args_from_probe(&streams, &strategy, "mkv", &sink);
        assert_eq!(map_args, vec!["-map", "0:a:0"]);
        // ac3 has an ffmpeg encoder, so re-encode to ac3 at cap.
        assert_eq!(codec_args, vec!["-c:a:0", "ac3", "-b:a:0", "640k"]);
    }

    #[test]
    fn test_audio_copy_capped_at_cap() {
        // Exactly at the cap → copy (<=).
        let streams = vec![AudioStreamInfo {
            index: 0,
            codec: "eac3".to_string(),
            bitrate_kbps: 640,
        }];
        let strategy = AudioStrategy::CopyCapped { cap_kbps: 640 };
        let sink = NoopSink;
        let (_, codec_args) = build_audio_args_from_probe(&streams, &strategy, "mkv", &sink);
        assert_eq!(codec_args, vec!["-c:a:0", "copy"]);
    }

    #[test]
    fn test_audio_copy_capped_opus_above_cap_respects_max() {
        // Opus has a max of 512 kbps via ffmpeg_encoder_for_codec.
        // With cap at 640, the encoder should use min(640, 512) = 512.
        let streams = vec![AudioStreamInfo {
            index: 0,
            codec: "opus".to_string(),
            bitrate_kbps: 768,
        }];
        let strategy = AudioStrategy::CopyCapped { cap_kbps: 640 };
        let sink = NoopSink;
        let (_, codec_args) = build_audio_args_from_probe(&streams, &strategy, "mkv", &sink);
        assert_eq!(codec_args, vec!["-c:a:0", "libopus", "-b:a:0", "512k"]);
    }

    #[test]
    fn test_audio_copy_capped_unknown_codec_no_encoder_fallback_eac3() {
        // A codec without an ffmpeg encoder (e.g. truehd) falls back to EAC3.
        let streams = vec![AudioStreamInfo {
            index: 0,
            codec: "truehd".to_string(),
            bitrate_kbps: 4000,
        }];
        let strategy = AudioStrategy::CopyCapped { cap_kbps: 640 };
        let sink = NoopSink;
        let (_, codec_args) = build_audio_args_from_probe(&streams, &strategy, "mkv", &sink);
        assert_eq!(codec_args, vec!["-c:a:0", "eac3", "-b:a:0", "640k"]);
    }

    #[test]
    fn test_audio_compat_capped_aac_below_cap() {
        let streams = vec![AudioStreamInfo {
            index: 0,
            codec: "aac".to_string(),
            bitrate_kbps: 256,
        }];
        let strategy = AudioStrategy::CompatCapped { cap_kbps: 640 };
        let sink = NoopSink;
        let (_, codec_args) = build_audio_args_from_probe(&streams, &strategy, "mkv", &sink);
        assert_eq!(codec_args, vec!["-c:a:0", "copy"]);
    }

    #[test]
    fn test_audio_compat_capped_aac_above_cap() {
        let streams = vec![AudioStreamInfo {
            index: 0,
            codec: "aac".to_string(),
            bitrate_kbps: 768,
        }];
        let strategy = AudioStrategy::CompatCapped { cap_kbps: 640 };
        let sink = NoopSink;
        let (_, codec_args) = build_audio_args_from_probe(&streams, &strategy, "mkv", &sink);
        // AAC above cap → re-encode to AAC at cap.
        assert_eq!(codec_args, vec!["-c:a:0", "aac", "-b:a:0", "640k"]);
    }

    #[test]
    fn test_audio_compat_capped_non_aac_reencode() {
        // Non-AAC codec in compat mode → always re-encode to AAC.
        let streams = vec![AudioStreamInfo {
            index: 0,
            codec: "ac3".to_string(),
            bitrate_kbps: 448,
        }];
        let strategy = AudioStrategy::CompatCapped { cap_kbps: 640 };
        let sink = NoopSink;
        let (_, codec_args) = build_audio_args_from_probe(&streams, &strategy, "mkv", &sink);
        // target_br = min(448, 640) = 448
        assert_eq!(codec_args, vec!["-c:a:0", "aac", "-b:a:0", "448k"]);
    }

    #[test]
    fn test_audio_compat_capped_non_aac_above_cap() {
        // Non-AAC above cap → re-encode to AAC at cap.
        let streams = vec![AudioStreamInfo {
            index: 0,
            codec: "eac3".to_string(),
            bitrate_kbps: 768,
        }];
        let strategy = AudioStrategy::CompatCapped { cap_kbps: 640 };
        let sink = NoopSink;
        let (_, codec_args) = build_audio_args_from_probe(&streams, &strategy, "mkv", &sink);
        assert_eq!(codec_args, vec!["-c:a:0", "aac", "-b:a:0", "640k"]);
    }

    #[test]
    fn test_audio_empty_streams() {
        let streams: Vec<AudioStreamInfo> = vec![];
        let strategy = AudioStrategy::CopyCapped { cap_kbps: 640 };
        let sink = NoopSink;
        let (map_args, codec_args) = build_audio_args_from_probe(&streams, &strategy, "mkv", &sink);
        assert!(map_args.is_empty());
        assert!(codec_args.is_empty());
    }

    #[test]
    fn test_audio_unknown_codec_skipped() {
        // "unknown" codec streams are skipped entirely.
        let streams = vec![AudioStreamInfo {
            index: 0,
            codec: "unknown".to_string(),
            bitrate_kbps: 0,
        }];
        let strategy = AudioStrategy::CopyCapped { cap_kbps: 640 };
        let sink = NoopSink;
        let (map_args, codec_args) = build_audio_args_from_probe(&streams, &strategy, "mkv", &sink);
        assert!(map_args.is_empty());
        assert!(codec_args.is_empty());
    }

    #[test]
    fn test_audio_multiple_streams_mixed() {
        // Two streams: one below cap (copy), one above cap (re-encode).
        let streams = vec![
            AudioStreamInfo {
                index: 0,
                codec: "ac3".to_string(),
                bitrate_kbps: 384,
            },
            AudioStreamInfo {
                index: 1,
                codec: "ac3".to_string(),
                bitrate_kbps: 768,
            },
        ];
        let strategy = AudioStrategy::CopyCapped { cap_kbps: 640 };
        let sink = NoopSink;
        let (map_args, codec_args) = build_audio_args_from_probe(&streams, &strategy, "mkv", &sink);
        assert_eq!(map_args, vec!["-map", "0:a:0", "-map", "0:a:1"]);
        assert_eq!(
            codec_args,
            vec!["-c:a:0", "copy", "-c:a:1", "ac3", "-b:a:1", "640k"]
        );
    }

    // ══════════════════════════════════════════════════════════════
    //  3. resolve_file_settings
    // ══════════════════════════════════════════════════════════════

    #[test]
    fn test_resolve_compatibility_mode() {
        let mut settings = default_settings();
        settings.compatibility_mode = true;
        let encoders = default_encoders();
        let result = resolve_file_settings("hevc", "mkv", &settings, &encoders);
        assert_eq!(result.codec_family, "h264");
        assert_eq!(result.container_ext, "mp4");
        assert!(matches!(
            result.audio_strategy,
            AudioStrategy::CompatCapped { .. }
        ));
    }

    #[test]
    fn test_resolve_source_h264_stays_h264() {
        let settings = default_settings();
        let encoders = default_encoders();
        let result = resolve_file_settings("h264", "mkv", &settings, &encoders);
        assert_eq!(result.codec_family, "h264");
    }

    #[test]
    fn test_resolve_source_hevc_stays_hevc() {
        let settings = default_settings();
        let encoders = default_encoders();
        let result = resolve_file_settings("hevc", "mkv", &settings, &encoders);
        assert_eq!(result.codec_family, "hevc");
    }

    #[test]
    fn test_resolve_source_h265_alias_stays_hevc() {
        // "h265" is an alias for "hevc".
        let settings = default_settings();
        let encoders = default_encoders();
        let result = resolve_file_settings("h265", "mkv", &settings, &encoders);
        assert_eq!(result.codec_family, "hevc");
    }

    #[test]
    fn test_resolve_preserve_av1_with_av1_source() {
        let mut settings = default_settings();
        settings.preserve_av1 = true;
        let encoders = default_encoders();
        let result = resolve_file_settings("av1", "mkv", &settings, &encoders);
        assert_eq!(result.codec_family, "av1");
    }

    #[test]
    fn test_resolve_av1_without_preserve_defaults_hevc() {
        // Without preserve_av1, AV1 source defaults to HEVC.
        let settings = default_settings();
        let encoders = default_encoders();
        let result = resolve_file_settings("av1", "mkv", &settings, &encoders);
        assert_eq!(result.codec_family, "hevc");
    }

    #[test]
    fn test_resolve_mpeg2_defaults_hevc() {
        // Any unrecognised codec defaults to HEVC.
        let settings = default_settings();
        let encoders = default_encoders();
        let result = resolve_file_settings("mpeg2video", "avi", &settings, &encoders);
        assert_eq!(result.codec_family, "hevc");
    }

    #[test]
    fn test_resolve_precision_mode_forces_software() {
        let mut settings = default_settings();
        settings.precision_mode = true;
        let encoders = default_encoders();
        let result = resolve_file_settings("hevc", "mkv", &settings, &encoders);
        assert_eq!(result.encoder_name, "libx265");
    }

    #[test]
    fn test_resolve_hw_encoder_preferred_when_available() {
        let settings = default_settings();
        let encoders = default_encoders();
        let result = resolve_file_settings("hevc", "mkv", &settings, &encoders);
        assert_eq!(result.encoder_name, "hevc_nvenc");
    }

    #[test]
    fn test_resolve_container_auto_from_mp4() {
        let settings = default_settings();
        let encoders = default_encoders();
        let result = resolve_file_settings("hevc", "mp4", &settings, &encoders);
        assert_eq!(result.container_ext, "mp4");
    }

    #[test]
    fn test_resolve_container_auto_from_avi() {
        // AVI → auto → MKV (default for non-mp4/mkv containers).
        let settings = default_settings();
        let encoders = default_encoders();
        let result = resolve_file_settings("hevc", "avi", &settings, &encoders);
        assert_eq!(result.container_ext, "mkv");
    }

    #[test]
    fn test_resolve_audio_strategy_default_copy_capped() {
        let settings = default_settings();
        let encoders = default_encoders();
        let result = resolve_file_settings("hevc", "mkv", &settings, &encoders);
        assert!(matches!(
            result.audio_strategy,
            AudioStrategy::CopyCapped { cap_kbps: 640 }
        ));
    }

    // ══════════════════════════════════════════════════════════════
    //  4. vbr_flags / cqp_flags / crf_flags
    // ══════════════════════════════════════════════════════════════

    #[test]
    fn test_vbr_flags_libx265() {
        let flags = vbr_flags("libx265", "4M", "6M");
        assert!(flags.contains(&"-preset".to_string()));
        assert!(flags.contains(&"slow".to_string()));
        assert!(flags.contains(&"-b:v".to_string()));
        assert!(flags.contains(&"4M".to_string()));
        assert!(flags.contains(&"-maxrate".to_string()));
        assert!(flags.contains(&"6M".to_string()));
    }

    #[test]
    fn test_vbr_flags_libx264() {
        let flags = vbr_flags("libx264", "3M", "4.5M");
        assert!(flags.contains(&"-preset".to_string()));
        assert!(flags.contains(&"slow".to_string()));
        assert!(flags.contains(&"-b:v".to_string()));
        assert!(flags.contains(&"3M".to_string()));
    }

    #[test]
    fn test_vbr_flags_nvenc() {
        let flags = vbr_flags("hevc_nvenc", "4M", "6M");
        assert!(flags.contains(&"-preset".to_string()));
        assert!(flags.contains(&"p7".to_string()));
        assert!(flags.contains(&"-rc".to_string()));
        assert!(flags.contains(&"vbr".to_string()));
    }

    #[test]
    fn test_vbr_flags_amf() {
        let flags = vbr_flags("hevc_amf", "4M", "6M");
        assert!(flags.contains(&"-quality".to_string()));
        assert!(flags.contains(&"quality".to_string()));
        assert!(flags.contains(&"-rc".to_string()));
        assert!(flags.contains(&"vbr_peak".to_string()));
    }

    #[test]
    fn test_vbr_flags_qsv() {
        let flags = vbr_flags("hevc_qsv", "4M", "6M");
        assert!(flags.contains(&"-preset".to_string()));
        assert!(flags.contains(&"veryslow".to_string()));
    }

    #[test]
    fn test_vbr_flags_videotoolbox() {
        let flags = vbr_flags("hevc_videotoolbox", "4M", "6M");
        assert_eq!(flags, vec!["-b:v", "4M", "-maxrate", "6M"]);
    }

    #[test]
    fn test_vbr_flags_vaapi() {
        let flags = vbr_flags("hevc_vaapi", "4M", "6M");
        assert!(flags.contains(&"-rc_mode".to_string()));
        assert!(flags.contains(&"VBR".to_string()));
    }

    #[test]
    fn test_vbr_flags_svtav1() {
        let flags = vbr_flags("libsvtav1", "4M", "6M");
        assert!(flags.contains(&"-preset".to_string()));
        assert!(flags.contains(&"6".to_string()));
        // SVT-AV1 rejects -maxrate in VBR mode
        assert!(!flags.contains(&"-maxrate".to_string()));
    }

    #[test]
    fn test_vbr_flags_av1_nvenc() {
        // AV1 HW encoders use the same flags as their H.264/HEVC counterparts.
        let flags = vbr_flags("av1_nvenc", "4M", "6M");
        assert!(flags.contains(&"p7".to_string()));
    }

    #[test]
    fn test_vbr_flags_unknown_encoder() {
        let flags = vbr_flags("some_unknown", "4M", "6M");
        assert_eq!(flags, vec!["-b:v", "4M", "-maxrate", "6M"]);
    }

    #[test]
    fn test_cqp_flags_libx265() {
        let flags = cqp_flags("libx265", "20", "22");
        assert!(flags.contains(&"-preset".to_string()));
        assert!(flags.contains(&"slow".to_string()));
        assert!(flags.contains(&"-qp".to_string()));
        assert!(flags.contains(&"20".to_string()));
    }

    #[test]
    fn test_cqp_flags_nvenc() {
        let flags = cqp_flags("hevc_nvenc", "20", "22");
        assert!(flags.contains(&"-rc".to_string()));
        assert!(flags.contains(&"constqp".to_string()));
        assert!(flags.contains(&"-qp".to_string()));
        assert!(flags.contains(&"20".to_string()));
    }

    #[test]
    fn test_cqp_flags_amf_uses_qi_and_qp() {
        let flags = cqp_flags("hevc_amf", "20", "22");
        assert!(flags.contains(&"-qp_i".to_string()));
        assert!(flags.contains(&"20".to_string()));
        assert!(flags.contains(&"-qp_p".to_string()));
        assert!(flags.contains(&"22".to_string()));
    }

    #[test]
    fn test_cqp_flags_qsv() {
        let flags = cqp_flags("hevc_qsv", "20", "22");
        assert!(flags.contains(&"-global_quality".to_string()));
        assert!(flags.contains(&"20".to_string()));
    }

    #[test]
    fn test_cqp_flags_videotoolbox() {
        let flags = cqp_flags("hevc_videotoolbox", "20", "22");
        assert_eq!(flags, vec!["-q:v", "20"]);
    }

    #[test]
    fn test_cqp_flags_vaapi() {
        let flags = cqp_flags("hevc_vaapi", "20", "22");
        assert!(flags.contains(&"-rc_mode".to_string()));
        assert!(flags.contains(&"CQP".to_string()));
    }

    #[test]
    fn test_cqp_flags_svtav1() {
        let flags = cqp_flags("libsvtav1", "20", "22");
        assert!(flags.contains(&"-preset".to_string()));
        assert!(flags.contains(&"6".to_string()));
        assert!(flags.contains(&"-rc".to_string()));
        assert!(flags.contains(&"0".to_string()));
        assert!(flags.contains(&"-qp".to_string()));
    }

    #[test]
    fn test_crf_flags_libx265() {
        let flags = crf_flags("libx265", "18", "20", "22");
        assert!(flags.contains(&"-preset".to_string()));
        assert!(flags.contains(&"slow".to_string()));
        assert!(flags.contains(&"-crf".to_string()));
        assert!(flags.contains(&"18".to_string()));
        // Should NOT contain -qp (CRF, not CQP).
        assert!(!flags.contains(&"-qp".to_string()));
    }

    #[test]
    fn test_crf_flags_libx264() {
        let flags = crf_flags("libx264", "22", "20", "22");
        assert!(flags.contains(&"-crf".to_string()));
        assert!(flags.contains(&"22".to_string()));
    }

    #[test]
    fn test_crf_flags_svtav1() {
        let flags = crf_flags("libsvtav1", "30", "20", "22");
        assert!(flags.contains(&"-crf".to_string()));
        assert!(flags.contains(&"30".to_string()));
    }

    #[test]
    fn test_crf_flags_hw_encoder_falls_back_to_cqp() {
        // Hardware encoders do not support CRF; should fall back to CQP flags.
        let flags = crf_flags("hevc_nvenc", "18", "20", "22");
        // Should contain CQP-style flags, not CRF.
        assert!(!flags.contains(&"-crf".to_string()));
        assert!(flags.contains(&"-rc".to_string()));
        assert!(flags.contains(&"constqp".to_string()));
    }

    #[test]
    fn test_crf_flags_amf_falls_back_to_cqp() {
        let flags = crf_flags("hevc_amf", "18", "20", "22");
        assert!(!flags.contains(&"-crf".to_string()));
        assert!(flags.contains(&"-rc".to_string()));
        assert!(flags.contains(&"cqp".to_string()));
    }

    // ══════════════════════════════════════════════════════════════
    //  5. build_audio_args_from_probe — MP4 container
    // ══════════════════════════════════════════════════════════════

    #[test]
    fn test_audio_opus_below_cap_in_mp4_reencodes_eac3() {
        // Opus is not MP4-compatible, so even below the cap it must be
        // re-encoded. The fallback codec for MP4 is EAC3.
        let streams = vec![AudioStreamInfo {
            index: 0,
            codec: "opus".to_string(),
            bitrate_kbps: 200,
        }];
        let strategy = AudioStrategy::CopyCapped { cap_kbps: 640 };
        let sink = NoopSink;
        let (map_args, codec_args) = build_audio_args_from_probe(&streams, &strategy, "mp4", &sink);
        assert_eq!(map_args, vec!["-map", "0:a:0"]);
        assert_eq!(codec_args, vec!["-c:a:0", "eac3", "-b:a:0", "640k"]);
    }

    #[test]
    fn test_audio_vorbis_below_cap_in_mp4_reencodes_eac3() {
        // Vorbis is not MP4-compatible, same behaviour as Opus above.
        let streams = vec![AudioStreamInfo {
            index: 0,
            codec: "vorbis".to_string(),
            bitrate_kbps: 300,
        }];
        let strategy = AudioStrategy::CopyCapped { cap_kbps: 640 };
        let sink = NoopSink;
        let (map_args, codec_args) = build_audio_args_from_probe(&streams, &strategy, "mp4", &sink);
        assert_eq!(map_args, vec!["-map", "0:a:0"]);
        assert_eq!(codec_args, vec!["-c:a:0", "eac3", "-b:a:0", "640k"]);
    }

    #[test]
    fn test_audio_aac_below_cap_in_mp4_copies() {
        // AAC is MP4-compatible and below cap — should be copied.
        let streams = vec![AudioStreamInfo {
            index: 0,
            codec: "aac".to_string(),
            bitrate_kbps: 200,
        }];
        let strategy = AudioStrategy::CopyCapped { cap_kbps: 640 };
        let sink = NoopSink;
        let (map_args, codec_args) = build_audio_args_from_probe(&streams, &strategy, "mp4", &sink);
        assert_eq!(map_args, vec!["-map", "0:a:0"]);
        assert_eq!(codec_args, vec!["-c:a:0", "copy"]);
    }

    #[test]
    fn test_audio_ac3_above_cap_in_mp4_reencodes_ac3() {
        // AC3 is MP4-compatible but above the cap — re-encode to AC3 at cap.
        let streams = vec![AudioStreamInfo {
            index: 0,
            codec: "ac3".to_string(),
            bitrate_kbps: 800,
        }];
        let strategy = AudioStrategy::CopyCapped { cap_kbps: 640 };
        let sink = NoopSink;
        let (map_args, codec_args) = build_audio_args_from_probe(&streams, &strategy, "mp4", &sink);
        assert_eq!(map_args, vec!["-map", "0:a:0"]);
        assert_eq!(codec_args, vec!["-c:a:0", "ac3", "-b:a:0", "640k"]);
    }

    // ══════════════════════════════════════════════════════════════
    //  6. assemble_ffmpeg_args
    // ══════════════════════════════════════════════════════════════

    #[test]
    fn vaapi_mp4_retains_parameter_sets_without_changing_other_backends() {
        for (encoder, tag) in [
            ("hevc_vaapi", "hev1"),
            ("copy", "hev1"),
            ("hevc_nvenc", "hvc1"),
            ("hevc_qsv", "hvc1"),
            ("libx265", "hvc1"),
        ] {
            let args = assemble_ffmpeg_args(&FfmpegAssemblyParts {
                input_path: "input.mkv",
                output_path: "output.mp4",
                video_args: &["-c:v", encoder],
                render_device: Some("/test/render"),
                hardware_decode: false,
                source_format: None,
                pix_fmt: "yuv420p10le",
                audio_map_args: &[],
                audio_codec_args: &[],
                is_image_source: false,
                threads: 0,
                tonemap_filter: None,
                codec_family: "hevc",
                colour_metadata: None,
                vaapi_hevc_bsf: None,
            });
            let index = args.iter().position(|a| a == "-tag:v").unwrap();
            assert_eq!(args[index + 1], tag);
        }
    }

    #[test]
    fn test_assemble_hevc_in_mp4_has_hvc1_and_faststart() {
        let args = assemble_ffmpeg_args(&FfmpegAssemblyParts {
            input_path: "input.mkv",
            output_path: "output.mp4",
            video_args: &["-c:v", "libx265"],
            render_device: None,
            hardware_decode: false,
            source_format: None,
            pix_fmt: "yuv420p",
            audio_map_args: &[],
            audio_codec_args: &[],
            is_image_source: false,
            threads: 0,
            tonemap_filter: None,
            codec_family: "hevc",
            colour_metadata: None,
            vaapi_hevc_bsf: None,
        });
        assert!(args.contains(&"-tag:v".to_string()));
        assert!(args.contains(&"hvc1".to_string()));
        assert!(args.contains(&"-movflags".to_string()));
        assert!(args.contains(&"+faststart".to_string()));
    }

    #[test]
    fn test_assemble_h264_in_mp4_no_hvc1_tag() {
        let args = assemble_ffmpeg_args(&FfmpegAssemblyParts {
            input_path: "input.mkv",
            output_path: "output.mp4",
            video_args: &["-c:v", "libx264"],
            render_device: None,
            hardware_decode: false,
            source_format: None,
            pix_fmt: "yuv420p",
            audio_map_args: &[],
            audio_codec_args: &[],
            is_image_source: false,
            threads: 0,
            tonemap_filter: None,
            codec_family: "h264",
            colour_metadata: None,
            vaapi_hevc_bsf: None,
        });
        assert!(!args.contains(&"-tag:v".to_string()));
        assert!(!args.contains(&"hvc1".to_string()));
        // Should still have faststart for MP4.
        assert!(args.contains(&"-movflags".to_string()));
        assert!(args.contains(&"+faststart".to_string()));
    }

    #[test]
    fn test_assemble_hevc_in_mkv_no_hvc1_or_movflags() {
        let args = assemble_ffmpeg_args(&FfmpegAssemblyParts {
            input_path: "input.mkv",
            output_path: "output.mkv",
            video_args: &["-c:v", "libx265"],
            render_device: None,
            hardware_decode: false,
            source_format: None,
            pix_fmt: "yuv420p",
            audio_map_args: &[],
            audio_codec_args: &[],
            is_image_source: false,
            threads: 0,
            tonemap_filter: None,
            codec_family: "hevc",
            colour_metadata: None,
            vaapi_hevc_bsf: None,
        });
        assert!(!args.contains(&"-tag:v".to_string()));
        assert!(!args.contains(&"hvc1".to_string()));
        assert!(!args.contains(&"-movflags".to_string()));
        assert!(!args.contains(&"+faststart".to_string()));
    }

    #[test]
    fn test_assemble_mp4_subtitles_use_mov_text() {
        let args = assemble_ffmpeg_args(&FfmpegAssemblyParts {
            input_path: "input.mkv",
            output_path: "output.mp4",
            video_args: &["-c:v", "libx265"],
            render_device: None,
            hardware_decode: false,
            source_format: None,
            pix_fmt: "yuv420p",
            audio_map_args: &[],
            audio_codec_args: &[],
            is_image_source: false,
            threads: 0,
            tonemap_filter: None,
            codec_family: "hevc",
            colour_metadata: None,
            vaapi_hevc_bsf: None,
        });
        assert!(args.contains(&"-c:s".to_string()));
        assert!(args.contains(&"mov_text".to_string()));
        // The subtitle codec value following -c:s must not be "copy".
        let cs_positions: Vec<usize> = args
            .iter()
            .enumerate()
            .filter(|(_, a)| *a == "-c:s")
            .map(|(i, _)| i)
            .collect();
        for pos in cs_positions {
            assert_ne!(args[pos + 1], "copy");
        }
    }

    #[test]
    fn test_assemble_mkv_subtitles_use_copy() {
        let args = assemble_ffmpeg_args(&FfmpegAssemblyParts {
            input_path: "input.mkv",
            output_path: "output.mkv",
            video_args: &["-c:v", "libx265"],
            render_device: None,
            hardware_decode: false,
            source_format: None,
            pix_fmt: "yuv420p",
            audio_map_args: &[],
            audio_codec_args: &[],
            is_image_source: false,
            threads: 0,
            tonemap_filter: None,
            codec_family: "hevc",
            colour_metadata: None,
            vaapi_hevc_bsf: None,
        });
        assert!(args.contains(&"-c:s".to_string()));
        assert!(args.contains(&"copy".to_string()));
        // Should not contain "mov_text".
        assert!(!args.contains(&"mov_text".to_string()));
    }

    /// HDR→SDR tonemap must emit a -vf filter before -pix_fmt when
    /// re-encoding. Regression test for the HDR H.264 batch bug where
    /// the tonemap filter was paired with -c:v copy and ffmpeg refused
    /// with EINVAL (exit 234).
    #[test]
    fn test_assemble_tonemap_with_reencode_emits_vf_filter() {
        let args = assemble_ffmpeg_args(&FfmpegAssemblyParts {
            input_path: "input.mkv",
            output_path: "output.mkv",
            video_args: &["-c:v", "libx265"],
            render_device: None,
            hardware_decode: false,
            source_format: None,
            pix_fmt: "yuv420p",
            audio_map_args: &[],
            audio_codec_args: &[],
            is_image_source: false,
            threads: 0,
            tonemap_filter: Some(TONEMAP_HABLE),
            codec_family: "hevc",
            colour_metadata: None,
            vaapi_hevc_bsf: None,
        });
        assert!(args.contains(&"-vf".to_string()));
        assert!(args.iter().any(|a| a.contains("tonemap=hable")));
        // -vf must precede -pix_fmt so the tonemap runs before format
        // conversion.
        let vf_pos = args.iter().position(|a| a == "-vf").unwrap();
        let pix_pos = args.iter().position(|a| a == "-pix_fmt").unwrap();
        assert!(vf_pos < pix_pos);
    }

    /// The assemble helper must refuse -c:v copy paired with a tonemap
    /// filter. ffmpeg rejects this combination at runtime (EINVAL), so
    /// the debug_assert is a defence-in-depth guard against future
    /// regressions where a caller forgets to override a Copy decision
    /// when tonemapping is required.
    #[test]
    #[should_panic(expected = "tonemap_filter is incompatible with -c:v copy")]
    fn test_assemble_tonemap_with_copy_panics() {
        let _ = assemble_ffmpeg_args(&FfmpegAssemblyParts {
            input_path: "input.mkv",
            output_path: "output.mkv",
            video_args: &["-c:v", "copy"],
            render_device: None,
            hardware_decode: false,
            source_format: None,
            pix_fmt: "yuv420p",
            audio_map_args: &[],
            audio_codec_args: &[],
            is_image_source: false,
            threads: 0,
            tonemap_filter: Some(TONEMAP_HABLE),
            codec_family: "h264",
            colour_metadata: None,
            vaapi_hevc_bsf: None,
        });
    }

    // ══════════════════════════════════════════════════════════════
    //  6. Compat mode invariants
    // ══════════════════════════════════════════════════════════════

    /// Compat mode MUST produce CompatCapped regardless of audio_encoder setting.
    #[test]
    fn test_compat_mode_forces_compat_capped_for_all_audio_encoders() {
        let audio_values = ["auto", "", "copy", "aac", "ac3", "eac3", "opus", "flac"];
        let encoders = default_encoders();
        for audio in &audio_values {
            let mut settings = default_settings();
            settings.compatibility_mode = true;
            settings.audio_encoder = audio.to_string();
            let result = resolve_file_settings("hevc", "mkv", &settings, &encoders);
            assert!(
                matches!(result.audio_strategy, AudioStrategy::CompatCapped { .. }),
                "audio_encoder={:?} should produce CompatCapped in compat mode, got {:?}",
                audio,
                result.audio_strategy,
            );
        }
    }

    /// Explicit Copy bypasses the cap; Auto continues to use CopyCapped.
    #[test]
    fn test_no_compat_copy_produces_unmodified_passthrough() {
        let mut settings = default_settings();
        settings.audio_encoder = "copy".to_string();
        let encoders = default_encoders();
        let result = resolve_file_settings("hevc", "mkv", &settings, &encoders);
        assert!(matches!(result.audio_strategy, AudioStrategy::CopyAll));
    }

    /// Without compat mode, explicit "aac" should produce CompatCapped.
    #[test]
    fn test_no_compat_explicit_aac_produces_compat_capped() {
        let mut settings = default_settings();
        settings.audio_encoder = "aac".to_string();
        let encoders = default_encoders();
        let result = resolve_file_settings("hevc", "mkv", &settings, &encoders);
        assert!(matches!(
            result.audio_strategy,
            AudioStrategy::CompatCapped { .. }
        ));
    }

    /// Compat mode MUST produce h264 codec and mp4 container regardless of
    /// codec_family, output_container, and source codec settings.
    #[test]
    fn test_compat_mode_forces_h264_mp4_across_settings() {
        let containers = ["auto", "mkv", "mp4"];
        let source_codecs = ["hevc", "h264", "av1", "mpeg2video"];
        let encoders = default_encoders();
        for container in &containers {
            for source in &source_codecs {
                let mut settings = default_settings();
                settings.compatibility_mode = true;
                settings.output_container = container.to_string();
                let result = resolve_file_settings(source, "mkv", &settings, &encoders);
                assert_eq!(
                    result.container_ext, "mp4",
                    "compat mode with container={}, source={} should force mp4",
                    container, source,
                );
                assert_eq!(
                    result.codec_family, "h264",
                    "compat mode with source={} should force h264",
                    source,
                );
            }
        }
    }

    // ══════════════════════════════════════════════════════════════
    //  7. Composed: resolve_file_settings + build_audio_args_from_probe
    // ══════════════════════════════════════════════════════════════

    /// Compat mode with DTS audio and explicit --audio copy: resolve should
    /// still pick CompatCapped, which makes build_audio_args re-encode to AAC.
    #[test]
    fn test_composed_compat_dts_becomes_aac() {
        let mut settings = default_settings();
        settings.compatibility_mode = true;
        settings.audio_encoder = "copy".to_string();
        let encoders = default_encoders();
        let resolved = resolve_file_settings("hevc", "mkv", &settings, &encoders);

        let streams = vec![AudioStreamInfo {
            index: 0,
            codec: "dts".to_string(),
            bitrate_kbps: 1509,
        }];
        let sink = NoopSink;
        let (_, codec_args) = build_audio_args_from_probe(
            &streams,
            &resolved.audio_strategy,
            &resolved.container_ext,
            &sink,
        );
        assert_eq!(codec_args[1], "aac");
    }

    /// Compat mode with TrueHD audio: same pipeline, AAC output.
    #[test]
    fn test_composed_compat_truehd_becomes_aac() {
        let mut settings = default_settings();
        settings.compatibility_mode = true;
        let encoders = default_encoders();
        let resolved = resolve_file_settings("hevc", "mkv", &settings, &encoders);

        let streams = vec![AudioStreamInfo {
            index: 0,
            codec: "truehd".to_string(),
            bitrate_kbps: 4000,
        }];
        let sink = NoopSink;
        let (_, codec_args) = build_audio_args_from_probe(
            &streams,
            &resolved.audio_strategy,
            &resolved.container_ext,
            &sink,
        );
        assert_eq!(codec_args[1], "aac");
    }

    /// Non-compat mode with DTS audio in MKV: CopyCapped, DTS copies
    /// (below cap, MKV-compatible).
    #[test]
    fn test_composed_no_compat_dts_copies_in_mkv() {
        let settings = default_settings();
        let encoders = default_encoders();
        let resolved = resolve_file_settings("hevc", "mkv", &settings, &encoders);

        let streams = vec![AudioStreamInfo {
            index: 0,
            codec: "dts".to_string(),
            bitrate_kbps: 448,
        }];
        let sink = NoopSink;
        let (_, codec_args) = build_audio_args_from_probe(
            &streams,
            &resolved.audio_strategy,
            &resolved.container_ext,
            &sink,
        );
        assert_eq!(codec_args, vec!["-c:a:0", "copy"]);
    }

    // ══════════════════════════════════════════════════════════════
    //  8. Remux fallback audio scenarios
    // ══════════════════════════════════════════════════════════════

    /// Simulate remux fallback in compat mode: DTS source → MP4.
    /// CompatCapped should re-encode to AAC, never copy DTS into MP4.
    #[test]
    fn test_remux_compat_dts_in_mp4_reencodes() {
        let streams = vec![AudioStreamInfo {
            index: 0,
            codec: "dts".to_string(),
            bitrate_kbps: 1509,
        }];
        let strategy = AudioStrategy::CompatCapped { cap_kbps: 640 };
        let sink = NoopSink;
        let (map_args, codec_args) = build_audio_args_from_probe(&streams, &strategy, "mp4", &sink);
        assert_eq!(map_args, vec!["-map", "0:a:0"]);
        assert_eq!(codec_args[1], "aac");
        assert!(!codec_args.contains(&"copy".to_string()));
    }

    /// Simulate remux fallback in compat mode: TrueHD source → MP4.
    #[test]
    fn test_remux_compat_truehd_in_mp4_reencodes() {
        let streams = vec![AudioStreamInfo {
            index: 0,
            codec: "truehd".to_string(),
            bitrate_kbps: 4000,
        }];
        let strategy = AudioStrategy::CompatCapped { cap_kbps: 640 };
        let sink = NoopSink;
        let (_, codec_args) = build_audio_args_from_probe(&streams, &strategy, "mp4", &sink);
        assert_eq!(codec_args[1], "aac");
    }

    /// Simulate remux fallback with CopyCapped + DTS in MP4.
    /// DTS is not MP4-compatible, so CopyCapped should fall back to EAC3.
    #[test]
    fn test_remux_no_compat_dts_in_mp4_falls_back_to_eac3() {
        let streams = vec![AudioStreamInfo {
            index: 0,
            codec: "dts".to_string(),
            bitrate_kbps: 1509,
        }];
        let strategy = AudioStrategy::CopyCapped { cap_kbps: 640 };
        let sink = NoopSink;
        let (_, codec_args) = build_audio_args_from_probe(&streams, &strategy, "mp4", &sink);
        assert_eq!(codec_args[1], "eac3");
    }

    /// Remux with AAC in compat mode: AAC below cap should be copied.
    #[test]
    fn test_remux_compat_aac_below_cap_copies() {
        let streams = vec![AudioStreamInfo {
            index: 0,
            codec: "aac".to_string(),
            bitrate_kbps: 256,
        }];
        let strategy = AudioStrategy::CompatCapped { cap_kbps: 640 };
        let sink = NoopSink;
        let (_, codec_args) = build_audio_args_from_probe(&streams, &strategy, "mp4", &sink);
        assert_eq!(codec_args, vec!["-c:a:0", "copy"]);
    }

    /// Multi-stream remux: mixed codecs in compat mode + MP4.
    #[test]
    fn test_remux_compat_mixed_streams_mp4() {
        let streams = vec![
            AudioStreamInfo {
                index: 0,
                codec: "aac".to_string(),
                bitrate_kbps: 256,
            },
            AudioStreamInfo {
                index: 1,
                codec: "dts".to_string(),
                bitrate_kbps: 1509,
            },
        ];
        let strategy = AudioStrategy::CompatCapped { cap_kbps: 640 };
        let sink = NoopSink;
        let (map_args, codec_args) = build_audio_args_from_probe(&streams, &strategy, "mp4", &sink);
        assert_eq!(map_args, vec!["-map", "0:a:0", "-map", "0:a:1"]);
        // Stream 0: AAC below cap → copy
        assert_eq!(codec_args[0], "-c:a:0");
        assert_eq!(codec_args[1], "copy");
        // Stream 1: DTS → AAC re-encode
        assert_eq!(codec_args[2], "-c:a:1");
        assert_eq!(codec_args[3], "aac");
    }

    // ══════════════════════════════════════════════════════════════
    //  validate_output_folder (#4c)
    // ══════════════════════════════════════════════════════════════

    #[test]
    fn test_validate_output_folder_creates_missing_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let nested = tmp.path().join("sub").join("dir");
        assert!(!nested.exists());
        validate_output_folder(&nested).expect("should create and validate");
        assert!(nested.is_dir());
        // Probe file must have been cleaned up.
        assert!(!nested.join(".histv_write_test").exists());
    }

    #[test]
    fn test_validate_output_folder_accepts_existing_writable_dir() {
        let tmp = tempfile::tempdir().unwrap();
        validate_output_folder(tmp.path()).expect("existing writable dir should pass");
        assert!(!tmp.path().join(".histv_write_test").exists());
    }

    // Note: a "read-only dir is rejected" test would be ideal, but
    // filesystem DAC checks are bypassed by root (which CI may run as
    // on some platforms) and by filesystems without permission
    // semantics. The existing Ok-path tests cover the happy path; the
    // writability probe is a simple write-then-remove which any std
    // lib regression would surface broadly.

    // ══════════════════════════════════════════════════════════════
    //  Render-device detection and association
    // ══════════════════════════════════════════════════════════════

    #[cfg(target_os = "linux")]
    fn device_test_args(encoder: &str, device: Option<&str>, pix_fmt: &str) -> Vec<String> {
        assemble_ffmpeg_args(&FfmpegAssemblyParts {
            input_path: "input.mkv",
            output_path: "output.mkv",
            video_args: &["-c:v", encoder],
            render_device: device,
            hardware_decode: false,
            source_format: None,
            pix_fmt,
            audio_map_args: &[],
            audio_codec_args: &[],
            is_image_source: false,
            threads: 0,
            tonemap_filter: None,
            codec_family: "hevc",
            colour_metadata: None,
            vaapi_hevc_bsf: None,
        })
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn preferred_gpu_is_verified_per_codec_and_reused_without_driver_overrides() {
        let sysfs = tempfile::tempdir().unwrap();
        let nodes = vec![
            "/test/dri/renderD134".to_string(),
            "/test/dri/renderD167".to_string(),
        ];
        for (name, vendor, width, vram) in [
            ("renderD134", "0x8086", "255", "0"),
            ("renderD167", "0x1002", "16", "17179869184"),
        ] {
            let device = sysfs.path().join(name).join("device");
            std::fs::create_dir_all(&device).unwrap();
            for (file, value) in [
                ("vendor", vendor),
                ("max_link_width", width),
                ("mem_info_vram_total", vram),
            ] {
                std::fs::write(device.join(file), value).unwrap();
            }
        }
        for (name, family, amd_works, intel_works, expected) in [
            ("hevc_vaapi", "hevc", true, true, Some(1)),
            ("h264_vaapi", "h264", false, true, Some(0)),
            ("av1_vaapi", "av1", false, false, None),
            ("hevc_qsv", "hevc", false, true, Some(0)),
        ] {
            let ranked = devices::preferred_render_nodes(name, &nodes, sysfs.path());
            let mut attempts = Vec::new();
            let verified = first_working_device(hardware_probe_devices(name, &ranked), |device| {
                attempts.push(device.clone());
                std::future::ready(if device.as_ref() == Some(&nodes[1]) {
                    amd_works
                } else {
                    intel_works
                })
            })
            .await;
            let mut encoders = Vec::new();
            if let Some(device) = verified {
                assert_eq!(device, expected.map(|i| nodes[i].clone()));
                encoders.push(EncoderInfo {
                    name: name.into(),
                    codec_family: family.into(),
                    is_hardware: true,
                    render_device: device,
                });
            } else {
                assert_eq!(expected, None);
            }
            ensure_fallback(&mut encoders, software_fallback(family), family);
            let mut settings = default_settings();
            settings.preserve_av1 = true;
            let resolved = resolve_file_settings(family, "mkv", &settings, &encoders);
            let device = encoder_render_device(&encoders, &resolved.encoder_name);
            if let Some(index) = expected {
                assert_eq!(device, Some(nodes[index].as_str()));
                let args = device_test_args(&resolved.encoder_name, device, "nv12");
                if is_qsv_encoder(name) {
                    assert!(args.contains(&format!("vaapi=va:{}", nodes[index])));
                } else {
                    assert!(args.contains(&nodes[index]));
                    assert!(!args.contains(&nodes[1 - index]));
                }
            } else {
                assert_eq!(resolved.encoder_name, software_fallback(family));
                assert!(device.is_none());
            }
            assert_eq!(
                attempts[0],
                Some(nodes[if is_qsv_encoder(name) { 0 } else { 1 }].clone())
            );
            assert_eq!(
                attempts.len(),
                if !is_qsv_encoder(name) && !amd_works {
                    2
                } else {
                    1
                }
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn probes_select_devices_per_encoder_and_encoding_reuses_them() {
        let nodes = vec![
            "/test/dri/renderD128".to_string(),
            "/test/dri/renderD129".to_string(),
        ];
        let mut encoders = Vec::new();
        for (name, family, supported) in [
            ("hevc_vaapi", "hevc", &nodes[1]),
            ("h264_vaapi", "h264", &nodes[0]),
            ("hevc_qsv", "hevc", &nodes[0]),
        ] {
            let mut attempts = Vec::new();
            let verified = first_working_device(hardware_probe_devices(name, &nodes), |device| {
                attempts.push(device.clone());
                std::future::ready(device.as_ref() == Some(supported))
            })
            .await
            .unwrap();
            assert_eq!(verified.as_ref(), Some(supported));
            assert_eq!(attempts.len(), if supported == &nodes[1] { 2 } else { 1 });
            encoders.push(EncoderInfo {
                name: name.into(),
                codec_family: family.into(),
                is_hardware: true,
                render_device: verified,
            });
        }
        let settings = default_settings();
        let resolved = resolve_file_settings("hevc", "mkv", &settings, &encoders);
        // Preserve the declared encoder priority instead of picking the last HW entry.
        assert_eq!(resolved.encoder_name, "hevc_vaapi");
        for encoder in &encoders {
            let device = encoder_render_device(&encoders, &encoder.name);
            let probe = test_encode_args(&encoder.name, device, "probe.mp4");
            for pix_fmt in ["nv12", "p010le"] {
                let args = device_test_args(&encoder.name, device, pix_fmt);
                if is_qsv_encoder(&encoder.name) {
                    let init = format!("vaapi=va:{}", device.unwrap());
                    assert!(probe.contains(&init));
                    assert!(args.contains(&init));
                    assert!(args.contains(&"qsv=qs@va".into()));
                } else {
                    assert!(probe.contains(&device.unwrap().into()));
                    assert!(args.contains(&device.unwrap().into()));
                }
            }
        }
        let mut forced = settings;
        forced.video_encoder = "hevc_qsv".into();
        let resolved = resolve_file_settings("hevc", "mkv", &forced, &encoders);
        assert_eq!(
            encoder_render_device(&encoders, &resolved.encoder_name),
            Some(nodes[0].as_str())
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn single_device_and_failed_or_unavailable_devices_fall_back() {
        let nodes = vec!["/test/dri/renderD128".to_string()];
        let devices = hardware_probe_devices("hevc_vaapi", &nodes);
        assert_eq!(
            first_working_device(devices.clone(), |_| std::future::ready(true)).await,
            Some(Some(nodes[0].clone()))
        );
        // Failed opens (including permissions), unsupported codecs, and no devices.
        assert_eq!(
            first_working_device(devices, |_| std::future::ready(false)).await,
            None
        );
        assert_eq!(
            first_working_device(hardware_probe_devices("hevc_qsv", &[]), |_| async {
                panic!("no device should be probed")
            })
            .await,
            None
        );
        let (encoders, _) = fallback_encoders();
        let resolved = resolve_file_settings("hevc", "mkv", &default_settings(), &encoders);
        assert_eq!(resolved.encoder_name, "libx265");
        assert!(encoder_render_device(&encoders, &resolved.encoder_name).is_none());
        let args = device_test_args("libx265", Some(&nodes[0]), "p010le");
        assert!(!args
            .iter()
            .any(|a| a.contains("hw_device") || a == "-vaapi_device" || a == &nodes[0]));
        // A stale verified path stays explicit, allowing FFmpeg to fail and
        // trigger the existing runtime fallback instead of switching GPUs.
        let args = device_test_args("hevc_vaapi", Some("/missing/renderD129"), "nv12");
        assert!(args.contains(&"/missing/renderD129".into()));
    }

    #[test]
    fn detection_uses_hd_frames_above_amd_minimum_width() {
        let args = test_encode_args("hevc_nvenc", None, "probe.mp4");
        let source = &args[args.iter().position(|a| a == "-i").unwrap() + 1];
        let size = source
            .split(':')
            .find_map(|s| s.strip_prefix("s="))
            .unwrap();
        let (width, height) = size.split_once('x').unwrap();
        let width: u32 = width.parse().unwrap();
        let height: u32 = height.parse().unwrap();
        assert!((384..=8192).contains(&width));
        assert!((128..=4352).contains(&height));
        assert_eq!((width, height), (1920, 1080));
    }

    #[tokio::test]
    async fn non_drm_backends_keep_default_device_detection() {
        let devices = hardware_probe_devices("hevc_nvenc", &["/test/dri/renderD129".into()]);
        assert_eq!(devices, vec![None]);
        assert_eq!(
            first_working_device(devices, |_| std::future::ready(true)).await,
            Some(None)
        );
    }

    #[test]
    fn encoder_info_accepts_old_json_and_round_trips_device() {
        let mut info: EncoderInfo =
            serde_json::from_str(r#"{"name":"hevc_vaapi","codecFamily":"hevc","isHardware":true}"#)
                .unwrap();
        assert!(info.render_device.is_none());
        info.render_device = Some("/test/dri/renderD129".into());
        let json = serde_json::to_string(&info).unwrap();
        let restored: EncoderInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.render_device, info.render_device);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn render_node_discovery_is_complete_and_sorted() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            "renderD130",
            "card0",
            "renderD129",
            "renderD128",
            "renderD",
            "renderDbad",
            "renderD128junk",
        ] {
            std::fs::File::create(dir.path().join(name)).unwrap();
        }
        std::fs::create_dir(dir.path().join("renderD131")).unwrap();
        let nodes = find_dri_render_nodes_in(dir.path());
        assert_eq!(
            nodes,
            [128, 129, 130].map(|n| dir
                .path()
                .join(format!("renderD{n}"))
                .to_str()
                .unwrap()
                .to_string())
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn render_node_discovery_handles_single_empty_and_missing_directories() {
        let dir = tempfile::tempdir().unwrap();
        assert!(find_dri_render_nodes_in(dir.path()).is_empty());
        assert!(find_dri_render_nodes_in(&dir.path().join("missing")).is_empty());
        std::fs::File::create(dir.path().join("renderD128")).unwrap();
        assert_eq!(find_dri_render_nodes_in(dir.path()).len(), 1);
    }

    // ══════════════════════════════════════════════════════════════
    //  hw_device_init / hw_upload_filter in assemble_ffmpeg_args
    // ══════════════════════════════════════════════════════════════

    /// QSV must derive its session from the selected device before `-i`,
    /// select that session for filtering, and upload frames in `-vf`.
    #[cfg(target_os = "linux")]
    #[test]
    fn test_assemble_qsv_emits_hw_device_init_and_upload() {
        let empty: &[String] = &[];
        let args = assemble_ffmpeg_args(&FfmpegAssemblyParts {
            input_path: "input.mkv",
            output_path: "output.mkv",
            video_args: &["-c:v", "hevc_qsv"],
            render_device: Some("/test/dri/renderD129"),
            hardware_decode: false,
            source_format: None,
            pix_fmt: "nv12",
            audio_map_args: empty,
            audio_codec_args: empty,
            is_image_source: false,
            threads: 0,
            tonemap_filter: None,
            codec_family: "hevc",
            colour_metadata: None,
            vaapi_hevc_bsf: None,
        });

        // hw_device_init args must precede -i
        let hw_init_pos = args.iter().position(|a| a == "-init_hw_device").unwrap();
        let input_pos = args.iter().position(|a| a == "-i").unwrap();
        assert!(
            hw_init_pos < input_pos,
            "-init_hw_device must come before -i"
        );

        // Verify full QSV init chain
        assert_eq!(args[hw_init_pos + 1], "vaapi=va:/test/dri/renderD129");
        assert!(args.iter().any(|a| a == "qsv=qs@va"));
        let filter_hw_pos = args.iter().position(|a| a == "-filter_hw_device").unwrap();
        assert_eq!(args[filter_hw_pos + 1], "qs");

        // hwupload filter must be present in -vf
        let vf_pos = args.iter().position(|a| a == "-vf").unwrap();
        assert!(args[vf_pos + 1].contains("hwupload=extra_hw_frames=64"));
    }

    /// QSV must always emit `-init_hw_device`. When no render node exists the
    /// value is a sentinel so ffmpeg fails loudly rather than silently.
    #[cfg(target_os = "linux")]
    #[test]
    fn test_assemble_qsv_missing_render_node_emits_sentinel() {
        let empty: &[String] = &[];
        let args = assemble_ffmpeg_args(&FfmpegAssemblyParts {
            input_path: "input.mkv",
            output_path: "output.mkv",
            video_args: &["-c:v", "hevc_qsv"],
            render_device: None,
            hardware_decode: false,
            source_format: None,
            pix_fmt: "nv12",
            audio_map_args: empty,
            audio_codec_args: empty,
            is_image_source: false,
            threads: 0,
            tonemap_filter: None,
            codec_family: "hevc",
            colour_metadata: None,
            vaapi_hevc_bsf: None,
        });
        assert!(
            args.iter().any(|a| a == "-init_hw_device"),
            "QSV encode must always include -init_hw_device"
        );
        let hw_init_pos = args.iter().position(|a| a == "-init_hw_device").unwrap();
        let init_value = &args[hw_init_pos + 1];
        assert_eq!(init_value, "vaapi=va:/dev/dri/renderD_missing");
    }

    /// VAAPI encoder must emit `-vaapi_device` before `-i` and
    /// `format=nv12,hwupload` as `-vf`, using an injected device path.
    #[cfg(target_os = "linux")]
    #[test]
    fn test_assemble_vaapi_emits_hw_device_init_and_upload() {
        let empty: &[String] = &[];
        let args = assemble_ffmpeg_args(&FfmpegAssemblyParts {
            input_path: "input.mkv",
            output_path: "output.mkv",
            video_args: &["-c:v", "hevc_vaapi"],
            render_device: Some("/test/dri/renderD129"),
            hardware_decode: false,
            source_format: None,
            pix_fmt: "nv12",
            audio_map_args: empty,
            audio_codec_args: empty,
            is_image_source: false,
            threads: 0,
            tonemap_filter: None,
            codec_family: "hevc",
            colour_metadata: None,
            vaapi_hevc_bsf: None,
        });

        // -vaapi_device must precede -i
        let vaapi_dev_pos = args.iter().position(|a| a == "-vaapi_device").unwrap();
        let input_pos = args.iter().position(|a| a == "-i").unwrap();
        assert!(
            vaapi_dev_pos < input_pos,
            "-vaapi_device must come before -i"
        );

        // hwupload filter in -vf
        let vf_pos = args.iter().position(|a| a == "-vf").unwrap();
        assert!(args[vf_pos + 1].contains("hwupload"));
    }

    /// When no render node exists for VAAPI, the args must still
    /// contain `-vaapi_device` (with a clearly-missing path) so
    /// ffmpeg fails fast rather than silently producing broken output.
    #[cfg(target_os = "linux")]
    #[test]
    fn test_assemble_vaapi_missing_render_node_still_emits_vaapi_device() {
        // Missing verification must never silently select a default GPU.
        let empty: &[String] = &[];
        let args = assemble_ffmpeg_args(&FfmpegAssemblyParts {
            input_path: "input.mkv",
            output_path: "output.mkv",
            video_args: &["-c:v", "hevc_vaapi"],
            render_device: None,
            hardware_decode: false,
            source_format: None,
            pix_fmt: "nv12",
            audio_map_args: empty,
            audio_codec_args: empty,
            is_image_source: false,
            threads: 0,
            tonemap_filter: None,
            codec_family: "hevc",
            colour_metadata: None,
            vaapi_hevc_bsf: None,
        });

        // VAAPI must always have -vaapi_device in args. If the render
        // node is missing, we emit a sentinel path so ffmpeg fails
        // loudly instead of silently producing broken output.
        assert!(
            args.iter().any(|a| a == "-vaapi_device"),
            "VAAPI encode must always include -vaapi_device (even with missing render node)"
        );
    }

    /// VAAPI with 10-bit + tonemap: tonemap forces frames back to CPU,
    /// so hwaccel decode can't be used. Falls back to -vaapi_device +
    /// format=nv12,hwupload (tonemap output is 8-bit yuv420p anyway).
    #[cfg(target_os = "linux")]
    #[test]
    fn test_assemble_vaapi_10bit_tonemap_uses_vaapi_device() {
        let empty: &[String] = &[];
        let args = assemble_ffmpeg_args(&FfmpegAssemblyParts {
            input_path: "input.mkv",
            output_path: "output.mkv",
            video_args: &["-c:v", "hevc_vaapi"],
            render_device: Some("/test/dri/renderD129"),
            hardware_decode: false,
            source_format: None,
            pix_fmt: "p010le",
            audio_map_args: empty,
            audio_codec_args: empty,
            is_image_source: false,
            threads: 0,
            tonemap_filter: Some(TONEMAP_HABLE),
            codec_family: "hevc",
            colour_metadata: None,
            vaapi_hevc_bsf: None,
        });

        // When tonemap is present, can't use hwaccel decode (tonemap is CPU)
        assert!(
            !args.iter().any(|a| a == "-hwaccel"),
            "VAAPI with tonemap must not use hwaccel decode"
        );
        // Must use -vaapi_device instead
        assert!(
            args.iter().any(|a| a == "-vaapi_device"),
            "VAAPI with tonemap must use -vaapi_device"
        );
        // Must have hwupload after tonemap
        let vf_pos = args.iter().position(|a| a == "-vf").unwrap();
        let vf_value = &args[vf_pos + 1];
        assert!(vf_value.contains("hwupload"));
        assert!(vf_value.contains("tonemap"));
    }

    /// Software encoder must NOT emit any hw_device_init or hwupload.
    #[test]
    fn test_assemble_software_encoder_no_hw_init() {
        let empty: &[String] = &[];
        let args = assemble_ffmpeg_args(&FfmpegAssemblyParts {
            input_path: "input.mkv",
            output_path: "output.mkv",
            video_args: &["-c:v", "libx265"],
            render_device: None,
            hardware_decode: false,
            source_format: None,
            pix_fmt: "yuv420p",
            audio_map_args: empty,
            audio_codec_args: empty,
            is_image_source: false,
            threads: 0,
            tonemap_filter: None,
            codec_family: "hevc",
            colour_metadata: None,
            vaapi_hevc_bsf: None,
        });

        assert!(!args.iter().any(|a| a == "-vaapi_device"));
        assert!(!args.iter().any(|a| a == "-init_hw_device"));
        assert!(!args.iter().any(|a| a == "-filter_hw_device"));
        assert!(!args.iter().any(|a| a == "hwupload"));
    }

    /// NVENC must NOT emit hw_device_init (it auto-detects) and
    /// must NOT emit hwupload filter.
    #[test]
    fn test_assemble_nvenc_no_hw_init() {
        let empty: &[String] = &[];
        let args = assemble_ffmpeg_args(&FfmpegAssemblyParts {
            input_path: "input.mkv",
            output_path: "output.mkv",
            video_args: &["-c:v", "hevc_nvenc"],
            render_device: None,
            hardware_decode: false,
            source_format: None,
            pix_fmt: "yuv420p",
            audio_map_args: empty,
            audio_codec_args: empty,
            is_image_source: false,
            threads: 0,
            tonemap_filter: None,
            codec_family: "hevc",
            colour_metadata: None,
            vaapi_hevc_bsf: None,
        });

        assert!(!args.iter().any(|a| a == "-vaapi_device"));
        assert!(!args.iter().any(|a| a == "-init_hw_device"));
        assert!(!args.iter().any(|a| a == "hwupload"));
    }

    /// QSV encoder with tonemap: the hwupload filter must be chained
    /// after the tonemap filter, not replace it. QSV hwupload is emitted
    /// only by the Linux `assemble_ffmpeg_args` branch.
    #[cfg(target_os = "linux")]
    #[test]
    fn test_assemble_qsv_tonemap_chains_hwupload() {
        let empty: &[String] = &[];
        let args = assemble_ffmpeg_args(&FfmpegAssemblyParts {
            input_path: "input.mkv",
            output_path: "output.mkv",
            video_args: &["-c:v", "hevc_qsv"],
            render_device: Some("/test/dri/renderD129"),
            hardware_decode: false,
            source_format: None,
            pix_fmt: "nv12",
            audio_map_args: empty,
            audio_codec_args: empty,
            is_image_source: false,
            threads: 0,
            tonemap_filter: Some(TONEMAP_HABLE),
            codec_family: "hevc",
            colour_metadata: None,
            vaapi_hevc_bsf: None,
        });

        let vf_pos = args.iter().position(|a| a == "-vf").unwrap();
        let vf_value = &args[vf_pos + 1];

        assert!(
            vf_value.contains("tonemap=hable"),
            "tonemap must be present"
        );
        assert!(vf_value.contains("hwupload"), "hwupload must be present");
        let tonemap_idx = vf_value.find("tonemap").unwrap();
        let hwupload_idx = vf_value.find("hwupload").unwrap();
        assert!(
            tonemap_idx < hwupload_idx,
            "tonemap must come before hwupload so it runs on CPU first"
        );
    }

    /// VAAPI encoder with tonemap: the hwupload filter must be
    /// chained after the tonemap filter.
    #[cfg(target_os = "linux")]
    #[test]
    fn test_assemble_vaapi_tonemap_chains_hwupload() {
        let empty: &[String] = &[];
        let args = assemble_ffmpeg_args(&FfmpegAssemblyParts {
            input_path: "input.mkv",
            output_path: "output.mkv",
            video_args: &["-c:v", "hevc_vaapi"],
            render_device: Some("/test/dri/renderD129"),
            hardware_decode: false,
            source_format: None,
            pix_fmt: "nv12",
            audio_map_args: empty,
            audio_codec_args: empty,
            is_image_source: false,
            threads: 0,
            tonemap_filter: Some(TONEMAP_HABLE),
            codec_family: "hevc",
            colour_metadata: None,
            vaapi_hevc_bsf: None,
        });

        let vf_pos = args.iter().position(|a| a == "-vf").unwrap();
        let vf_value = &args[vf_pos + 1];

        assert!(vf_value.contains("tonemap=hable"));
        assert!(vf_value.contains("hwupload"));
        let tonemap_idx = vf_value.find("tonemap").unwrap();
        let hwupload_idx = vf_value.find("hwupload").unwrap();
        assert!(tonemap_idx < hwupload_idx);
    }

    // ══════════════════════════════════════════════════════════════
    //  10-bit (HDR/DV) hwupload format: p010le instead of nv12
    // ══════════════════════════════════════════════════════════════

    /// With optional GPU decoding enabled for a 10-bit source, the args must
    /// contain -hwaccel vaapi, -hwaccel_device, and -hwaccel_output_format,
    /// must use scale_vaapi=format=p010 to force P010 surface format, and must
    /// NOT contain hwupload or -pix_fmt.
    #[cfg(target_os = "linux")]
    #[test]
    fn test_assemble_vaapi_10bit_uses_hwaccel_decode() {
        let empty: &[String] = &[];
        let args = assemble_ffmpeg_args(&FfmpegAssemblyParts {
            input_path: "input.mkv",
            output_path: "output.mkv",
            video_args: &["-c:v", "hevc_vaapi"],
            render_device: Some("/test/dri/renderD129"),
            hardware_decode: true,
            source_format: None,
            pix_fmt: "p010le",
            audio_map_args: empty,
            audio_codec_args: empty,
            is_image_source: false,
            threads: 0,
            tonemap_filter: None,
            codec_family: "hevc",
            colour_metadata: None,
            vaapi_hevc_bsf: None,
        });

        // Must use hwaccel decode for 10-bit VAAPI
        let hwaccel_pos = args.iter().position(|a| a == "-hwaccel").unwrap();
        assert_eq!(args[hwaccel_pos + 1], "vaapi");
        let hwaccel_dev_pos = args.iter().position(|a| a == "-hwaccel_device").unwrap();
        assert!(args[hwaccel_dev_pos + 1].contains("renderD"));
        let hwaccel_fmt_pos = args
            .iter()
            .position(|a| a == "-hwaccel_output_format")
            .unwrap();
        assert_eq!(args[hwaccel_fmt_pos + 1], "vaapi");

        // Must use scale_vaapi to force P010 surface format
        let vf_pos = args.iter().position(|a| a == "-vf").unwrap();
        assert!(
            args[vf_pos + 1].contains("scale_vaapi=format=p010"),
            "10-bit VAAPI hwaccel must use scale_vaapi=format=p010, got: {}",
            args[vf_pos + 1]
        );

        // Must NOT use hwupload or -pix_fmt (encoder auto-negotiates)
        assert!(
            !args.iter().any(|a| a == "hwupload"),
            "10-bit VAAPI hwaccel must not use hwupload"
        );
        assert!(
            !args.iter().any(|a| a == "-pix_fmt"),
            "10-bit VAAPI hwaccel must not set -pix_fmt (encoder auto-negotiates)"
        );

        // Must NOT use old -vaapi_device approach
        assert!(
            !args.iter().any(|a| a == "-vaapi_device"),
            "10-bit VAAPI must use -hwaccel vaapi, not -vaapi_device"
        );
    }

    /// VAAPI with 8-bit SDR source must use format=nv12,hwupload.
    #[cfg(target_os = "linux")]
    #[test]
    fn test_assemble_vaapi_8bit_uses_nv12() {
        let empty: &[String] = &[];
        let args = assemble_ffmpeg_args(&FfmpegAssemblyParts {
            input_path: "input.mkv",
            output_path: "output.mkv",
            video_args: &["-c:v", "hevc_vaapi"],
            render_device: Some("/test/dri/renderD129"),
            hardware_decode: false,
            source_format: None,
            pix_fmt: "nv12",
            audio_map_args: empty,
            audio_codec_args: empty,
            is_image_source: false,
            threads: 0,
            tonemap_filter: None,
            codec_family: "hevc",
            colour_metadata: None,
            vaapi_hevc_bsf: None,
        });

        let vf_pos = args.iter().position(|a| a == "-vf").unwrap();
        let vf_value = &args[vf_pos + 1];
        assert!(
            vf_value.contains("nv12"),
            "8-bit VAAPI must use format=nv12,hwupload, got: {vf_value}"
        );
    }

    /// QSV with 10-bit HDR/DV source must use format=p010le,hwupload.
    /// QSV args are emitted only by the Linux branch of
    /// `assemble_ffmpeg_args`; the device is injected for testing.
    #[cfg(target_os = "linux")]
    #[test]
    fn test_assemble_qsv_10bit_uses_p010le() {
        let empty: &[String] = &[];
        let args = assemble_ffmpeg_args(&FfmpegAssemblyParts {
            input_path: "input.mkv",
            output_path: "output.mkv",
            video_args: &["-c:v", "hevc_qsv"],
            render_device: Some("/test/dri/renderD129"),
            hardware_decode: false,
            source_format: None,
            pix_fmt: "p010le",
            audio_map_args: empty,
            audio_codec_args: empty,
            is_image_source: false,
            threads: 0,
            tonemap_filter: None,
            codec_family: "hevc",
            colour_metadata: None,
            vaapi_hevc_bsf: None,
        });

        let vf_pos = args.iter().position(|a| a == "-vf").unwrap();
        let vf_value = &args[vf_pos + 1];
        assert!(
            vf_value.contains("p010le"),
            "10-bit QSV must use format=p010le,hwupload, got: {vf_value}"
        );
        assert!(
            !vf_value.contains("nv12"),
            "10-bit QSV must not use format=nv12, got: {vf_value}"
        );
    }

    /// QSV with 8-bit SDR source must use format=nv12,hwupload.
    /// QSV args are emitted only by the Linux branch of
    /// `assemble_ffmpeg_args`; the device is injected for testing.
    #[cfg(target_os = "linux")]
    #[test]
    fn test_assemble_qsv_8bit_uses_nv12() {
        let empty: &[String] = &[];
        let args = assemble_ffmpeg_args(&FfmpegAssemblyParts {
            input_path: "input.mkv",
            output_path: "output.mkv",
            video_args: &["-c:v", "hevc_qsv"],
            render_device: Some("/test/dri/renderD129"),
            hardware_decode: false,
            source_format: None,
            pix_fmt: "nv12",
            audio_map_args: empty,
            audio_codec_args: empty,
            is_image_source: false,
            threads: 0,
            tonemap_filter: None,
            codec_family: "hevc",
            colour_metadata: None,
            vaapi_hevc_bsf: None,
        });

        let vf_pos = args.iter().position(|a| a == "-vf").unwrap();
        let vf_value = &args[vf_pos + 1];
        assert!(
            vf_value.contains("nv12"),
            "8-bit QSV must use format=nv12,hwupload, got: {vf_value}"
        );
    }

    // ── Colour metadata tests ─────────────────────────────────────

    /// Colour metadata flags must appear when VAAPI hwaccel decode is
    /// used (10-bit, no tonemap) because hwaccel decode does not
    /// propagate colour primaries/transfer/matrix from the input.
    #[cfg(target_os = "linux")]
    #[test]
    fn test_colour_metadata_present_for_vaapi_hwaccel_decode() {
        let empty: &[String] = &[];
        let args = assemble_ffmpeg_args(&FfmpegAssemblyParts {
            input_path: "input.mkv",
            output_path: "output.mp4",
            video_args: &["-c:v", "hevc_vaapi"],
            render_device: Some("/test/dri/renderD129"),
            hardware_decode: true,
            source_format: None,
            pix_fmt: "p010le",
            audio_map_args: empty,
            audio_codec_args: empty,
            is_image_source: false,
            threads: 0,
            tonemap_filter: None,
            codec_family: "hevc",
            colour_metadata: Some(ColourMetadata {
                primaries: "bt2020",
                trc: "smpte2084",
                matrix: "bt2020nc",
                range: "tv",
            }),
            vaapi_hevc_bsf: None,
        });

        assert!(
            args.iter().any(|a| a == "-color_primaries"),
            "colour metadata must include -color_primaries for VAAPI hwaccel decode"
        );
        assert!(
            args.iter().any(|a| a == "-color_trc"),
            "colour metadata must include -color_trc for VAAPI hwaccel decode"
        );
        // Colour flags must come before the output path
        let colour_pos = args.iter().position(|a| a == "-color_primaries").unwrap();
        let output_pos = args.iter().rposition(|a| a == "output.mp4").unwrap();
        assert!(
            colour_pos < output_pos,
            "colour metadata must come before output path"
        );
    }

    /// Software encode (libx265) should NOT have explicit colour
    /// metadata flags. ffmpeg propagates colour info from the input
    /// automatically; explicit flags cause haloing around text on PQ
    /// content.
    #[test]
    fn test_colour_metadata_absent_for_software_encode() {
        let empty: &[String] = &[];
        let args = assemble_ffmpeg_args(&FfmpegAssemblyParts {
            input_path: "input.mkv",
            output_path: "output.mp4",
            video_args: &["-c:v", "libx265"],
            render_device: None,
            hardware_decode: false,
            source_format: None,
            pix_fmt: "p010le",
            audio_map_args: empty,
            audio_codec_args: empty,
            is_image_source: false,
            threads: 0,
            tonemap_filter: None,
            codec_family: "hevc",
            colour_metadata: None,
            vaapi_hevc_bsf: None,
        });

        assert!(
            !args.iter().any(|a| a == "-color_primaries"),
            "software encode must not have explicit colour metadata flags"
        );
        assert!(
            !args.iter().any(|a| a == "-color_trc"),
            "software encode must not have explicit colour metadata flags"
        );
        assert!(
            !args.iter().any(|a| a == "-color_range"),
            "software encode must not have explicit colour metadata flags"
        );
    }

    /// 8-bit VAAPI uses hwupload (not hwaccel decode), so colour
    /// metadata should not be needed even for VAAPI.
    #[cfg(target_os = "linux")]
    #[test]
    fn test_colour_metadata_absent_for_vaapi_8bit() {
        let empty: &[String] = &[];
        let args = assemble_ffmpeg_args(&FfmpegAssemblyParts {
            input_path: "input.mkv",
            output_path: "output.mp4",
            video_args: &["-c:v", "hevc_vaapi"],
            render_device: Some("/test/dri/renderD129"),
            hardware_decode: false,
            source_format: None,
            pix_fmt: "nv12",
            audio_map_args: empty,
            audio_codec_args: empty,
            is_image_source: false,
            threads: 0,
            tonemap_filter: None,
            codec_family: "hevc",
            colour_metadata: None,
            vaapi_hevc_bsf: None,
        });

        assert!(
            !args.iter().any(|a| a == "-color_primaries"),
            "8-bit VAAPI uses hwupload, not hwaccel decode - no colour metadata needed"
        );
    }

    /// VAAPI 10-bit HEVC encode must include the conformance window crop BSF
    /// to fix the 1088-line green bar on AMD VCN.
    #[cfg(target_os = "linux")]
    #[test]
    fn test_vaapi_hevc_bsf_crop_for_10bit() {
        let empty: &[String] = &[];
        let args = assemble_ffmpeg_args(&FfmpegAssemblyParts {
            input_path: "input.mkv",
            output_path: "output.mp4",
            video_args: &["-c:v", "hevc_vaapi"],
            render_device: Some("/test/dri/renderD129"),
            hardware_decode: false,
            source_format: None,
            pix_fmt: "p010le",
            audio_map_args: empty,
            audio_codec_args: empty,
            is_image_source: false,
            threads: 0,
            tonemap_filter: None,
            codec_family: "hevc",
            colour_metadata: None,
            vaapi_hevc_bsf: Some("hevc_metadata=crop_bottom=8"),
        });

        assert!(
            args.iter().any(|a| a == "-bsf:v"),
            "VAAPI 10-bit HEVC must include -bsf:v for conformance window crop"
        );
        assert!(
            args.windows(2)
                .any(|w| w[0] == "-bsf:v" && w[1].contains("crop_bottom=8")),
            "VAAPI 10-bit HEVC BSF must include crop_bottom=8"
        );
    }

    /// Non-VAAPI encoders must NOT include the VAAPI crop BSF.
    #[test]
    fn test_no_vaapi_bsf_for_software_encode() {
        let empty: &[String] = &[];
        let args = assemble_ffmpeg_args(&FfmpegAssemblyParts {
            input_path: "input.mkv",
            output_path: "output.mp4",
            video_args: &["-c:v", "libx265"],
            render_device: None,
            hardware_decode: false,
            source_format: None,
            pix_fmt: "p010le",
            audio_map_args: empty,
            audio_codec_args: empty,
            is_image_source: false,
            threads: 0,
            tonemap_filter: None,
            codec_family: "hevc",
            colour_metadata: None,
            vaapi_hevc_bsf: None,
        });

        assert!(
            !args.iter().any(|a| a == "-bsf:v"),
            "software encode must not include VAAPI crop BSF"
        );
    }
}

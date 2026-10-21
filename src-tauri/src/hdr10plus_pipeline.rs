//! HDR10+ dynamic metadata extract/inject pipeline.
//!
//! Opt-in flow:
//! 1. Pre-encode: demux source to raw HEVC, extract HDR10+ SEI metadata
//! 2. Normal ffmpeg encode (unchanged)
//! 3. Post-encode: demux encoded output, inject HDR10+ SEI NALUs back
//!
//! All bitstream processing uses streaming I/O via `hevc_utils::NalReader`
//! to avoid loading multi-gigabyte files into memory.

use hdr10plus::metadata::Hdr10PlusMetadata;

use std::path::Path;

use crate::events::EventSink;
use crate::ffmpeg as ffbin;
use crate::hevc_utils;

/// Original HDR10+ SEI NAL units indexed by presentation frame.
pub struct ExtractedHdr10Plus {
    /// All metadata NALs per frame, including empty frames in sparse streams.
    pub frames: Vec<Vec<Vec<u8>>>,
}

/// Result of HDR10+ injection.
pub struct Hdr10PlusPipelineResult {
    pub success: bool,
    pub message: String,
}

// ── HDR10+ SEI Constants ──────────────────────────────────────────

/// ITU-T T.35 country code for HDR10+ (United States)
const ITU_T35_COUNTRY_CODE_US: u8 = 0xB5;
/// Samsung HDR10+ provider code
const HDR10PLUS_PROVIDER_CODE: u16 = 0x003C;

// ── Extraction ────────────────────────────────────────────────────

/// Extract HDR10+ dynamic metadata from a source video file.
///
/// Demuxes to raw HEVC via ffmpeg, then streams through NAL units looking
/// for SEI_PREFIX NALs containing HDR10+ payloads. Only the small metadata
/// payloads are held in memory.
pub async fn extract_hdr10plus(
    source_path: &Path,
    sink: &dyn EventSink,
) -> Result<ExtractedHdr10Plus, String> {
    extract_hdr10plus_if_present(source_path, sink)
        .await?
        .ok_or_else(|| "No HDR10+ metadata found in bitstream".into())
}

pub async fn extract_hdr10plus_if_present(
    source_path: &Path,
    sink: &dyn EventSink,
) -> Result<Option<ExtractedHdr10Plus>, String> {
    extract_hdr10plus_with_stage(source_path, sink, "preparing").await
}

pub async fn extract_hdr10plus_with_stage(
    source_path: &Path,
    sink: &dyn EventSink,
    stage: &str,
) -> Result<Option<ExtractedHdr10Plus>, String> {
    extract_hdr10plus_in(source_path, sink, stage, source_path.parent()).await
}

pub async fn extract_hdr10plus_in(
    source_path: &Path,
    sink: &dyn EventSink,
    stage: &str,
    near: Option<&Path>,
) -> Result<Option<ExtractedHdr10Plus>, String> {
    let temp_dir = hevc_utils::make_tempdir(".histv-hdr10plus-", near)?;
    let raw_hevc = temp_dir.path().join("source.h265");

    sink.processing_stage(stage, "Extracting HDR10+ video bitstream...", None);
    sink.log("  HDR10+: Extracting HEVC bitstream from source...");
    hevc_utils::demux_to_annexb(source_path, &raw_hevc, sink, "HDR10+").await?;

    // Stream through the bitstream, extracting HDR10+ payloads from SEI NALs
    sink.log("  HDR10+: Scanning for dynamic metadata (streaming)...");
    sink.processing_stage(stage, "Scanning HDR10+ dynamic metadata...", None);
    let frames = crate::metadata_order::frame_nalus(
        &raw_hevc,
        false,
        |n| {
            n.nal_type == hevc_utils::HEVC_NAL_SEI_PREFIX
                && parse_hdr10plus_from_sei_nalu(&n.data).is_some()
        },
        sink,
        stage,
        "Scanning HDR10+ dynamic metadata...",
    )?;
    if frames.iter().all(|f| f.is_empty()) {
        return Ok(None);
    }
    let order = crate::metadata_order::packet_order(source_path, frames.len(), sink).await?;
    let frames = crate::metadata_order::to_presentation(frames, &order);
    let mut count = 0;
    for (index, frame) in frames.iter().enumerate() {
        crate::pipeline_progress::frames(
            sink,
            stage,
            "Preparing HDR10+ metadata for reinsertion...",
            index,
            frames.len(),
        )
        .map_err(|e| e.to_string())?;
        for nalu in frame {
            let payload = parse_hdr10plus_from_sei_nalu(nalu).ok_or("Invalid HDR10+ SEI")?;
            Hdr10PlusMetadata::parse(&payload)
                .map_err(|e| format!("Invalid HDR10+ metadata: {e}"))?;
            count += 1;
        }
    }
    if count == 0 {
        return Err("No HDR10+ metadata found in bitstream".into());
    }
    sink.log(&format!(
        "  HDR10+: Prepared {count} original SEI NALUs without rewriting metadata"
    ));
    crate::pipeline_progress::frames(
        sink,
        stage,
        "Preparing HDR10+ metadata for reinsertion...",
        frames.len(),
        frames.len(),
    )
    .map_err(|e| e.to_string())?;
    Ok(Some(ExtractedHdr10Plus { frames }))
}

/// Inject HDR10+ metadata back into an encoded HEVC file.
///
/// Streams through the encoded bitstream, inserting SEI_PREFIX NALs
/// before the first VCL NAL of each new picture (detected via
/// `first_slice_segment_in_pic_flag`). Then remuxes with the original
/// audio/subtitle tracks.
pub async fn inject_hdr10plus(
    encoded_path: &Path,
    output_path: &Path,
    metadata: &ExtractedHdr10Plus,
    sink: &dyn EventSink,
) -> Result<Hdr10PlusPipelineResult, String> {
    let temp_dir = hevc_utils::make_tempdir(".histv-hdr10plus-inject-", output_path.parent())?;

    // Step 1: Demux encoded output to raw HEVC
    let encoded_hevc = temp_dir.path().join("encoded.h265");
    sink.processing_stage(
        "reinserting",
        "Extracting encoded bitstream for HDR10+ reinsertion...",
        None,
    );
    sink.log("  HDR10+: Extracting encoded HEVC bitstream...");
    hevc_utils::demux_to_annexb(encoded_path, &encoded_hevc, sink, "HDR10+ inject").await?;

    // SEI NALUs were pre-encoded at extraction time — use directly
    let sei_nalus = &metadata.frames;
    let timeline =
        crate::metadata_order::packet_timeline(encoded_path, sei_nalus.len(), sink).await?;
    let order = &timeline.rank;
    if sei_nalus.is_empty() {
        return Err("No pre-encoded HDR10+ SEI NALUs available".to_string());
    }

    // Step 2: Stream through encoded bitstream, inject SEI before each new picture
    sink.processing_stage("reinserting", "Reinserting HDR10+ metadata...", None);
    sink.log("  HDR10+: Injecting dynamic metadata (streaming)...");
    let injected_hevc = temp_dir.path().join("injected.h265");
    let mut sei_index: usize = 0;
    let sei_ref = sei_nalus;

    let mut nal_diag = hevc_utils::NalDiagnostics::new();

    if let Err(e) =
        hevc_utils::transform_bitstream(&encoded_hevc, &injected_hevc, |nalu, writer| {
            crate::pipeline_progress::check(sink)?;
            nal_diag.observe(nalu, false);

            // Encoder B-frame order may differ from the source; use PTS ranks.
            if nalu.is_first_slice_of_picture() {
                if sei_index >= order.len() {
                    return Err(std::io::Error::other("Extra encoded pictures"));
                }
                for sei in &sei_ref[order[sei_index]] {
                    writer.write_nalu(sei)?;
                }
                sei_index += 1;
            }
            if nalu.nal_type == hevc_utils::HEVC_NAL_SEI_PREFIX
                && parse_hdr10plus_from_sei_nalu(&nalu.data).is_some()
            {
                return Ok(());
            }

            writer.write_nalu(&nalu.data)?;
            Ok(())
        })
    {
        return Err(format!("HDR10+ injection failed: {e}"));
    }

    let _ = std::fs::remove_file(&encoded_hevc);

    sink.log(&format!(
        "  HDR10+: Injected {} SEIs ({} NALUs: {} first-slice)",
        sei_index,
        nal_diag.total(),
        nal_diag.first_slice_count(),
    ));
    sink.log(&format!("  HDR10+: NAL types: {}", nal_diag.type_summary()));

    if sei_index != sei_nalus.len() {
        return Err(format!(
            "Metadata count mismatch: {} SEI frames available but only {} pictures in encoded output. \
             HDR10+ metadata would be incomplete; output rejected.",
            sei_nalus.len(), sei_index,
        ));
    }

    // Step 3: Remux the injected bitstream with audio/subs from encoded output
    sink.log("  HDR10+: Remuxing with injected metadata...");
    sink.processing_stage(
        "reinserting",
        "Remuxing video with HDR10+ metadata...",
        None,
    );
    let probe = crate::probe::probe_file(&encoded_path.to_string_lossy(), sink).await?;
    let fps = probe.video_fps.to_string();
    // Raw FFmpeg HEVC demuxing cannot reliably reconstruct B-frame PTS.
    // GPAC reconstructs composition offsets from picture order before remux.
    if mp4box_path().is_err() {
        return Err("HDR10+ timing preservation requires a working MP4Box; source retained".into());
    }
    let video_mp4 = temp_dir.path().join("hdr10plus-video.mp4");
    let child = mp4box_command()?
        .kill_on_drop(true)
        .args([
            "-add",
            &format!("{}#video:fps={fps}", injected_hevc.display()),
            "-new",
            &video_mp4.to_string_lossy(),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let packaged = crate::pipeline_progress::wait_output(child, sink).await?;
    if !packaged.status.success() {
        return Err(format!(
            "HDR10+ video packaging failed: {}",
            String::from_utf8_lossy(&packaged.stderr)
        ));
    }
    let mut remux = ffbin::ffmpeg_command();
    remux.kill_on_drop(true).args([
        "-y",
        "-copyts",
        "-itsoffset",
        &timeline.first_pts.to_string(),
        "-i",
        &video_mp4.to_string_lossy(),
        "-i",
        &encoded_path.to_string_lossy(),
        "-map",
        "0:v:0",
        "-map",
        "1:a?",
        "-map",
        "1:s?",
        "-map_metadata",
        "1",
        "-map_chapters",
        "1",
        "-c",
        "copy",
        "-strict",
        "unofficial",
    ]);
    if output_path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("mkv"))
    {
        // Sparse subtitles in input 1 can advance its audio far ahead of
        // input 0's video. The default 10 s timeout then flushes future audio
        // before earlier video, overflowing player demux queues. Wait for
        // proper DTS interleaving instead; do not rewrite either timeline.
        // This can buffer packets until the next sparse subtitle or EOF.
        remux.args(["-max_interleave_delta", "0"]);
    }
    let child = remux
        .arg(output_path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to launch ffmpeg for remux: {e}"))?;
    let remux_output = crate::pipeline_progress::wait_output(child, sink).await?;

    drop(temp_dir); // clean up temp files

    if !remux_output.status.success() {
        let stderr = String::from_utf8_lossy(&remux_output.stderr);
        sink.log(&format!("  HDR10+ remux stderr: {stderr}"));
        return Ok(Hdr10PlusPipelineResult {
            success: false,
            message: "HDR10+ remux failed; output rejected".to_string(),
        });
    }

    sink.log(&format!(
        "  HDR10+: Preserved {} dynamic metadata frames",
        sei_index
    ));

    Ok(Hdr10PlusPipelineResult {
        success: true,
        message: format!("HDR10+ preserved ({} frames)", sei_index),
    })
}

/// Runtime lookup occurs only for explicitly requested metadata preservation.
fn mp4box_path() -> Result<std::path::PathBuf, String> {
    let name = if cfg!(windows) {
        "MP4Box.exe"
    } else {
        "MP4Box"
    };
    let mut candidates = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            candidates.push(parent.join(name));
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        candidates.extend(std::env::split_paths(&path).map(|p| p.join(name)));
    }
    candidates.into_iter().find(|p| p.is_file())
        .ok_or_else(|| "HDR10+ preservation needs MP4Box (included in the AppImage). Disable Preserve HDR10+ metadata for fast HDR10 output.".into())
}

fn mp4box_command() -> Result<tokio::process::Command, String> {
    let mut command = tokio::process::Command::new(mp4box_path()?);
    crate::ffmpeg::hide_window(&mut command);
    Ok(command)
}

pub fn preservation_requested(enabled: bool, source_codec: &str, bit_depth: u8) -> bool {
    enabled && source_codec == "hevc" && bit_depth == 10
}

/// Expensive work is gated by the setting before this function is called.
pub async fn prepare(
    source: &Path,
    near: Option<&Path>,
    codec: &str,
    sink: &dyn EventSink,
) -> Result<Option<ExtractedHdr10Plus>, String> {
    // A missing packager should fail immediately, not after an expensive
    // HEVC extraction and HDR10+ metadata scan.
    if codec != "hevc" {
        return Err("Preserving HDR10+ requires HEVC output. Select HEVC or disable Preserve HDR10+ metadata.".into());
    }
    let child = mp4box_command()?
        .arg("-version")
        .kill_on_drop(true)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("Cannot start HDR10+ packaging tool: {e}"))?;
    if !crate::pipeline_progress::wait_output(child, sink)
        .await?
        .status
        .success()
    {
        return Err("HDR10+ packaging tool is unavailable; source retained".into());
    }
    let result = crate::pipeline_progress::metadata_work(extract_hdr10plus_in(
        source,
        sink,
        "preparing",
        near,
    ))
    .await?;
    if result.is_some() {
        // MP4Box was validated before metadata extraction.
    } else {
        sink.log(
            "  HDR10+ scan complete: no dynamic metadata found; continuing with normal encoding.",
        );
    }
    Ok(result)
}

/// The encoded file is still pending final validation/source replacement.
pub async fn inject_pending(
    encoded: &Path,
    metadata: &ExtractedHdr10Plus,
    sink: &dyn EventSink,
) -> Result<(), String> {
    let staging = hevc_utils::make_tempdir(".histv-hdr10plus-output-", encoded.parent())?;
    let ext = encoded
        .extension()
        .and_then(|v| v.to_str())
        .unwrap_or("mkv");
    let output = staging.path().join(format!("preserved.{ext}"));
    let result =
        crate::pipeline_progress::metadata_work(inject_hdr10plus(encoded, &output, metadata, sink))
            .await?;
    if !result.success {
        return Err(result.message);
    }
    crate::pipeline_progress::check(sink).map_err(|e| e.to_string())?;
    let backup = staging.path().join("encoded-backup");
    std::fs::rename(encoded, &backup).map_err(|e| e.to_string())?;
    if let Err(error) = std::fs::rename(&output, encoded) {
        let _ = std::fs::rename(&backup, encoded);
        return Err(format!("Cannot install HDR10+ output: {error}"));
    }
    Ok(())
}

/// Full presentation-order comparison includes sparse/late metadata and empty
/// pictures, so no metadata can shift onto the wrong frame unnoticed.
pub async fn verify_output(
    output: &Path,
    expected: &ExtractedHdr10Plus,
    sink: &dyn EventSink,
) -> Result<(), String> {
    let actual = crate::pipeline_progress::metadata_work(extract_hdr10plus_in(
        output,
        sink,
        "finalizing",
        output.parent(),
    ))
    .await?
    .ok_or("Required HDR10+ metadata was lost")?;
    if actual.frames != expected.frames {
        return Err("HDR10+ payloads or their frame alignment changed; source retained".into());
    }
    sink.log("  HDR10+: Verified exact metadata and frame alignment across the complete output.");
    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────

/// Parse an HDR10+ ITU-T T.35 payload from a SEI_PREFIX NAL unit's data.
/// Returns the raw T.35 payload (including country code) if found.
pub(crate) fn parse_hdr10plus_from_sei_nalu(nalu_data: &[u8]) -> Option<Vec<u8>> {
    if nalu_data.len() < 6 {
        return None;
    }
    // SEI sizes count RBSP bytes, excluding emulation-prevention 0x03 bytes.
    // Parse an unescaped copy, while retaining the original NAL for reinsertion.
    let mut rbsp = Vec::with_capacity(nalu_data.len() - 2);
    let mut zeros = 0;
    for &byte in &nalu_data[2..] {
        if zeros >= 2 && byte == 3 {
            zeros = 0;
            continue;
        }
        rbsp.push(byte);
        zeros = if byte == 0 { zeros + 1 } else { 0 };
    }
    let mut pos = 0;
    while pos < rbsp.len() {
        let mut kind = 0usize;
        loop {
            let byte = *rbsp.get(pos)?;
            pos += 1;
            kind = kind.checked_add(byte as usize)?;
            if byte != 255 {
                break;
            }
        }
        let mut size = 0usize;
        loop {
            let byte = *rbsp.get(pos)?;
            pos += 1;
            size = size.checked_add(byte as usize)?;
            if byte != 255 {
                break;
            }
        }
        let end = pos.checked_add(size)?;
        let payload = rbsp.get(pos..end)?;
        if kind == 4
            && payload.len() >= 5
            && payload[0] == ITU_T35_COUNTRY_CODE_US
            && u16::from_be_bytes([payload[1], payload[2]]) == HDR10PLUS_PROVIDER_CODE
        {
            return Some(payload.to_vec());
        }
        pos = end;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancellation_cleans_metadata_workspaces_and_keeps_input() {
        use crate::{
            pipeline_progress::PipelineSink,
            test_helpers::{CancellableBatchControl, NoopSink},
        };
        use std::sync::atomic::Ordering;
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source.mkv");
        std::fs::write(&source, b"original source").unwrap();
        let control = CancellableBatchControl::new();
        control.cancel_current.store(true, Ordering::SeqCst);
        let sink = PipelineSink {
            inner: &NoopSink,
            control: &control,
        };
        assert!(prepare(&source, Some(root.path()), "hevc", &sink)
            .await
            .is_err());
        let expected = ExtractedHdr10Plus {
            frames: vec![vec![]],
        };
        assert!(inject_pending(&source, &expected, &sink).await.is_err());
        assert!(verify_output(&source, &expected, &sink).await.is_err());
        assert_eq!(std::fs::read(&source).unwrap(), b"original source");
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[test]
    fn preservation_requires_explicit_opt_in_and_supported_source() {
        assert!(!preservation_requested(false, "hevc", 10));
        assert!(preservation_requested(true, "hevc", 10));
        for (codec, depth) in [("hevc", 8), ("hevc", 12), ("h264", 10), ("av1", 10)] {
            assert!(!preservation_requested(true, codec, depth));
        }
    }

    /// Build a minimal SEI_PREFIX NAL with an HDR10+ T.35 payload.
    fn build_hdr10plus_sei_nalu(payload: &[u8]) -> Vec<u8> {
        // 2-byte NAL header for SEI_PREFIX (type 39): (39 << 1) | 0 = 0x4E, then 0x01
        let mut nalu = vec![0x4E, 0x01];
        // SEI payload type 4 (user_data_registered_itu_t_t35)
        nalu.push(4);
        // SEI payload size (single byte for small payloads)
        nalu.push(payload.len() as u8);
        nalu.extend_from_slice(payload);
        nalu
    }

    #[test]
    fn parse_valid_hdr10plus_sei() {
        // Country code US (0xB5) + Samsung provider (0x003C) + dummy data
        let payload = vec![
            ITU_T35_COUNTRY_CODE_US,
            0x00,
            0x3C, // provider code
            0x00,
            0x01, // provider-oriented code (HDR10+)
            0xAA,
            0xBB,
            0xCC, // payload data
        ];
        let nalu = build_hdr10plus_sei_nalu(&payload);
        let result = parse_hdr10plus_from_sei_nalu(&nalu);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), payload);
    }

    #[test]
    fn escaped_and_second_sei_payloads_are_not_truncated_or_omitted() {
        let payload = vec![0xb5, 0, 0x3c, 0, 1, 4, 1, 0, 0, 1, 0, 0, 2];
        let mut unescaped = vec![5, 1, 42]; // An unrelated SEI before HDR10+.
        unescaped.extend_from_slice(&build_hdr10plus_sei_nalu(&payload)[2..]);
        let mut nalu = vec![0x4e, 1];
        let mut zeros = 0;
        for byte in unescaped {
            if zeros >= 2 && byte <= 3 {
                nalu.push(3);
                zeros = 0;
            }
            nalu.push(byte);
            zeros = if byte == 0 { zeros + 1 } else { 0 };
        }
        assert_eq!(parse_hdr10plus_from_sei_nalu(&nalu), Some(payload));
    }

    #[test]
    fn parse_rejects_wrong_country_code() {
        let payload = vec![
            0x00, // wrong country code
            0x00, 0x3C, 0x00, 0x01, 0xAA, 0xBB, 0xCC,
        ];
        let nalu = build_hdr10plus_sei_nalu(&payload);
        assert!(parse_hdr10plus_from_sei_nalu(&nalu).is_none());
    }

    #[test]
    fn parse_rejects_wrong_provider() {
        let payload = vec![
            ITU_T35_COUNTRY_CODE_US,
            0x00,
            0xFF, // wrong provider
            0x00,
            0x01,
            0xAA,
            0xBB,
            0xCC,
        ];
        let nalu = build_hdr10plus_sei_nalu(&payload);
        assert!(parse_hdr10plus_from_sei_nalu(&nalu).is_none());
    }

    #[test]
    fn parse_rejects_non_t35_sei_type() {
        // SEI type 5 (user_data_unregistered) instead of type 4
        let mut nalu = vec![0x4E, 0x01];
        nalu.push(5); // wrong type
        nalu.push(8); // size
        nalu.extend_from_slice(&[
            ITU_T35_COUNTRY_CODE_US,
            0x00,
            0x3C,
            0x00,
            0x01,
            0xAA,
            0xBB,
            0xCC,
        ]);
        assert!(parse_hdr10plus_from_sei_nalu(&nalu).is_none());
    }

    #[test]
    fn parse_rejects_too_short() {
        assert!(parse_hdr10plus_from_sei_nalu(&[]).is_none());
        assert!(parse_hdr10plus_from_sei_nalu(&[0x4E, 0x01, 4, 2, 0xB5, 0x00]).is_none());
    }
}

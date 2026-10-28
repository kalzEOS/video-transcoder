//! Associate dynamic HDR metadata with pictures, including B-frame reordering.
//! Ambiguous timestamps/counts are rejected instead of shifting metadata.
use crate::{
    events::EventSink,
    hevc_utils::{NalReader, NalUnit},
    pipeline_progress,
};
use std::{
    io::{BufRead, BufReader},
    path::Path,
};

/// Decode-order packet index -> presentation-order picture index.
pub fn rank_timestamps(pts: &[f64]) -> Result<Vec<usize>, String> {
    if pts.is_empty() || pts.iter().any(|p| !p.is_finite()) {
        return Err("Missing picture timestamps; metadata alignment cannot be verified".into());
    }
    let mut sorted: Vec<_> = (0..pts.len()).collect();
    sorted.sort_by(|a, b| pts[*a].total_cmp(&pts[*b]));
    if sorted.windows(2).any(|w| pts[w[0]] == pts[w[1]]) {
        return Err("Duplicate picture timestamps; metadata alignment is ambiguous".into());
    }
    if sorted.len() > 2 {
        let interval = (pts[*sorted.last().unwrap()] - pts[sorted[0]]) / (sorted.len() - 1) as f64;
        if sorted
            .windows(2)
            .any(|w| ((pts[w[1]] - pts[w[0]]) - interval).abs() > 0.0015f64.max(interval * 0.01))
        {
            return Err("Variable picture timing cannot be preserved by the raw HDR metadata muxer; source retained instead of changing frame timing".into());
        }
    }
    let mut rank = vec![0; pts.len()];
    for (position, index) in sorted.into_iter().enumerate() {
        rank[index] = position;
    }
    Ok(rank)
}

pub async fn packet_order(
    path: &Path,
    count: usize,
    temp_parent: &Path,
    sink: &dyn EventSink,
) -> Result<Vec<usize>, String> {
    Ok(packet_timeline(path, count, temp_parent, sink).await?.rank)
}

pub struct PacketTimeline {
    pub rank: Vec<usize>,
    pub first_pts: f64,
}

pub async fn packet_timeline(
    path: &Path,
    count: usize,
    temp_parent: &Path,
    sink: &dyn EventSink,
) -> Result<PacketTimeline, String> {
    pipeline_progress::check(sink).map_err(|e| e.to_string())?;
    let file = tempfile::NamedTempFile::new_in(temp_parent).map_err(|e| e.to_string())?;
    let child = crate::ffmpeg::ffprobe_command()
        .env("TMPDIR", temp_parent)
        .env("TMP", temp_parent)
        .env("TEMP", temp_parent)
        .kill_on_drop(true)
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_packets",
            "-show_entries",
            "packet=pts_time",
            "-of",
            "csv=p=0",
            &path.to_string_lossy(),
        ])
        .stdout(std::process::Stdio::from(
            file.reopen().map_err(|e| e.to_string())?,
        ))
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let result = pipeline_progress::wait_output(child, sink).await?;
    if !result.status.success() {
        return Err(format!(
            "Cannot read metadata timestamps: {}",
            String::from_utf8_lossy(&result.stderr)
        ));
    }
    let mut pts = Vec::with_capacity(count);
    for line in BufReader::new(file.reopen().map_err(|e| e.to_string())?).lines() {
        pipeline_progress::check(sink).map_err(|e| e.to_string())?;
        let line = line.map_err(|e| e.to_string())?;
        if line.trim().is_empty() {
            continue;
        }
        pts.push(
            line.split(',')
                .next()
                .unwrap_or("")
                .parse::<f64>()
                .map_err(|_| "Missing metadata timestamp")?,
        );
        if pts.len() > count {
            return Err(
                "Picture/packet count mismatch; metadata alignment cannot be verified".into(),
            );
        }
    }
    if pts.len() != count {
        return Err(format!(
            "Metadata alignment requires {count} pictures, found {} packets",
            pts.len()
        ));
    }
    let rank = rank_timestamps(&pts)?;
    let first_pts = pts.iter().copied().fold(f64::INFINITY, f64::min);
    Ok(PacketTimeline { rank, first_pts })
}

pub fn to_presentation<T: Default>(mut frames: Vec<T>, rank: &[usize]) -> Vec<T> {
    let mut result: Vec<T> = (0..frames.len()).map(|_| T::default()).collect();
    for (index, position) in rank.iter().enumerate() {
        result[*position] = std::mem::take(&mut frames[index]);
    }
    result
}

/// Prefix SEIs belong to the following picture; DV RPUs to the preceding VCL.
pub fn frame_nalus<F: Fn(&NalUnit) -> bool>(
    path: &Path,
    suffix: bool,
    filter: F,
    sink: &dyn EventSink,
    stage: &str,
    message: &str,
) -> Result<Vec<Vec<Vec<u8>>>, String> {
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let total = file.metadata().map_err(|e| e.to_string())?.len();
    let mut reader = NalReader::new(file);
    let mut frames: Vec<Vec<Vec<u8>>> = Vec::new();
    let mut pending = Vec::new();
    let mut last = std::time::Instant::now();
    loop {
        pipeline_progress::check(sink).map_err(|e| e.to_string())?;
        let Some(nalu) = reader.next_nalu().map_err(|e| e.to_string())? else {
            break;
        };
        if nalu.is_first_slice_of_picture() {
            frames.push(std::mem::take(&mut pending));
        }
        if filter(&nalu) {
            if suffix {
                frames
                    .last_mut()
                    .ok_or("Metadata before first picture")?
                    .push(nalu.data.clone());
            } else {
                pending.push(nalu.data.clone());
            }
        }
        reader.recycle(nalu);
        if total > 0 && last.elapsed() >= std::time::Duration::from_millis(250) {
            sink.processing_stage(
                stage,
                message,
                Some(reader.bytes_scanned() as f64 / total as f64 * 100.0),
            );
            last = std::time::Instant::now();
        }
    }
    if !pending.is_empty() {
        return Err("Unattached dynamic metadata after final picture".into());
    }
    sink.processing_stage(stage, message, Some(100.0));
    Ok(frames)
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn timestamp_scratch_requires_the_selected_workspace() {
        let root = tempfile::tempdir().unwrap();
        let blocked = root.path().join("unavailable workspace");
        std::fs::write(&blocked, b"unrelated file").unwrap();
        assert!(super::packet_timeline(
            &root.path().join("input.mkv"),
            1,
            &blocked,
            &crate::test_helpers::NoopSink
        )
        .await
        .is_err());
        assert_eq!(std::fs::read(&blocked).unwrap(), b"unrelated file");
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }
    use super::*;
    #[test]
    fn reordered_b_frames_keep_metadata_with_presentation_picture() {
        let rank = rank_timestamps(&[0.0, 0.12, 0.04, 0.08]).unwrap();
        assert_eq!(rank, vec![0, 3, 1, 2]);
        assert_eq!(
            to_presentation(vec!["I", "P", "B1", "B2"], &rank),
            vec!["I", "B1", "B2", "P"]
        );
        assert!(rank_timestamps(&[0.0, 0.0]).is_err());
        assert!(rank_timestamps(&[f64::NAN]).is_err());
        assert!(rank_timestamps(&[0.0, 0.04, 0.11]).is_err());
    }
    #[test]
    fn sparse_hdr10plus_and_suffix_rpus_stay_on_their_frames() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let mut data = Vec::new();
        for nalu in [
            &[78, 1, 99][..],
            &[2, 1, 128],
            &[124, 1, 42],
            &[2, 1, 128],
            &[124, 1, 43],
        ] {
            data.extend_from_slice(&crate::hevc_utils::START_CODE);
            data.extend_from_slice(nalu);
        }
        std::fs::write(file.path(), data).unwrap();
        let sink = crate::test_helpers::NoopSink;
        let hdr = frame_nalus(
            file.path(),
            false,
            |n| n.nal_type == 39,
            &sink,
            "preparing",
            "HDR",
        )
        .unwrap();
        assert_eq!(hdr.len(), 2);
        assert_eq!(hdr[0].len(), 1);
        assert!(hdr[1].is_empty());
        let dv = frame_nalus(
            file.path(),
            true,
            |n| n.nal_type == 62,
            &sink,
            "preparing",
            "DV",
        )
        .unwrap();
        assert_eq!(dv[0][0], vec![124, 1, 42]);
        assert_eq!(dv[1][0], vec![124, 1, 43]);
    }
}

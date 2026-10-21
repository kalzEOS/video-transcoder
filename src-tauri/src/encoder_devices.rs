//! Best-effort Linux device preference, separate from codec verification.
//! Metadata only changes probe order; it never makes a device compatible or
//! removes an unknown/unreadable device from consideration.

use std::cmp::Reverse;
use std::path::Path;

#[derive(Default)]
struct DevicePreference {
    dedicated: bool,
    vram_bytes: u64,
    intel: bool,
}

fn preference(node: &str, sysfs_drm: &Path) -> DevicePreference {
    let Some(name) = Path::new(node).file_name() else {
        return DevicePreference::default();
    };
    let device = sysfs_drm.join(name).join("device");
    let read_number = |file: &str| {
        std::fs::read_to_string(device.join(file))
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
    };
    let vram_bytes = read_number("mem_info_vram_total").unwrap_or(0);
    // PCIe-attached GPUs expose a real maximum link width. Integrated GPUs
    // commonly report 0 or the unknown-width sentinel 255. Use the maximum,
    // not the current width, so runtime power management does not affect order.
    // Older kernels may omit the attribute; reported VRAM is then a hint.
    // These are preference heuristics, not a universal GPU-type classifier.
    let dedicated = match read_number("max_link_width") {
        Some(width) => matches!(width, 1 | 2 | 4 | 8 | 12 | 16 | 32),
        None => vram_bytes > 0 && !device.join("uma").exists(),
    };
    let intel = std::fs::read_to_string(device.join("vendor"))
        .ok()
        .and_then(|s| u32::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok())
        == Some(0x8086);
    DevicePreference {
        dedicated,
        vram_bytes: if dedicated { vram_bytes } else { 0 },
        intel,
    }
}

/// Prefer standalone GPUs for VAAPI, with VRAM as a deterministic tie-breaker.
/// For QSV, prefer Intel independently. Stable sorting preserves discovery
/// order when metadata is missing or equal, and every device remains eligible.
pub(super) fn preferred_render_nodes(
    encoder: &str,
    nodes: &[String],
    sysfs_drm: &Path,
) -> Vec<String> {
    let mut ranked: Vec<_> = nodes
        .iter()
        .map(|n| (n, preference(n, sysfs_drm)))
        .collect();
    if super::is_vaapi_encoder(encoder) {
        ranked.sort_by_key(|(_, p)| Reverse((p.dedicated, p.vram_bytes)));
    } else if super::is_qsv_encoder(encoder) {
        ranked.sort_by_key(|(_, p)| Reverse(p.intel));
    }
    ranked.into_iter().map(|(n, _)| n.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(root: &Path, name: &str, vendor: &str, width: &str, vram: &str) {
        let path = root.join(name).join("device");
        std::fs::create_dir_all(&path).unwrap();
        for (file, value) in [
            ("vendor", vendor),
            ("max_link_width", width),
            ("mem_info_vram_total", vram),
        ] {
            std::fs::write(path.join(file), value).unwrap();
        }
    }

    #[test]
    fn dedicated_gpu_precedes_integrated_regardless_of_node_number_or_boot_gpu() {
        let root = tempfile::tempdir().unwrap();
        for (integrated, dedicated) in [("renderD128", "renderD129"), ("renderD140", "renderD130")]
        {
            device(root.path(), integrated, "0x8086", "255", "0");
            device(root.path(), dedicated, "0x1002", "16", "17179869184");
            let nodes = vec![
                format!("/dev/dri/{integrated}"),
                format!("/dev/dri/{dedicated}"),
            ];
            // Being the display/boot GPU is not an encoding preference.
            std::fs::write(root.path().join(integrated).join("device/boot_vga"), "1").unwrap();
            std::fs::write(root.path().join(dedicated).join("device/boot_vga"), "0").unwrap();
            assert_eq!(
                preferred_render_nodes("hevc_vaapi", &nodes, root.path()),
                vec![nodes[1].clone(), nodes[0].clone()]
            );
            assert_eq!(
                preferred_render_nodes("hevc_qsv", &nodes, root.path()),
                nodes
            );
        }
    }

    #[test]
    fn dedicated_intel_also_precedes_integrated_amd() {
        let root = tempfile::tempdir().unwrap();
        // An APU's reserved UMA memory must not outrank an Intel discrete GPU.
        device(root.path(), "renderD128", "0x1002", "0", "8589934592");
        device(root.path(), "renderD129", "0x8086", "8", "0");
        let nodes = vec!["/dev/dri/renderD128".into(), "/dev/dri/renderD129".into()];
        assert_eq!(
            preferred_render_nodes("h264_vaapi", &nodes, root.path()),
            vec![nodes[1].clone(), nodes[0].clone()]
        );
    }

    #[test]
    fn missing_invalid_and_single_device_metadata_are_safe() {
        let root = tempfile::tempdir().unwrap();
        device(root.path(), "renderD128", "bad", "unknown", "invalid");
        let nodes = vec!["/dev/dri/renderD128".into(), "/dev/dri/renderD129".into()];
        assert_eq!(
            preferred_render_nodes("av1_vaapi", &nodes, root.path()),
            nodes
        );
        assert_eq!(
            preferred_render_nodes("hevc_qsv", &nodes, root.path()),
            nodes
        );
        assert_eq!(
            preferred_render_nodes("hevc_vaapi", &nodes[..1], root.path()),
            nodes[..1]
        );
        assert!(preferred_render_nodes("hevc_vaapi", &[], root.path()).is_empty());
        assert_eq!(
            preferred_render_nodes("hevc_nvenc", &nodes, root.path()),
            nodes
        );
    }

    #[test]
    fn old_kernel_vram_hint_and_equal_preferences_preserve_all_candidates() {
        let root = tempfile::tempdir().unwrap();
        for name in ["renderD129", "renderD130"] {
            device(root.path(), name, "0x1002", "16", "17179869184");
            std::fs::remove_file(root.path().join(name).join("device/max_link_width")).unwrap();
        }
        let nodes = vec![
            "/dev/dri/renderD128".into(),
            "/dev/dri/renderD129".into(),
            "/dev/dri/renderD130".into(),
        ];
        assert_eq!(
            preferred_render_nodes("hevc_vaapi", &nodes, root.path()),
            vec![nodes[1].clone(), nodes[2].clone(), nodes[0].clone()]
        );
    }
}

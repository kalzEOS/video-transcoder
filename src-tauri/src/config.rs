use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use tauri::AppHandle;
use tauri::Manager;

/// All persisted user settings (§13.2).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(default)]
pub struct AppConfig {
    pub theme: String,
    pub output_folder: String,
    pub output_mode: String, // "folder" | "beside" | "replace"
    pub overwrite: bool,
    pub delete_source: bool,
    pub save_log: bool,
    pub show_toast: bool,
    pub post_action: String,
    pub post_countdown: u32,
    pub custom_command: String,
    pub video_codec: String, // "HEVC" | "H.264"
    pub target_bitrate: u32,
    pub qp_i: u32,
    pub qp_p: u32,
    pub crf: u32,
    pub rate_control_mode: String, // "QP" | "CRF"
    pub hdr: bool,
    /// None denotes a config written by a build that forced HDR preservation.
    pub preserve_hdr: Option<bool>,
    pub audio_codec: String, // "AC3" | "EAC3" | "AAC" | "Copy"
    pub audio_bitrate_cap: u32,
    pub auto_clear_completed: bool,
    pub log_drawer_open: bool,
    pub peak_multiplier: f64,
    pub threads: u32,
    pub low_priority: bool,
    pub precision_mode: bool,
    pub hardware_decoding: bool,
    pub preserve_hdr10plus: bool,
    /// New single HDR selector. None migrates the legacy checkboxes.
    pub hdr_mode: Option<crate::encoder::HdrMode>,
    #[serde(default)]
    pub compatibility_mode: bool,
    #[serde(default)]
    pub preserve_av1: bool,
    #[serde(default)]
    pub force_local: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            theme: "Default Dark".to_string(),
            output_folder: "output".to_string(),
            output_mode: "folder".to_string(),
            overwrite: false,
            delete_source: false,
            save_log: false,
            show_toast: false,
            post_action: "None".to_string(),
            post_countdown: 0,
            custom_command: String::new(),
            video_codec: "HEVC".to_string(),
            target_bitrate: 4,
            qp_i: 20,
            qp_p: 22,
            crf: 20,
            rate_control_mode: "QP".to_string(),
            hdr: false,
            preserve_hdr: None,
            audio_codec: "AC3".to_string(),
            audio_bitrate_cap: 640,
            auto_clear_completed: false,
            log_drawer_open: false,
            peak_multiplier: 1.5,
            threads: 0,
            low_priority: false,
            precision_mode: false,
            hardware_decoding: false,
            preserve_hdr10plus: false,
            hdr_mode: None,
            compatibility_mode: false,
            preserve_av1: false,
            force_local: false,
        }
    }
}

/// Get the directory where config.json lives.
///
/// Uses the platform-standard app data directory (via Tauri's path API),
/// falling back to the directory containing the executable if unavailable.
/// This avoids writing to read-only locations (AppImage mounts, /usr/bin)
/// and follows OS conventions (Linux: ~/.local/share, macOS: ~/Library,
/// Windows: AppData\Roaming).
fn config_dir(app: &AppHandle) -> PathBuf {
    // Prefer platform-standard app data directory
    if let Ok(dir) = app.path().app_data_dir() {
        let _ = fs::create_dir_all(&dir);
        return dir;
    }
    // Fallback: directory containing the executable
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            return dir.to_path_buf();
        }
    }
    PathBuf::from(".")
}

fn config_path(app: &AppHandle) -> PathBuf {
    config_dir(app).join("config.json")
}

pub fn load_config(app: &AppHandle) -> AppConfig {
    let path = config_path(app);

    // Migrate: if the new location is empty but the old exe-relative config
    // exists, copy it over so users don't lose settings on upgrade.
    if !path.exists() {
        if let Ok(exe) = std::env::current_exe() {
            if let Some(exe_dir) = exe.parent() {
                let legacy = exe_dir.join("config.json");
                if legacy.exists() && legacy != path {
                    eprintln!(
                        "Migrating config from {} to {}",
                        legacy.display(),
                        path.display()
                    );
                    let _ = fs::copy(&legacy, &path);
                }
            }
        }
    }

    if path.exists() {
        match fs::read_to_string(&path) {
            Ok(contents) => match serde_json::from_str::<AppConfig>(&contents) {
                Ok(cfg) => return cfg,
                Err(e) => {
                    eprintln!("Malformed config.json, using defaults: {e}");
                }
            },
            Err(e) => {
                eprintln!("Could not read config.json: {e}");
            }
        }
    }
    AppConfig::default()
}

pub fn save_config(app: &AppHandle, config: &AppConfig) -> Result<(), Box<dyn std::error::Error>> {
    let path = config_path(app);
    let json = serde_json::to_string_pretty(config)?;
    // Write to a temporary file then rename for atomicity. If the app
    // crashes mid-write, only the temp file is corrupted.
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, &json)?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hdr10plus_is_opt_in_and_saved_independently_of_gpu_decoding() {
        let mut config: AppConfig = serde_json::from_str("{}").unwrap();
        assert!(!config.preserve_hdr10plus);
        config.preserve_hdr10plus = true;
        let json = serde_json::to_value(&config).unwrap();
        assert_eq!(json["preserveHdr10plus"], true);
        let restored: AppConfig = serde_json::from_value(json).unwrap();
        assert!(restored.preserve_hdr10plus);
        assert!(!restored.hardware_decoding);
        assert_eq!(restored.hdr, AppConfig::default().hdr);
    }

    #[test]
    fn legacy_hdr_config_does_not_silently_opt_into_sdr_conversion() {
        let old: AppConfig = serde_json::from_str(r#"{"hdr":false}"#).unwrap();
        assert_eq!(old.preserve_hdr, None);
        let mut selected = old;
        selected.preserve_hdr = Some(false);
        let restored: AppConfig =
            serde_json::from_value(serde_json::to_value(selected).unwrap()).unwrap();
        assert_eq!(restored.preserve_hdr, Some(false));
    }

    #[test]
    fn single_hdr_mode_round_trips_without_changing_legacy_fields() {
        let mut config: AppConfig = serde_json::from_str(r#"{"preserveHdr":false}"#).unwrap();
        config.hdr_mode = Some(crate::encoder::HdrMode::Auto);
        let saved = serde_json::to_value(&config).unwrap();
        assert_eq!(saved["hdrMode"], "auto");
        assert_eq!(saved["preserveHdr"], false);
        let restored: AppConfig = serde_json::from_value(saved).unwrap();
        assert_eq!(restored.hdr_mode, Some(crate::encoder::HdrMode::Auto));
    }

    #[test]
    fn hardware_decoding_defaults_off_and_persists() {
        let old: AppConfig = serde_json::from_str(r#"{"hdr":true}"#).unwrap();
        assert!(!old.hardware_decoding);
        let mut config = old;
        config.hardware_decoding = true;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
        let restored: AppConfig = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert!(restored.hardware_decoding);
        assert!(restored.hdr);
    }

    #[test]
    fn test_default_round_trip() {
        let config = AppConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let deserialized: AppConfig = serde_json::from_str(&json).unwrap();
        // Spot-check key fields
        assert_eq!(deserialized.theme, "Default Dark");
        assert_eq!(deserialized.video_codec, "HEVC");
        assert_eq!(deserialized.rate_control_mode, "QP");
        assert_eq!(deserialized.audio_bitrate_cap, 640);
        assert!(!deserialized.overwrite);
        assert!(!deserialized.compatibility_mode);
        assert!(!deserialized.preserve_av1);
    }

    #[test]
    fn test_forward_compat_unknown_fields() {
        // JSON with an unknown field should deserialize without error
        let json = r#"{"theme":"Nord","unknownNewField":42,"videoCodec":"H.264"}"#;
        let config: AppConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.theme, "Nord");
        assert_eq!(config.video_codec, "H.264");
        // Other fields should have defaults
        assert_eq!(config.output_folder, "output");
    }

    #[test]
    fn test_backward_compat_missing_fields() {
        // Minimal JSON with only one field - all others should get defaults
        let json = r#"{"theme":"Custom"}"#;
        let config: AppConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.theme, "Custom");
        assert_eq!(config.output_mode, "folder");
        assert_eq!(config.crf, 20);
        assert_eq!(config.peak_multiplier, 1.5);
        assert!(!config.force_local);
    }

    #[test]
    fn test_empty_json_uses_defaults() {
        let config: AppConfig = serde_json::from_str("{}").unwrap();
        let default = AppConfig::default();
        assert_eq!(config.theme, default.theme);
        assert_eq!(config.video_codec, default.video_codec);
        assert_eq!(config.threads, default.threads);
    }

    #[test]
    fn test_camel_case_serialization() {
        let config = AppConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        // serde(rename_all = "camelCase") should produce camelCase keys
        assert!(json.contains("outputFolder"));
        assert!(json.contains("rateControlMode"));
        assert!(json.contains("peakMultiplier"));
        assert!(!json.contains("output_folder")); // Should NOT have snake_case
    }
}

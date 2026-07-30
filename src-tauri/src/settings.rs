//! Settings store — serde JSON file (same schema as electron-store)

use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

/// Full settings schema matching the Electron version exactly.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppSettings {
    pub output_folder: String,
    pub hotkey_clip30_sec: String,
    pub hotkey_clip1_min: String,
    pub hotkey_clip5_min: String,
    pub hotkey_record: String,
    pub buffer_duration: u32,
    pub resolution: String,
    pub fps: u32,
    pub aspect_ratio: String,
    pub encoder: String,
    pub bitrate: u32,
    pub audio_bitrate: u32,
    pub capture_audio: bool,
    pub capture_mic: bool,
    pub audio_source: String,
    pub audio_input_device_id: String,
    pub game_detect: bool,
    pub auto_upload: bool,
    pub minimize_to_tray: bool,
    pub overlay_enabled: bool,
    pub clip_sound_enabled: bool,
    pub cloud_enabled: bool,
    pub cloud_pair_code: String,
    pub upload_bandwidth: u32,
    pub delete_after_upload: bool,
    pub desktop_device_id: String,
    pub desktop_audio_device_id: String,
    /// Output orientation: "landscape" (1920x1088) or "portrait" (1088x1920)
    pub orientation: String,
    pub watch_folder_path: String,
    pub watch_folder_enabled: bool,
}

impl Default for AppSettings {
    fn default() -> Self {
        let output = dirs::video_dir()
            .unwrap_or_else(|| dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")))
            .join("Clipsta");
        Self {
            output_folder: output.to_string_lossy().to_string(),
            hotkey_clip30_sec: "Ctrl+Shift+G".to_string(),
            hotkey_clip1_min: "Alt+F9".to_string(),
            hotkey_clip5_min: "Alt+F10".to_string(),
            hotkey_record: "F9".to_string(),
            buffer_duration: 300,
            resolution: "720p".to_string(),
            fps: 60,
            aspect_ratio: "16:9".to_string(),
            encoder: "auto".to_string(),
            bitrate: 8000,
            audio_bitrate: 192,
            capture_audio: true,
            capture_mic: false,
            audio_source: "both".to_string(),
            audio_input_device_id: String::new(),
            game_detect: true,
            auto_upload: false,
            minimize_to_tray: true,
            overlay_enabled: true,
            clip_sound_enabled: true,
            cloud_enabled: false,
            cloud_pair_code: String::new(),
            upload_bandwidth: 0,
            delete_after_upload: false,
            desktop_device_id: String::new(),
            desktop_audio_device_id: String::new(),
            orientation: "landscape".to_string(),
            watch_folder_path: String::new(),
            watch_folder_enabled: false,
        }
    }
}

/// Thread-safe settings store backed by a JSON file.
#[derive(Clone)]
pub struct SettingsStore {
    inner: Arc<RwLock<AppSettings>>,
    path: PathBuf,
}

impl SettingsStore {
    /// Load settings from `app_data_dir/settings.json`, or create defaults.
    pub fn load(app_data_dir: &std::path::Path) -> Self {
        let path = app_data_dir.join("settings.json");
        let settings = if path.exists() {
            match std::fs::read_to_string(&path) {
                Ok(json) => serde_json::from_str::<AppSettings>(&json).unwrap_or_default(),
                Err(_) => AppSettings::default(),
            }
        } else {
            AppSettings::default()
        };

        let store = Self {
            inner: Arc::new(RwLock::new(settings)),
            path,
        };
        // Ensure desktop_device_id is set
        {
            let mut s = store.inner.write();
            if s.desktop_device_id.is_empty() {
                s.desktop_device_id = format!("desktop_{}", uuid::Uuid::new_v4().simple());
            }
        }
        store.save_to_disk();
        store
    }

    pub fn get(&self) -> AppSettings {
        self.inner.read().clone()
    }

    pub fn get_field(&self, key: &str) -> serde_json::Value {
        let s = self.inner.read();
        let full = serde_json::to_value(&*s).unwrap_or_default();
        full.get(key).cloned().unwrap_or(serde_json::Value::Null)
    }

    pub fn set_field(&self, key: &str, value: serde_json::Value) {
        let mut s = self.inner.write();
        let mut map = serde_json::to_value(&*s).unwrap_or_default();
        if let Some(obj) = map.as_object_mut() {
            obj.insert(key.to_string(), value);
        }
        if let Ok(updated) = serde_json::from_value::<AppSettings>(map) {
            *s = updated;
        }
        drop(s);
        self.save_to_disk();
    }

    pub fn set_all(&self, partial: serde_json::Value) {
        let mut s = self.inner.write();
        let mut current = serde_json::to_value(&*s).unwrap_or_default();
        if let (Some(cur_obj), Some(new_obj)) = (current.as_object_mut(), partial.as_object()) {
            for (k, v) in new_obj {
                cur_obj.insert(k.clone(), v.clone());
            }
        }
        if let Ok(updated) = serde_json::from_value::<AppSettings>(current) {
            *s = updated;
        }
        drop(s);
        self.save_to_disk();
    }

    pub fn update(&self, settings: AppSettings) {
        *self.inner.write() = settings;
        self.save_to_disk();
    }

    fn save_to_disk(&self) {
        let s = self.inner.read();
        if let Ok(json) = serde_json::to_string_pretty(&*s) {
            if let Some(parent) = self.path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&self.path, json);
        }
    }
}

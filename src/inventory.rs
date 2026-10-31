use crate::vapix::DeviceInfo;
use serde::{Deserialize, Serialize};
use std::{fs, path::PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SavedDevice {
    pub address: String,
    pub info: Option<DeviceInfo>,
    pub last_seen: Option<u64>,
}

pub fn path() -> PathBuf {
    data_directory().join("axis-device-manager-for-linux/devices.json")
}

fn data_directory() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .unwrap_or_else(std::env::temp_dir);
    base
}

pub fn load() -> Result<Vec<SavedDevice>, String> {
    load_from(&path())
}

fn load_from(path: &std::path::Path) -> Result<Vec<SavedDevice>, String> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let bytes = fs::read(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    serde_json::from_slice(&bytes).map_err(|error| format!("{}: {error}", path.display()))
}

pub fn save(devices: &[SavedDevice]) -> Result<(), String> {
    save_to(&path(), devices)
}

fn save_to(path: &std::path::Path, devices: &[SavedDevice]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "inventory path has no parent directory".to_owned())?;
    fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let temporary = parent.join(format!(".devices.{:016x}.tmp", rand::random::<u64>()));
    let bytes = serde_json::to_vec_pretty(devices).map_err(|error| error.to_string())?;
    fs::write(&temporary, bytes).map_err(|error| error.to_string())?;
    fs::rename(&temporary, &path).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        error.to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_device_round_trips() {
        let saved = SavedDevice {
            address: "http://10.0.0.162".into(),
            info: Some(DeviceInfo {
                product_name: Some("AXIS M1065-LW Network Camera".into()),
                product_number: Some("M1065-LW".into()),
                firmware_version: Some("9.80.146".into()),
                serial_number: Some("B8A44F081A9A".into()),
            }),
            last_seen: Some(1234),
        };
        let test_dir = std::env::temp_dir().join(format!(
            "axis-device-manager-inventory-test-{:016x}",
            rand::random::<u64>()
        ));
        let test_path = test_dir.join("devices.json");
        save_to(&test_path, std::slice::from_ref(&saved)).unwrap();
        let restored = load_from(&test_path).unwrap().remove(0);
        assert_eq!(restored.address, saved.address);
        assert_eq!(restored.last_seen, saved.last_seen);
        assert_eq!(
            restored.info.unwrap().product_number.as_deref(),
            Some("M1065-LW")
        );
        std::fs::remove_dir_all(test_dir).unwrap();
    }
}

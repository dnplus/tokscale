use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use uuid::Uuid;

const DEVICE_FILE_NAME: &str = "device.json";
const DEVICE_ID_ENV: &str = "TOKSCALE_DEVICE_ID";
const DEVICE_NAME_ENV: &str = "TOKSCALE_DEVICE_NAME";
const MAX_DEVICE_ID_LEN: usize = 96;
const MAX_DEVICE_NAME_LEN: usize = 120;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SubmitDevice {
    pub id: String,
    pub name: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct StoredDevice {
    id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    created_at: String,
}

pub fn resolve_submit_device() -> Result<SubmitDevice> {
    if let Some(device) = existing_submit_device()? {
        return Ok(device);
    }

    let name_override = env_value(DEVICE_NAME_ENV)
        .map(|name| validate_device_name(&name))
        .transpose()?;
    let stored = StoredDevice {
        id: format!("dev_{}", Uuid::new_v4().simple()),
        name: name_override,
        created_at: Utc::now().to_rfc3339(),
    };
    write_stored_device(&device_file_path(), &stored)?;

    Ok(SubmitDevice {
        id: stored.id,
        name: stored.name,
    })
}

/// The device a submit would use, from the environment or `device.json`,
/// without creating one.
pub fn existing_submit_device() -> Result<Option<SubmitDevice>> {
    let name_override = env_value(DEVICE_NAME_ENV)
        .map(|name| validate_device_name(&name))
        .transpose()?;
    if let Some(id) = env_value(DEVICE_ID_ENV) {
        return Ok(Some(SubmitDevice {
            id: validate_device_id(&id)?,
            name: name_override,
        }));
    }

    let path = device_file_path();
    if !path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let stored: StoredDevice = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(Some(SubmitDevice {
        id: validate_device_id(&stored.id)?,
        name: name_override.or(stored.name),
    }))
}

/// Save `name` as this machine's device label in `device.json`, creating the
/// device id if it does not exist yet. Scheduled runs read it from there, since
/// they do not see `TOKSCALE_DEVICE_NAME`.
pub fn store_device_name(name: &str) -> Result<()> {
    let name = validate_device_name(name)?;
    let path = device_file_path();
    let stored = if path.exists() {
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let mut stored: StoredDevice = serde_json::from_str(&content)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        stored.name = Some(name);
        stored
    } else {
        StoredDevice {
            id: format!("dev_{}", Uuid::new_v4().simple()),
            name: Some(name),
            created_at: Utc::now().to_rfc3339(),
        }
    };
    write_stored_device(&path, &stored)
}

fn env_value(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn device_file_path() -> PathBuf {
    crate::paths::get_config_dir().join(DEVICE_FILE_NAME)
}

fn validate_device_id(id: &str) -> Result<String> {
    let trimmed = id.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("{} must not be empty", DEVICE_ID_ENV));
    }
    if trimmed.len() > MAX_DEVICE_ID_LEN {
        return Err(anyhow!(
            "{} must be at most {} characters",
            DEVICE_ID_ENV,
            MAX_DEVICE_ID_LEN
        ));
    }
    if !trimmed
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.' | ':'))
    {
        return Err(anyhow!(
            "{} may only contain ASCII letters, numbers, '.', '_', '-', or ':'",
            DEVICE_ID_ENV
        ));
    }
    Ok(trimmed.to_string())
}

pub(crate) fn validate_device_name(name: &str) -> Result<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("{} must not be empty", DEVICE_NAME_ENV));
    }
    if trimmed.len() > MAX_DEVICE_NAME_LEN {
        return Err(anyhow!(
            "{} must be at most {} characters",
            DEVICE_NAME_ENV,
            MAX_DEVICE_NAME_LEN
        ));
    }
    Ok(trimmed.to_string())
}

fn write_stored_device(path: &Path, device: &StoredDevice) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let tmp_path = path.with_extension("json.tmp");
    let content = serde_json::to_string_pretty(device)?;
    std::fs::write(&tmp_path, content)
        .with_context(|| format!("failed to write {}", tmp_path.display()))?;
    tokscale_core::fs_atomic::replace_file(&tmp_path, path)
        .with_context(|| format!("failed to replace {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::env;

    fn save_env() -> (
        Option<std::ffi::OsString>,
        Option<std::ffi::OsString>,
        Option<std::ffi::OsString>,
    ) {
        (
            env::var_os("TOKSCALE_CONFIG_DIR"),
            env::var_os("TOKSCALE_DEVICE_ID"),
            env::var_os("TOKSCALE_DEVICE_NAME"),
        )
    }

    struct EnvRestore(
        Option<std::ffi::OsString>,
        Option<std::ffi::OsString>,
        Option<std::ffi::OsString>,
    );

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            restore_env((self.0.clone(), self.1.clone(), self.2.clone()));
        }
    }

    fn restore_env(
        prev: (
            Option<std::ffi::OsString>,
            Option<std::ffi::OsString>,
            Option<std::ffi::OsString>,
        ),
    ) {
        unsafe {
            match prev.0 {
                Some(v) => env::set_var("TOKSCALE_CONFIG_DIR", v),
                None => env::remove_var("TOKSCALE_CONFIG_DIR"),
            }
            match prev.1 {
                Some(v) => env::set_var("TOKSCALE_DEVICE_ID", v),
                None => env::remove_var("TOKSCALE_DEVICE_ID"),
            }
            match prev.2 {
                Some(v) => env::set_var("TOKSCALE_DEVICE_NAME", v),
                None => env::remove_var("TOKSCALE_DEVICE_NAME"),
            }
        }
    }

    #[test]
    #[serial]
    fn env_device_id_is_used_without_touching_config_file() {
        let prev = save_env();
        let _restore = EnvRestore(prev.0, prev.1, prev.2);
        let dir = tempfile::tempdir().unwrap();
        unsafe {
            env::set_var("TOKSCALE_CONFIG_DIR", dir.path());
            env::set_var("TOKSCALE_DEVICE_ID", "dev_ci");
            env::set_var("TOKSCALE_DEVICE_NAME", "CI runner");
        }

        let device = resolve_submit_device().unwrap();

        assert_eq!(device.id, "dev_ci");
        assert_eq!(device.name.as_deref(), Some("CI runner"));
        assert!(!dir.path().join("device.json").exists());
    }

    #[test]
    #[serial]
    fn generated_device_id_is_stable_in_config_dir() {
        let prev = save_env();
        let _restore = EnvRestore(prev.0, prev.1, prev.2);
        let dir = tempfile::tempdir().unwrap();
        unsafe {
            env::set_var("TOKSCALE_CONFIG_DIR", dir.path());
            env::remove_var("TOKSCALE_DEVICE_ID");
            env::remove_var("TOKSCALE_DEVICE_NAME");
        }

        let first = resolve_submit_device().unwrap();
        let second = resolve_submit_device().unwrap();

        assert!(first.id.starts_with("dev_"));
        assert_eq!(first, second);
        assert!(dir.path().join("device.json").exists());
    }

    #[test]
    #[serial]
    fn stored_device_name_keeps_the_id_and_labels_later_runs() {
        let prev = save_env();
        let _restore = EnvRestore(prev.0, prev.1, prev.2);
        let dir = tempfile::tempdir().unwrap();
        unsafe {
            env::set_var("TOKSCALE_CONFIG_DIR", dir.path());
            env::remove_var("TOKSCALE_DEVICE_ID");
            env::remove_var("TOKSCALE_DEVICE_NAME");
        }

        let before = resolve_submit_device().unwrap();
        assert_eq!(before.name, None);

        store_device_name("  Work laptop  ").unwrap();
        let after = resolve_submit_device().unwrap();

        assert_eq!(after.id, before.id);
        assert_eq!(after.name.as_deref(), Some("Work laptop"));
        assert!(store_device_name("   ").is_err());
    }

    #[test]
    #[serial]
    fn stored_device_name_creates_a_device_when_none_exists() {
        let prev = save_env();
        let _restore = EnvRestore(prev.0, prev.1, prev.2);
        let dir = tempfile::tempdir().unwrap();
        unsafe {
            env::set_var("TOKSCALE_CONFIG_DIR", dir.path());
            env::remove_var("TOKSCALE_DEVICE_ID");
            env::remove_var("TOKSCALE_DEVICE_NAME");
        }

        store_device_name("Desk").unwrap();
        let device = resolve_submit_device().unwrap();

        assert!(device.id.starts_with("dev_"));
        assert_eq!(device.name.as_deref(), Some("Desk"));
    }
}

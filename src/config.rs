use anyhow::{Context, Result, ensure};
use argon2::{Algorithm, Argon2, Params, Version};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub device_id: String,
    pub layout: crate::layout::Layout,
    pub name: String,
    pub peer_address: String,
    pub port: u16,
    pub edge_switch: bool,
    pub edge_switch_condition: crate::switching::EdgeSwitchCondition,
    pub keyboard: bool,
    pub relative_mouse: bool,
    pub auto_reconnect: bool,
    pub auto_connect: bool,
    pub saved_connection: Option<crate::pairing::SavedConnection>,
    pub block_corners: bool,
    pub auto_clipboard: bool,
    pub transfer_rate_mib: u32,
    pub receive_directory: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            device_id: new_device_id(),
            layout: crate::layout::Layout::default(),
            name: std::env::var("COMPUTERNAME").unwrap_or_else(|_| "My PC".into()),
            peer_address: String::new(),
            port: 24810,
            edge_switch: true,
            edge_switch_condition: crate::switching::EdgeSwitchCondition::Always,
            keyboard: true,
            relative_mouse: false,
            auto_reconnect: true,
            auto_connect: true,
            saved_connection: None,
            block_corners: false,
            auto_clipboard: true,
            transfer_rate_mib: 8,
            receive_directory: std::env::var_os("USERPROFILE")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."))
                .join("Downloads")
                .join("MWB Received"),
        }
    }
}

impl Config {
    pub fn path() -> PathBuf {
        // MWB_CONFIG_DIR allows isolated test/demo profiles without touching real settings.
        let root = std::env::var_os("MWB_CONFIG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                std::env::var_os("LOCALAPPDATA")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join("MWB-Rust")
            });
        root.join("settings.json")
    }

    pub fn load() -> Result<Self> {
        let path = Self::path();
        if !path.exists() {
            return Ok(Self::default());
        }
        serde_json::from_slice(&fs::read(&path)?).context("配置文件无法读取")
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&Self::path())
    }

    fn save_to(&self, path: &Path) -> Result<()> {
        fs::create_dir_all(path.parent().context("配置目录无效")?)?;
        let temporary = path.with_extension(format!("{}.tmp", new_device_id()));
        let result = (|| -> Result<()> {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            file.write_all(&serde_json::to_vec_pretty(self)?)?;
            file.sync_all()?;
            drop(file);
            fs::rename(&temporary, path).context("替换配置文件失败")?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    pub fn restore_connection(&self) -> Result<Option<crate::pairing::RestoredConnection>> {
        self.saved_connection
            .as_ref()
            .map(|saved| saved.restore(&self.device_id))
            .transpose()
    }
}

pub const MAX_PAIRING_KEY_CHARS: usize = 64;

fn new_device_id() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("Windows secure random source unavailable");
    hex::encode(bytes)
}

// Fixed protocol domain salt: peers derive the same PSK without transmitting the
// passphrase. This is not a PAKE; short secrets remain vulnerable to guessing.
const PAIRING_SALT: &[u8] = b"mwb-rust/pairing/argon2id/v1";

pub fn new_pairing_key() -> Result<String> {
    new_pairing_key_with_length(MAX_PAIRING_KEY_CHARS)
}

pub fn new_pairing_key_with_length(length: usize) -> Result<String> {
    ensure!(
        (1..=MAX_PAIRING_KEY_CHARS).contains(&length),
        "随机密钥位数须为 1–64"
    );
    let mut key = [0u8; 32];
    getrandom::fill(&mut key).map_err(|e| anyhow::anyhow!("随机数生成失败: {e}"))?;
    let mut text = hex::encode(key);
    text.truncate(length);
    Ok(text)
}

pub fn validate_pairing_key(value: &str) -> Result<()> {
    let value = value.trim();
    ensure!(!value.is_empty(), "请输入配对密钥");
    ensure!(
        value.chars().count() <= MAX_PAIRING_KEY_CHARS,
        "配对密钥最多 64 个字符"
    );
    Ok(())
}

pub fn parse_pairing_key(value: &str) -> Result<[u8; 32]> {
    validate_pairing_key(value)?;
    let value = value.trim();
    let mut result = [0; 32];
    // Preserve the original interpretation for existing 256-bit random keys.
    if value.len() == 64
        && let Ok(key) = hex::decode(value)
    {
        result.copy_from_slice(&key);
        return Ok(result);
    }

    // Parameters are part of the pairing format and must match on both peers.
    let params =
        Params::new(19 * 1024, 2, 1, Some(32)).map_err(|e| anyhow::anyhow!("配对参数无效：{e}"))?;
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into(value.as_bytes(), PAIRING_SALT, &mut result)
        .map_err(|e| anyhow::anyhow!("无法准备配对密钥：{e}"))?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_identity_and_free_positions_survive_settings_roundtrip() {
        let mut config = Config::default();
        let mut info = crate::layout::test_device(1, 3840, 2160);
        info.id = config.device_id.clone();
        config.layout.upsert(info, true).unwrap();
        config.layout.devices[0].x = -2471;
        config.layout.devices[0].y = 369;
        let restored: Config =
            serde_json::from_slice(&serde_json::to_vec(&config).unwrap()).unwrap();
        assert_eq!(restored.device_id, config.device_id);
        assert_eq!(restored.layout, config.layout);
        assert_ne!(Config::default().device_id, config.device_id);
    }

    #[test]
    fn existing_settings_default_to_screen_relative_motion() {
        let config: Config = serde_json::from_str(r#"{"port":24810,"keyboard":false}"#).unwrap();
        assert!(!config.relative_mouse);
        assert!(!config.keyboard);
        assert!(config.auto_reconnect);
        assert!(config.auto_connect);
        assert!(config.saved_connection.is_none());
        assert!(config.restore_connection().unwrap().is_none());
        assert!(!config.block_corners);
        assert_eq!(
            config.edge_switch_condition,
            crate::switching::EdgeSwitchCondition::Always
        );
        let mut config = config;
        config.relative_mouse = true;
        config.auto_reconnect = false;
        config.block_corners = true;
        config.edge_switch_condition = crate::switching::EdgeSwitchCondition::Shift;
        let saved = serde_json::to_vec(&config).unwrap();
        let restored: Config = serde_json::from_slice(&saved).unwrap();
        assert!(!restored.auto_reconnect);
        assert!(restored.block_corners);
        assert_eq!(
            restored.edge_switch_condition,
            crate::switching::EdgeSwitchCondition::Shift
        );
        assert!(
            serde_json::from_slice::<Config>(&saved)
                .unwrap()
                .relative_mouse
        );
    }

    #[test]
    fn default_keys_remain_random_64_digit_hex() {
        let first = new_pairing_key().unwrap();
        let second = new_pairing_key().unwrap();
        assert_ne!(first, second);
        assert_eq!(first.len(), 64);
        assert_eq!(hex::decode(&first).unwrap().len(), 32);
        assert_eq!(
            parse_pairing_key(&first).unwrap().as_slice(),
            hex::decode(&first).unwrap()
        );
    }

    #[test]
    fn random_key_lengths_cover_short_odd_and_maximum_lengths() {
        for length in [1, 4, 6, 7, 8, 12, 63, 64] {
            let key = new_pairing_key_with_length(length).unwrap();
            assert_eq!(key.len(), length);
            assert!(key.bytes().all(|byte| byte.is_ascii_hexdigit()));
        }
        assert!(new_pairing_key_with_length(0).is_err());
        assert!(new_pairing_key_with_length(65).is_err());
        assert!(new_pairing_key_with_length(usize::MAX).is_err());
    }

    #[test]
    fn custom_keys_accept_text_and_reject_empty_or_overlong_values() {
        for value in [
            "1".to_string(),
            "1234".into(),
            "家里的电脑🔑".into(),
            "z".repeat(64),
            "密".repeat(64),
        ] {
            assert!(validate_pairing_key(&value).is_ok());
        }
        for value in [
            String::new(),
            " \t\n".into(),
            "x".repeat(65),
            "密".repeat(65),
        ] {
            assert!(parse_pairing_key(&value).is_err());
        }
    }

    #[test]
    fn existing_hex_keys_keep_their_bytes_and_case_insensitivity() {
        let key = "0123456789abcdef".repeat(4);
        let expected = hex::decode(&key).unwrap();
        assert_eq!(parse_pairing_key(&key).unwrap().as_slice(), expected);
        assert_eq!(
            parse_pairing_key(&key.to_uppercase()).unwrap().as_slice(),
            expected
        );
    }

    #[test]
    fn custom_keys_are_deterministic_trimmed_and_case_sensitive() {
        let key = parse_pairing_key("  家里-Ab1  ").unwrap();
        assert_eq!(key, parse_pairing_key("家里-Ab1").unwrap());
        assert_ne!(key, parse_pairing_key("家里-ab1").unwrap());
        assert_ne!(
            parse_pairing_key("1234").unwrap(),
            parse_pairing_key("1235").unwrap()
        );
    }

    #[test]
    fn settings_are_replaced_atomically_and_failed_save_keeps_previous_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        let mut config = Config::default();
        config.save_to(&path).unwrap();
        config.auto_connect = false;
        config.save_to(&path).unwrap();
        let restored: Config = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert!(!restored.auto_connect);
        assert_eq!(restored.device_id, config.device_id);
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);

        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            // An in-use destination must retain its previous complete contents.
            let locked = fs::OpenOptions::new()
                .read(true)
                .share_mode(0)
                .open(&path)
                .unwrap();
            config.auto_connect = true;
            assert!(config.save_to(&path).is_err());
            drop(locked);
            let unchanged: Config = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            assert!(!unchanged.auto_connect);
            assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
        }

        // A directory at the destination forces replacement to fail.
        let blocked = directory.path().join("blocked");
        fs::create_dir(&blocked).unwrap();
        assert!(config.save_to(&blocked).is_err());
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 2);
        let unchanged: Config = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(unchanged.device_id, config.device_id);
        assert!(!unchanged.auto_connect);
    }

    #[cfg(windows)]
    #[test]
    fn saved_pairing_opt_out_replacement_and_forgetting_survive_restart() {
        use crate::pairing::SavedConnection;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        let mut config = Config::default();
        config.saved_connection = Some(
            SavedConnection::new(
                "home-secret",
                Some("hub.local".into()),
                24811,
                &config.device_id,
            )
            .unwrap(),
        );
        config.auto_connect = false;
        config.save_to(&path).unwrap();
        let bytes = fs::read(&path).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("home-secret"));
        let mut restored: Config = serde_json::from_slice(&bytes).unwrap();
        assert!(!restored.auto_connect);
        let saved = restored.restore_connection().unwrap().unwrap();
        assert_eq!(saved.secret, "home-secret");
        assert_eq!(saved.target.as_deref(), Some("hub.local"));
        assert_eq!(saved.port, 24811);

        restored.saved_connection =
            Some(SavedConnection::new("new-secret", None, 24812, &restored.device_id).unwrap());
        restored.auto_connect = true;
        restored.save_to(&path).unwrap();
        let mut reopened: Config = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert!(reopened.auto_connect);
        let saved = reopened.restore_connection().unwrap().unwrap();
        assert_eq!(saved.secret, "new-secret");
        assert!(saved.target.is_none());
        assert_eq!(saved.port, 24812);

        reopened.saved_connection = None;
        reopened.save_to(&path).unwrap();
        let forgotten: Config = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert!(forgotten.restore_connection().unwrap().is_none());
        assert!(!String::from_utf8_lossy(&fs::read(&path).unwrap()).contains("dpapi-user-v1:"));
    }
}

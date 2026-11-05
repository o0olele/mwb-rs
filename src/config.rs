use anyhow::{Context, Result, ensure};
use argon2::{Algorithm, Argon2, Params, Version};
use serde::{Deserialize, Serialize};
use std::{fs, path::PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub name: String,
    pub peer_address: String,
    pub port: u16,
    pub peer_on_right: bool,
    pub edge_switch: bool,
    pub keyboard: bool,
    pub relative_mouse: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            name: std::env::var("COMPUTERNAME").unwrap_or_else(|_| "My PC".into()),
            peer_address: String::new(),
            port: 24810,
            peer_on_right: true,
            edge_switch: true,
            keyboard: true,
            relative_mouse: false,
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
        let path = Self::path();
        fs::create_dir_all(path.parent().context("配置目录无效")?)?;
        fs::write(path, serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }
}

pub const MAX_PAIRING_KEY_CHARS: usize = 64;

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
    fn existing_settings_default_to_screen_relative_motion() {
        let config: Config = serde_json::from_str(r#"{"port":24810,"keyboard":false}"#).unwrap();
        assert!(!config.relative_mouse);
        assert!(!config.keyboard);
        let mut config = config;
        config.relative_mouse = true;
        let saved = serde_json::to_vec(&config).unwrap();
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
}

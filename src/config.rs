use anyhow::{Context, Result, ensure};
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

pub fn new_pairing_key() -> Result<String> {
    let mut key = [0u8; 32];
    getrandom::fill(&mut key).map_err(|e| anyhow::anyhow!("随机数生成失败: {e}"))?;
    Ok(hex::encode(key))
}

pub fn parse_pairing_key(value: &str) -> Result<[u8; 32]> {
    let key = hex::decode(value.trim()).context("配对密钥必须为 64 位十六进制字符")?;
    ensure!(key.len() == 32, "配对密钥必须为 64 位十六进制字符");
    let mut result = [0; 32];
    result.copy_from_slice(&key);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_have_full_entropy_and_strict_length() {
        let first = new_pairing_key().unwrap();
        let second = new_pairing_key().unwrap();
        assert_ne!(first, second);
        assert!(parse_pairing_key(&first).is_ok());
        assert!(parse_pairing_key("abcd").is_err());
        assert!(parse_pairing_key(&"z".repeat(64)).is_err());
    }
}

//! Saved connection credentials are protected for the current Windows user.
use crate::config::validate_pairing_key;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SavedConnection {
    /// None restores a hub; Some(address) restores a member of that hub.
    pub target: Option<String>,
    pub port: u16,
    protected_key: String,
}

// Deliberately no Debug/Serialize: this value contains the plaintext credential.
pub struct RestoredConnection {
    pub secret: String,
    pub target: Option<String>,
    pub port: u16,
}

const PREFIX: &str = "dpapi-user-v1:";

impl SavedConnection {
    pub fn new(secret: &str, target: Option<String>, port: u16, device_id: &str) -> Result<Self> {
        validate_pairing_key(secret)?;
        let target = target.map(|address| address.trim().to_string());
        validate_endpoint(&target, port)?;
        let encrypted = protect(secret.trim().as_bytes(), device_id, true)?;
        Ok(Self {
            target,
            port,
            protected_key: format!("{PREFIX}{}", hex::encode(encrypted)),
        })
    }

    pub fn restore(&self, device_id: &str) -> Result<RestoredConnection> {
        validate_endpoint(&self.target, self.port)?;
        ensure!(self.protected_key.len() <= 16 * 1024, "保存的配对信息过大");
        let encoded = self
            .protected_key
            .strip_prefix(PREFIX)
            .context("无法识别保存的配对信息格式")?;
        let encrypted = hex::decode(encoded).context("保存的配对信息已损坏")?;
        let secret = String::from_utf8(protect(&encrypted, device_id, false)?)
            .context("保存的配对密钥无效")?;
        validate_pairing_key(&secret)?;
        Ok(RestoredConnection {
            secret,
            target: self.target.clone(),
            port: self.port,
        })
    }
}

fn validate_endpoint(target: &Option<String>, port: u16) -> Result<()> {
    ensure!(port != 0, "保存的连接端口无效");
    ensure!(
        target
            .as_ref()
            .is_none_or(|address| !address.trim().is_empty()),
        "保存的连接中心地址为空"
    );
    Ok(())
}

#[cfg(windows)]
fn protect(bytes: &[u8], device_id: &str, encrypt: bool) -> Result<Vec<u8>> {
    use windows::Win32::{
        Foundation::{HLOCAL, LocalFree},
        Security::Cryptography::{
            CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
        },
    };
    use windows_core::w;

    struct LocalBlob(CRYPT_INTEGER_BLOB);
    impl Drop for LocalBlob {
        fn drop(&mut self) {
            if !self.0.pbData.is_null() {
                unsafe {
                    // Clear the DPAPI allocation before returning it to Windows.
                    for byte in
                        std::slice::from_raw_parts_mut(self.0.pbData, self.0.cbData as usize)
                    {
                        std::ptr::write_volatile(byte, 0);
                    }
                    let _ = LocalFree(Some(HLOCAL(self.0.pbData.cast())));
                }
            }
        }
    }

    ensure!(!bytes.is_empty(), "保存的配对信息为空");
    let input = CRYPT_INTEGER_BLOB {
        cbData: bytes.len().try_into().context("配对信息过大")?,
        pbData: bytes.as_ptr().cast_mut(),
    };
    // Bind this application's credential to the stable local device identity.
    let entropy_bytes = format!("mwb-rust/pairing/dpapi/v1/{device_id}");
    let entropy = CRYPT_INTEGER_BLOB {
        cbData: entropy_bytes.len().try_into().context("设备标识过长")?,
        pbData: entropy_bytes.as_ptr().cast_mut(),
    };
    let mut output = LocalBlob(CRYPT_INTEGER_BLOB::default());
    unsafe {
        if encrypt {
            // No LOCAL_MACHINE flag: another Windows account cannot restore it.
            CryptProtectData(
                &input,
                w!("MWB Rust pairing"),
                Some(&entropy),
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output.0,
            )
        } else {
            CryptUnprotectData(
                &input,
                None,
                Some(&entropy),
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output.0,
            )
        }
        .context("Windows 无法保护或还原配对信息")?;
        ensure!(
            !output.0.pbData.is_null() && output.0.cbData != 0,
            "Windows 返回了空的配对信息"
        );
        Ok(std::slice::from_raw_parts(output.0.pbData, output.0.cbData as usize).to_vec())
    }
}

#[cfg(not(windows))]
fn protect(_: &[u8], _: &str, _: bool) -> Result<Vec<u8>> {
    anyhow::bail!("此平台不支持 Windows 用户配对信息保护")
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn protected_profiles_restore_hub_member_unicode_and_hex_keys() {
        for secret in ["家里的电脑-Ab1🔑".into(), "aB01".repeat(16)] {
            for target in [None, Some("  hub.local:24811  ".into())] {
                let saved =
                    SavedConnection::new(&secret, target.clone(), 24811, "local-id").unwrap();
                let json = serde_json::to_string(&saved).unwrap();
                assert!(!json.contains(&secret));
                assert!(!json.contains(&hex::encode(secret.as_bytes())));
                assert!(!format!("{saved:?}").contains(&secret));
                let loaded: SavedConnection = serde_json::from_str(&json).unwrap();
                let restored = loaded.restore("local-id").unwrap();
                assert_eq!(restored.secret, secret);
                assert_eq!(
                    restored.target,
                    target.map(|value| value.trim().to_string())
                );
                assert_eq!(restored.port, 24811);
            }
        }
    }

    #[test]
    fn protected_profiles_reject_other_identities_and_corrupt_credentials() {
        let saved = SavedConnection::new("1234", None, 24810, "local-id").unwrap();
        assert!(saved.restore("another-id").is_err());
        for protected_key in [
            "plaintext:1234",
            "dpapi-user-v1:zz",
            "dpapi-user-v1:00000000",
            PREFIX,
        ] {
            let corrupt = SavedConnection {
                protected_key: protected_key.into(),
                ..saved.clone()
            };
            assert!(corrupt.restore("local-id").is_err());
        }
        assert_eq!(saved.restore("local-id").unwrap().secret, "1234");
    }

    #[test]
    fn invalid_connection_details_are_not_remembered_or_restored() {
        assert!(SavedConnection::new(" ", None, 24810, "id").is_err());
        assert!(SavedConnection::new("key", Some("  ".into()), 24810, "id").is_err());
        assert!(SavedConnection::new("key", None, 0, "id").is_err());
        let mut saved = SavedConnection::new("  trimmed  ", None, 24810, "id").unwrap();
        assert_eq!(saved.restore("id").unwrap().secret, "trimmed");
        saved.target = Some(String::new());
        assert!(saved.restore("id").is_err());
    }
}

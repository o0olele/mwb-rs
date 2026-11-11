//! Opt-in, current-user logon startup. The Run entry is the source of truth;
//! ordinary launches never recreate an entry removed by the user or Windows.
use anyhow::{Context, Result, ensure};
use std::{os::windows::ffi::OsStrExt, path::Path};
use windows::{
    Win32::{
        Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND},
        System::Registry::*,
    },
    core::{PCWSTR, w},
};

const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const VALUE_NAME: PCWSTR = w!("MWB Rust");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Disabled,
    Enabled,
    OtherExecutable,
}

pub fn status() -> Result<Status> {
    RunEntry::current_user().status(&std::env::current_exe().context("读取程序位置")?)
}

pub fn set_enabled(enabled: bool) -> Result<Status> {
    let executable = std::env::current_exe().context("读取程序位置")?;
    let entry = RunEntry::current_user();
    entry.set_enabled(enabled, &executable)?;
    let status = entry.status(&executable)?;
    ensure!(
        status
            == if enabled {
                Status::Enabled
            } else {
                Status::Disabled
            },
        "自启动设置已被其他程序更改，请重试"
    );
    Ok(status)
}

/// Used only on a real MSI uninstall, before removing the executable. A portable
/// copy registered by this user must not lose its startup entry in the process.
pub fn remove_if_current() -> Result<()> {
    RunEntry::current_user().remove_if_matches(&std::env::current_exe()?)
}

fn command(executable: &Path) -> Result<Vec<u16>> {
    ensure!(executable.is_absolute(), "自启动程序路径必须为绝对路径");
    let path: Vec<u16> = executable.as_os_str().encode_wide().collect();
    ensure!(
        !path.iter().any(|c| *c == 0 || *c == b'"' as u16),
        "自启动程序路径包含无效字符"
    );
    let mut command = vec![b'"' as u16];
    command.extend(path);
    command.extend("\" --autostart".encode_utf16());
    // Windows documents a 260-character limit for Run command lines.
    ensure!(
        command.len() <= 260,
        "程序路径过长，请移到较短的路径后开启自启动"
    );
    command.push(0);
    Ok(command)
}

struct Key(HKEY);
impl Drop for Key {
    fn drop(&mut self) {
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}

struct RunEntry {
    root: HKEY,
    subkey: PCWSTR,
}

impl RunEntry {
    fn current_user() -> Self {
        Self {
            root: HKEY_CURRENT_USER,
            subkey: RUN_KEY,
        }
    }

    fn read(&self) -> Result<Option<Vec<u16>>> {
        let mut size = 0;
        let result = unsafe {
            RegGetValueW(
                self.root,
                self.subkey,
                VALUE_NAME,
                RRF_RT_REG_SZ,
                None,
                None,
                Some(&mut size),
            )
        };
        if result == ERROR_FILE_NOT_FOUND || result == ERROR_PATH_NOT_FOUND {
            return Ok(None);
        }
        result.ok().context("读取 Windows 自启动项")?;
        ensure!(size <= 64 * 1024, "Windows 自启动项内容过长");
        let mut text = vec![0u16; (size as usize).div_ceil(2)];
        unsafe {
            RegGetValueW(
                self.root,
                self.subkey,
                VALUE_NAME,
                RRF_RT_REG_SZ,
                None,
                Some(text.as_mut_ptr().cast()),
                Some(&mut size),
            )
            .ok()
            .context("读取 Windows 自启动命令")?;
        }
        ensure!(
            size >= 2 && size.is_multiple_of(2),
            "Windows 自启动命令格式无效"
        );
        text.truncate(size as usize / 2);
        ensure!(text.last() == Some(&0), "Windows 自启动命令未终止");
        Ok(Some(text))
    }

    fn status(&self, executable: &Path) -> Result<Status> {
        match self.read()? {
            None => Ok(Status::Disabled),
            Some(saved) if command(executable).is_ok_and(|expected| saved == expected) => {
                Ok(Status::Enabled)
            }
            Some(_) => Ok(Status::OtherExecutable),
        }
    }

    fn set_enabled(&self, enabled: bool, executable: &Path) -> Result<()> {
        if enabled {
            // Validate before opening/creating a key, preserving any old entry
            // when the executable cannot be registered.
            let bytes: Vec<u8> = command(executable)?
                .into_iter()
                .flat_map(u16::to_le_bytes)
                .collect();
            let mut key = HKEY::default();
            unsafe {
                RegCreateKeyExW(
                    self.root,
                    self.subkey,
                    None,
                    PCWSTR::null(),
                    REG_OPTION_NON_VOLATILE,
                    KEY_SET_VALUE,
                    None,
                    &mut key,
                    None,
                )
                .ok()
                .context("打开 Windows 自启动设置")?;
            }
            let key = Key(key);
            unsafe {
                RegSetValueExW(key.0, VALUE_NAME, None, REG_SZ, Some(&bytes))
                    .ok()
                    .context("保存 Windows 自启动项")?;
            }
        } else {
            let mut key = HKEY::default();
            let result =
                unsafe { RegOpenKeyExW(self.root, self.subkey, None, KEY_SET_VALUE, &mut key) };
            if result == ERROR_FILE_NOT_FOUND || result == ERROR_PATH_NOT_FOUND {
                return Ok(());
            }
            result.ok().context("打开 Windows 自启动设置")?;
            let key = Key(key);
            let result = unsafe { RegDeleteValueW(key.0, VALUE_NAME) };
            if result != ERROR_FILE_NOT_FOUND {
                result.ok().context("关闭 Windows 自启动")?;
            }
        }
        Ok(())
    }

    fn remove_if_matches(&self, executable: &Path) -> Result<()> {
        if self.status(executable)? == Status::Enabled {
            self.set_enabled(false, executable)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // No test reads or writes the real Run key or installs a startup task.
    struct TestEntry {
        subkey: Vec<u16>,
        _directory: tempfile::TempDir,
    }
    impl TestEntry {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let name = directory.path().file_name().unwrap().to_str().unwrap();
            let subkey = format!("Software\\MWB-Rust\\Tests\\{name}")
                .encode_utf16()
                .chain(Some(0))
                .collect();
            Self {
                subkey,
                _directory: directory,
            }
        }
        fn entry(&self) -> RunEntry {
            RunEntry {
                root: HKEY_CURRENT_USER,
                subkey: PCWSTR(self.subkey.as_ptr()),
            }
        }
    }
    impl Drop for TestEntry {
        fn drop(&mut self) {
            unsafe {
                let _ = RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(self.subkey.as_ptr()));
            }
        }
    }

    #[test]
    fn startup_command_quotes_unicode_and_spaces_without_secrets() {
        let path = Path::new(r"C:\Program Files\无界 协作\mwb.exe");
        let text = command(path).unwrap();
        assert_eq!(
            String::from_utf16(&text[..text.len() - 1]).unwrap(),
            "\"C:\\Program Files\\无界 协作\\mwb.exe\" --autostart"
        );
        for path in [
            "mwb.exe".to_owned(),
            "C:\\a\"b\\mwb.exe".into(),
            "C:\\a\0b.exe".into(),
            format!("C:\\{}\\mwb.exe", "a".repeat(260)),
        ] {
            assert!(command(Path::new(&path)).is_err());
        }
    }

    #[test]
    fn registration_survives_reopen_and_disable_only_removes_our_value() {
        let test = TestEntry::new();
        let executable = Path::new(r"C:\Program Files\MWB-Rust\mwb.exe");
        assert_eq!(test.entry().status(executable).unwrap(), Status::Disabled);
        test.entry().set_enabled(false, executable).unwrap();
        test.entry().set_enabled(true, executable).unwrap();
        assert_eq!(test.entry().status(executable).unwrap(), Status::Enabled);
        // An unrelated entry in the same key survives disabling MWB.
        let mut key = HKEY::default();
        unsafe {
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR(test.subkey.as_ptr()),
                None,
                KEY_SET_VALUE,
                &mut key,
            )
            .ok()
            .unwrap();
            let key = Key(key);
            RegSetValueExW(key.0, w!("Unrelated"), None, REG_SZ, Some(&[0, 0]))
                .ok()
                .unwrap();
        }
        test.entry().set_enabled(false, executable).unwrap();
        assert_eq!(test.entry().status(executable).unwrap(), Status::Disabled);
        let mut size = 0;
        unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                PCWSTR(test.subkey.as_ptr()),
                w!("Unrelated"),
                RRF_RT_REG_SZ,
                None,
                None,
                Some(&mut size),
            )
            .ok()
            .unwrap();
        }
        test.entry().set_enabled(false, executable).unwrap();
    }

    #[test]
    fn moving_program_is_explicit_and_uninstall_preserves_other_copy() {
        let test = TestEntry::new();
        let installed = Path::new(r"C:\Program Files\MWB-Rust\mwb.exe");
        let portable = Path::new(r"D:\Apps\MWB Rust\mwb.exe");
        test.entry().set_enabled(true, installed).unwrap();
        assert_eq!(
            test.entry().status(portable).unwrap(),
            Status::OtherExecutable
        );
        test.entry().set_enabled(true, portable).unwrap();
        test.entry().remove_if_matches(installed).unwrap();
        assert_eq!(test.entry().status(portable).unwrap(), Status::Enabled);
        test.entry().remove_if_matches(portable).unwrap();
        assert_eq!(test.entry().status(portable).unwrap(), Status::Disabled);
    }

    #[test]
    fn invalid_command_and_registry_error_preserve_saved_registration() {
        let test = TestEntry::new();
        let executable = Path::new(r"C:\MWB\mwb.exe");
        test.entry().set_enabled(true, executable).unwrap();
        assert!(
            test.entry()
                .set_enabled(true, Path::new("relative.exe"))
                .is_err()
        );
        assert_eq!(test.entry().status(executable).unwrap(), Status::Enabled);
        let invalid = RunEntry {
            root: HKEY::default(),
            subkey: PCWSTR::null(),
        };
        assert!(invalid.set_enabled(true, executable).is_err());
        assert!(invalid.set_enabled(false, executable).is_err());
        assert!(invalid.status(executable).is_err());
        assert_eq!(test.entry().status(executable).unwrap(), Status::Enabled);
    }
}

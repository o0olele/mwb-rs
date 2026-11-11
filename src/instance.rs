//! One desktop instance per settings directory, shared by installed and portable
//! copies. The OS releases the file handle even after an unclean process exit.
use anyhow::{Context, Result};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    os::windows::fs::OpenOptionsExt,
    path::Path,
};
use windows::Win32::{
    Foundation::{ERROR_SHARING_VIOLATION, HWND, LPARAM},
    Storage::FileSystem::FILE_SHARE_READ,
    UI::WindowsAndMessaging::*,
};

pub const WINDOW_TITLE: &str = "MWB · 无界协作";

pub struct InstanceGuard {
    _file: File,
}

pub enum Acquisition {
    Acquired(InstanceGuard),
    Running(Option<u32>),
}

pub fn acquire(config_path: &Path) -> Result<Acquisition> {
    let directory = config_path.parent().context("配置目录无效")?;
    fs::create_dir_all(directory).context("创建配置目录")?;
    let path = directory.join("mwb.lock");
    // Readers may inspect the PID; a second writer cannot acquire this handle.
    // Do not truncate until the exclusive writer handle is ours.
    match OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(FILE_SHARE_READ.0)
        .open(&path)
    {
        Ok(mut file) => {
            file.set_len(0)?;
            write!(file, "{}", std::process::id())?;
            file.flush()?;
            Ok(Acquisition::Acquired(InstanceGuard { _file: file }))
        }
        Err(error) if error.raw_os_error() == Some(ERROR_SHARING_VIOLATION.0 as i32) => {
            let pid = fs::read_to_string(&path)
                .ok()
                .and_then(|text| text.parse().ok());
            Ok(Acquisition::Running(pid))
        }
        Err(error) => Err(error).context("锁定 MWB 配置目录"),
    }
}

pub fn activate_existing(pid: u32) {
    unsafe {
        let _ = EnumWindows(Some(activate_window), LPARAM(pid as isize));
    }
}

unsafe extern "system" fn activate_window(hwnd: HWND, parameter: LPARAM) -> windows::core::BOOL {
    unsafe {
        let mut pid = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == parameter.0 as u32 && IsWindowVisible(hwnd).as_bool() {
            let mut title = [0; 128];
            let length = GetWindowTextW(hwnd, &mut title);
            if String::from_utf16_lossy(&title[..length as usize]) == WINDOW_TITLE {
                if IsIconic(hwnd).as_bool() {
                    let _ = ShowWindowAsync(hwnd, SW_RESTORE);
                }
                let _ = SetForegroundWindow(hwnd);
                return false.into();
            }
        }
    }
    true.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_writer_per_profile_and_release_allows_restart() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("profile/settings.json");
        let Acquisition::Acquired(guard) = acquire(&path).unwrap() else {
            panic!("first launch refused")
        };
        assert!(
            matches!(acquire(&path).unwrap(), Acquisition::Running(Some(pid)) if pid == std::process::id())
        );
        let other_path = directory.path().join("other/settings.json");
        let Acquisition::Acquired(_other) = acquire(&other_path).unwrap() else {
            panic!("separate profile refused")
        };
        drop(guard);
        assert!(matches!(acquire(&path).unwrap(), Acquisition::Acquired(_)));
        assert!(!path.exists());
    }

    #[test]
    fn stale_pid_is_replaced_but_invalid_directory_is_reported() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("mwb.lock"), "4294967295").unwrap();
        let path = directory.path().join("settings.json");
        let Acquisition::Acquired(_guard) = acquire(&path).unwrap() else {
            panic!("stale PID blocked restart")
        };
        assert_eq!(
            fs::read_to_string(directory.path().join("mwb.lock")).unwrap(),
            std::process::id().to_string()
        );
        let blocked = directory.path().join("blocked");
        fs::write(&blocked, "file").unwrap();
        assert!(acquire(&blocked.join("settings.json")).is_err());
    }
}

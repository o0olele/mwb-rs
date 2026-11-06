use anyhow::{Context, Result, ensure};
use std::{
    mem::size_of,
    path::{Path, PathBuf},
};
use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree},
        Security::{
            Authorization::{
                ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
                SDDL_REVISION_1,
            },
            *,
        },
        System::{
            RemoteDesktop::{ProcessIdToSessionId, WTSQueryUserToken},
            Threading::*,
        },
    },
    core::{PCWSTR, PWSTR},
};

pub(super) struct Handle(pub HANDLE);
// Owned kernel handles may be moved between threads; no borrowed pseudo handles
// or desktop handles are stored here.
unsafe impl Send for Handle {}
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

pub(super) struct Descriptor(PSECURITY_DESCRIPTOR);
impl Descriptor {
    pub fn new(sddl: &str) -> Result<Self> {
        let text = super::pipe::wide(sddl);
        let mut sd = PSECURITY_DESCRIPTOR::default();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(text.as_ptr()),
                SDDL_REVISION_1,
                &mut sd,
                None,
            )?;
        }
        Ok(Self(sd))
    }
    pub fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.0.0,
            bInheritHandle: false.into(),
        }
    }
}
impl Drop for Descriptor {
    fn drop(&mut self) {
        unsafe {
            LocalFree(Some(HLOCAL(self.0.0)));
        }
    }
}

pub(super) fn process_token(process: HANDLE) -> Result<Handle> {
    let mut token = HANDLE::default();
    unsafe {
        OpenProcessToken(process, TOKEN_QUERY, &mut token)?;
    }
    Ok(Handle(token))
}

fn token_data(token: HANDLE, class: TOKEN_INFORMATION_CLASS) -> Result<Vec<usize>> {
    let mut size = 0;
    unsafe {
        let _ = GetTokenInformation(token, class, None, 0, &mut size);
    }
    ensure!(size > 0 && size < 1024 * 1024, "无法读取进程令牌");
    // Token structures contain pointers and must be suitably aligned.
    let mut data = vec![0usize; (size as usize).div_ceil(size_of::<usize>())];
    unsafe {
        GetTokenInformation(
            token,
            class,
            Some(data.as_mut_ptr().cast()),
            size,
            &mut size,
        )?;
    }
    Ok(data)
}

pub(super) fn is_system(process: HANDLE) -> Result<bool> {
    let token = process_token(process)?;
    let data = token_data(token.0, TokenUser)?;
    let user = unsafe { &*data.as_ptr().cast::<TOKEN_USER>() };
    Ok(unsafe { IsWellKnownSid(user.User.Sid, WinLocalSystemSid).as_bool() })
}

pub(super) fn logon_sid(token: HANDLE) -> Result<String> {
    let data = token_data(token, TokenGroups)?;
    let groups = unsafe { &*data.as_ptr().cast::<TOKEN_GROUPS>() };
    let entries =
        unsafe { std::slice::from_raw_parts(groups.Groups.as_ptr(), groups.GroupCount as usize) };
    // SE_GROUP_LOGON_ID, specified by the token API (not a user/account SID).
    let entry = entries
        .iter()
        .find(|g| g.Attributes & 0xC0000000 == 0xC0000000)
        .context("令牌没有登录会话 SID")?;
    let mut text = PWSTR::null();
    unsafe {
        ConvertSidToStringSidW(entry.Sid, &mut text)?;
        let result = text.to_string();
        LocalFree(Some(HLOCAL(text.0.cast())));
        Ok(result?)
    }
}

pub(super) fn session_user(session: u32) -> Result<String> {
    let mut token = HANDLE::default();
    unsafe {
        WTSQueryUserToken(session, &mut token)?;
    }
    logon_sid(Handle(token).0)
}

pub(super) fn current_session() -> Result<u32> {
    let mut session = 0;
    unsafe {
        ProcessIdToSessionId(GetCurrentProcessId(), &mut session)?;
    }
    Ok(session)
}

pub(super) fn process_image(process: HANDLE) -> Result<PathBuf> {
    let mut text = vec![0u16; 32768];
    let mut size = text.len() as u32;
    unsafe {
        QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(text.as_mut_ptr()),
            &mut size,
        )?;
    }
    Ok(PathBuf::from(String::from_utf16(&text[..size as usize])?))
}

pub(super) fn sibling(name: &str) -> Result<PathBuf> {
    Ok(std::env::current_exe()?
        .parent()
        .context("程序目录无效")?
        .join(name))
}

pub(super) fn same_image(actual: &Path, expected: &Path) -> Result<bool> {
    let actual = actual.canonicalize()?;
    let expected = expected.canonicalize()?;
    Ok(actual
        .as_os_str()
        .to_string_lossy()
        .eq_ignore_ascii_case(&expected.as_os_str().to_string_lossy()))
}

/// Keep the process handle open for the entire pipe connection, preventing PID
/// reuse from changing the authenticated peer.
pub(super) fn verify_peer(
    pid: u32,
    session: u32,
    image: &Path,
    system: bool,
    user: Option<&str>,
) -> Result<Handle> {
    let process = Handle(unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            false,
            pid,
        )?
    });
    let mut actual_session = 0;
    unsafe {
        ProcessIdToSessionId(pid, &mut actual_session)?;
    }
    ensure!(actual_session == session, "输入服务拒绝跨会话连接");
    ensure!(is_system(process.0)? == system, "输入服务进程身份不匹配");
    ensure!(
        same_image(&process_image(process.0)?, image)?,
        "请使用安装目录中的 MWB 程序连接锁屏输入服务"
    );
    if let Some(user) = user {
        ensure!(
            logon_sid(process_token(process.0)?.0)? == user,
            "输入服务登录身份不匹配"
        );
    }
    Ok(process)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_identity_binds_process_image_account_and_logon_session() {
        let process = unsafe { GetCurrentProcess() };
        let pid = unsafe { GetCurrentProcessId() };
        let session = current_session().unwrap();
        let image = std::env::current_exe().unwrap();
        let system = is_system(process).unwrap();
        let user = logon_sid(process_token(process).unwrap().0).unwrap();
        assert!(verify_peer(pid, session, &image, system, Some(&user)).is_ok());
        assert!(verify_peer(pid, session.wrapping_add(1), &image, system, Some(&user)).is_err());
        assert!(verify_peer(pid, session, &image, !system, None).is_err());
        assert!(verify_peer(pid, session, &image, system, Some("S-1-5-5-0-0")).is_err());
        assert!(
            verify_peer(
                pid,
                session,
                &image.with_file_name("not-the-installed-app.exe"),
                system,
                None
            )
            .is_err()
        );
    }
}

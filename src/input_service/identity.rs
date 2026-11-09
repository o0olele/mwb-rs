use anyhow::{Context, Result, ensure};
use std::{
    mem::size_of,
    path::{Path, PathBuf},
};
use windows::{
    Win32::{
        Foundation::{CloseHandle, ERROR_SERVICE_DOES_NOT_EXIST, HANDLE, HLOCAL, LocalFree},
        Security::{
            Authorization::{
                ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
                SDDL_REVISION_1,
            },
            *,
        },
        System::{
            Diagnostics::ToolHelp::*,
            Pipes::{GetNamedPipeServerProcessId, GetNamedPipeServerSessionId},
            RemoteDesktop::{ProcessIdToSessionId, WTSQueryUserToken},
            Services::*,
            SystemServices::SECURITY_MANDATORY_SYSTEM_RID,
            Threading::*,
        },
        UI::Shell::CommandLineToArgvW,
    },
    core::{HRESULT, PCWSTR, PWSTR},
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
    fn apply_dacl(&self, object: HANDLE) -> Result<()> {
        unsafe {
            SetKernelObjectSecurity(object, DACL_SECURITY_INFORMATION, self.0)?;
        }
        Ok(())
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
        OpenProcessToken(process, TOKEN_QUERY, &mut token)
            .context("OpenProcessToken 读取对端账户令牌")?;
    }
    Ok(Handle(token))
}

pub(super) fn worker_process_descriptor(logon: &str) -> Result<Descriptor> {
    Descriptor::new(&format!(
        "O:SYG:SYD:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;0x00101000;;;{logon})"
    ))
}

/// Permit only image/session queries and waiting. The client's trust proof uses
/// the SCM-owned service and worker lineage; no SYSTEM token query is needed.
pub(super) fn allow_worker_verification(logon: &str) -> Result<()> {
    worker_process_descriptor(logon)?
        .apply_dacl(unsafe { GetCurrentProcess() })
        .context("设置输入代理进程查询权限")?;
    Ok(())
}

struct ServiceHandle(SC_HANDLE);
// SCM handles have no thread affinity and are exclusively owned by this value.
unsafe impl Send for ServiceHandle {}
impl Drop for ServiceHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseServiceHandle(self.0);
        }
    }
}

pub(super) struct VerifiedWorker {
    _process: Handle,
    _service: ServiceHandle,
}

struct RegisteredService {
    handle: ServiceHandle,
    image: PathBuf,
}

fn service_command_image(command: &str) -> Result<PathBuf> {
    let command = super::pipe::wide(command);
    let mut count = 0;
    let args = unsafe { CommandLineToArgvW(PCWSTR(command.as_ptr()), &mut count) };
    if args.is_null() {
        return Err(windows::core::Error::from_win32()).context("解析输入服务启动命令");
    }
    let result = (|| {
        ensure!(count == 2, "输入服务启动命令必须仅包含程序路径和 --service");
        let args = unsafe { std::slice::from_raw_parts(args, 2) };
        ensure!(
            unsafe { args[1].to_string()? } == "--service",
            "输入服务启动参数不匹配"
        );
        let image = PathBuf::from(unsafe { args[0].to_string()? });
        ensure!(image.is_absolute(), "输入服务程序路径必须为绝对路径");
        Ok(image)
    })();
    unsafe {
        LocalFree(Some(HLOCAL(args.cast())));
    }
    result
}

fn registered_service_config() -> Result<Option<RegisteredService>> {
    let manager = ServiceHandle(unsafe {
        OpenSCManagerW(None, None, SC_MANAGER_CONNECT).context("连接本机服务管理器（只读）")?
    });
    let service_name = super::pipe::wide(super::SERVICE_NAME);
    let service = match unsafe {
        OpenServiceW(
            manager.0,
            PCWSTR(service_name.as_ptr()),
            SERVICE_QUERY_CONFIG | SERVICE_QUERY_STATUS,
        )
    } {
        Ok(handle) => ServiceHandle(handle),
        Err(error) if error.code() == HRESULT::from_win32(ERROR_SERVICE_DOES_NOT_EXIST.0) => {
            return Ok(None);
        }
        Err(error) => return Err(error).context("读取已注册的 MWB 输入服务"),
    };
    let mut needed = 0;
    unsafe {
        let _ = QueryServiceConfigW(service.0, None, 0, &mut needed);
    }
    ensure!(needed > 0 && needed <= 64 * 1024, "输入服务配置长度无效");
    let mut buffer = vec![0usize; (needed as usize).div_ceil(size_of::<usize>())];
    unsafe {
        QueryServiceConfigW(
            service.0,
            Some(buffer.as_mut_ptr().cast()),
            needed,
            &mut needed,
        )
        .context("读取输入服务程序和账户配置")?;
    }
    let config = unsafe { &*buffer.as_ptr().cast::<QUERY_SERVICE_CONFIGW>() };
    let account = unsafe { config.lpServiceStartName.to_string()? };
    ensure!(
        account.eq_ignore_ascii_case("LocalSystem")
            || account.eq_ignore_ascii_case(r"NT AUTHORITY\SYSTEM"),
        "输入服务未使用 LocalSystem 账户"
    );
    let command = unsafe { config.lpBinaryPathName.to_string()? };
    let image = service_command_image(&command)?;
    Ok(Some(RegisteredService {
        handle: service,
        image,
    }))
}

pub(super) fn installation_service_image() -> Result<Option<PathBuf>> {
    Ok(registered_service_config()?.map(|service| service.image))
}

fn registered_service() -> Result<Option<RegisteredService>> {
    let Some(mut service) = registered_service_config()? else {
        return Ok(None);
    };
    service.image = service
        .image
        .canonicalize()
        .context("解析输入服务安装路径")?;
    ensure!(
        service
            .image
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.eq_ignore_ascii_case(super::SERVICE_EXE)),
        "输入服务程序名称不匹配"
    );
    Ok(Some(service))
}

fn installed_client(service: &RegisteredService) -> Result<PathBuf> {
    Ok(service
        .image
        .parent()
        .context("服务安装目录无效")?
        .join(super::CLIENT_EXE))
}

pub(super) fn installed_client_path() -> Result<Option<PathBuf>> {
    registered_service()?
        .map(|service| installed_client(&service))
        .transpose()
}

fn running_service_pid(service: SC_HANDLE) -> Result<u32> {
    let mut status = SERVICE_STATUS_PROCESS::default();
    let bytes = unsafe {
        std::slice::from_raw_parts_mut(
            (&mut status as *mut SERVICE_STATUS_PROCESS).cast(),
            size_of::<SERVICE_STATUS_PROCESS>(),
        )
    };
    let mut needed = 0;
    unsafe {
        QueryServiceStatusEx(service, SC_STATUS_PROCESS_INFO, Some(bytes), &mut needed)
            .context("查询输入服务运行状态")?;
    }
    ensure!(
        status.dwCurrentState == SERVICE_RUNNING && status.dwProcessId != 0,
        "锁屏输入服务尚未运行"
    );
    Ok(status.dwProcessId)
}

fn parent_process(pid: u32) -> Result<u32> {
    let snapshot = Handle(unsafe {
        CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).context("读取进程父子关系")?
    });
    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    unsafe {
        Process32FirstW(snapshot.0, &mut entry)?;
        loop {
            if entry.th32ProcessID == pid {
                return Ok(entry.th32ParentProcessID);
            }
            if Process32NextW(snapshot.0, &mut entry).is_err() {
                break;
            }
        }
    }
    anyhow::bail!("输入代理进程已退出")
}

fn verify_worker_parent(worker: u32, service: u32) -> Result<()> {
    ensure!(
        service != 0 && parent_process(worker)? == service,
        "输入代理并非由已注册的系统服务启动"
    );
    Ok(())
}

/// Non-elevated clients cannot reliably open another account's primary token.
/// Instead, bind the worker's image/session/parent to the running LocalSystem
/// service registered in SCM. Forging this parent requires PROCESS_CREATE_PROCESS
/// on the SYSTEM service, a right never granted to the interactive user.
pub(super) fn verify_worker(pipe: HANDLE, session: u32, image: &Path) -> Result<VerifiedWorker> {
    let mut pid = 0;
    let mut actual_session = 0;
    unsafe {
        GetNamedPipeServerProcessId(pipe, &mut pid).context("从管道读取输入代理进程编号")?;
        GetNamedPipeServerSessionId(pipe, &mut actual_session).context("从管道读取输入代理会话")?;
    }
    ensure!(session == actual_session, "输入服务拒绝跨会话连接");
    let service = registered_service()?.context("尚未安装 MWB 锁屏输入服务")?;
    ensure!(
        same_image(&service.image, image).context("核验输入服务安装路径")?,
        "当前程序目录与输入服务安装目录不一致；请关闭当前 MWB 后运行：{}",
        installed_client(&service)?.display()
    );
    let service_pid = running_service_pid(service.handle.0)?;
    let process = Handle(unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            false,
            pid,
        )
        .context("打开输入代理以核验程序路径")?
    });
    ensure!(
        same_image(
            &process_image(process.0).context("读取输入代理程序路径")?,
            image
        )?,
        "输入代理程序路径不匹配"
    );
    verify_worker_parent(pid, service_pid)?;
    ensure!(
        running_service_pid(service.handle.0)? == service_pid,
        "身份核验期间输入服务发生了重启"
    );
    Ok(VerifiedWorker {
        _process: process,
        _service: service.handle,
    })
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

#[derive(Debug)]
pub(super) struct InputTokenInfo {
    pub system: bool,
    pub session: u32,
    pub integrity: u32,
    pub ui_access: bool,
}

impl InputTokenInfo {
    pub fn validate_worker(&self, session: u32) -> Result<()> {
        ensure!(self.system, "输入代理必须使用 SYSTEM 身份");
        ensure!(
            self.session == session && session != 0,
            "输入代理令牌会话不匹配"
        );
        ensure!(
            self.integrity >= SECURITY_MANDATORY_SYSTEM_RID as u32,
            "输入代理令牌完整性级别不足"
        );
        ensure!(
            self.ui_access,
            "输入代理令牌缺少 UIAccess，请更新并重启输入服务"
        );
        Ok(())
    }
}

fn token_u32(token: HANDLE, class: TOKEN_INFORMATION_CLASS) -> Result<u32> {
    let mut value = 0u32;
    let mut length = 0;
    unsafe {
        GetTokenInformation(
            token,
            class,
            Some((&mut value as *mut u32).cast()),
            size_of::<u32>() as u32,
            &mut length,
        )?;
    }
    ensure!(length as usize == size_of::<u32>(), "令牌信息长度无效");
    Ok(value)
}

/// Read only non-secret process attributes. Never include logon/account SIDs,
/// credentials or input payloads in the service diagnostic snapshot.
pub(super) fn input_token_info(token: HANDLE) -> Result<InputTokenInfo> {
    let user_data = token_data(token, TokenUser)?;
    let user = unsafe { &*user_data.as_ptr().cast::<TOKEN_USER>() };
    let label_data = token_data(token, TokenIntegrityLevel)?;
    let label = unsafe { &*label_data.as_ptr().cast::<TOKEN_MANDATORY_LABEL>() };
    ensure!(
        unsafe { IsValidSid(label.Label.Sid).as_bool() },
        "令牌完整性 SID 无效"
    );
    let count = unsafe { *GetSidSubAuthorityCount(label.Label.Sid) };
    ensure!(count > 0, "令牌完整性 SID 为空");
    Ok(InputTokenInfo {
        system: unsafe { IsWellKnownSid(user.User.Sid, WinLocalSystemSid).as_bool() },
        session: token_u32(token, TokenSessionId)?,
        integrity: unsafe { *GetSidSubAuthority(label.Label.Sid, u32::from(count - 1)) },
        ui_access: token_u32(token, TokenUIAccess)? != 0,
    })
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
        )
        .context("OpenProcess 打开对端进程进行身份核验")?
    });
    let mut actual_session = 0;
    unsafe {
        ProcessIdToSessionId(pid, &mut actual_session).context("读取对端 Windows 会话")?;
    }
    ensure!(actual_session == session, "输入服务拒绝跨会话连接");
    ensure!(
        is_system(process.0).context("核验对端 SYSTEM 账户身份")? == system,
        "输入服务进程身份不匹配"
    );
    ensure!(
        same_image(
            &process_image(process.0).context("读取对端可执行文件路径")?,
            image
        )
        .context("核验对端安装路径")?,
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
    fn input_token_snapshot_matches_real_process_identity_and_session() {
        let process = unsafe { GetCurrentProcess() };
        let token = process_token(process).unwrap();
        let info = input_token_info(token.0).unwrap();
        assert_eq!(info.system, is_system(process).unwrap());
        assert_eq!(info.session, current_session().unwrap());
        assert!(info.integrity > 0);
        assert_eq!(
            info.ui_access,
            token_u32(token.0, TokenUIAccess).unwrap() != 0
        );
    }

    #[test]
    fn input_worker_requires_system_session_integrity_and_ui_access_together() {
        let valid = || InputTokenInfo {
            system: true,
            session: 1,
            integrity: SECURITY_MANDATORY_SYSTEM_RID as u32,
            ui_access: true,
        };
        valid().validate_worker(1).unwrap();
        let mut legacy = valid();
        legacy.ui_access = false;
        assert!(
            legacy
                .validate_worker(1)
                .unwrap_err()
                .to_string()
                .contains("UIAccess")
        );
        let mut user = valid();
        user.system = false;
        assert!(user.validate_worker(1).is_err());
        let mut low = valid();
        low.integrity -= 1;
        assert!(low.validate_worker(1).is_err());
        assert!(valid().validate_worker(2).is_err());
        let mut service = valid();
        service.session = 0;
        assert!(service.validate_worker(0).is_err());
    }

    #[test]
    fn service_commands_accept_windows_quoting_but_reject_other_entrypoints() {
        let image = PathBuf::from(r"C:\Program Files\MWB-Rust\mwb-input-service.exe");
        assert_eq!(
            service_command_image(r#""C:\Program Files\MWB-Rust\mwb-input-service.exe" --service"#)
                .unwrap(),
            image
        );
        assert_eq!(
            service_command_image(r#"C:\MWB\mwb-input-service.exe "--service""#).unwrap(),
            PathBuf::from(r"C:\MWB\mwb-input-service.exe")
        );
        for command in [
            "",
            r"C:\MWB\mwb-input-service.exe",
            r"C:\MWB\mwb-input-service.exe --worker",
            r"C:\MWB\mwb-input-service.exe --service --worker",
            r"mwb-input-service.exe --service",
            r"\MWB\mwb-input-service.exe --service",
            r"C:MWB\mwb-input-service.exe --service",
        ] {
            assert!(service_command_image(command).is_err(), "{command}");
        }
    }

    #[test]
    fn image_paths_accept_aliases_but_reject_a_portable_copy() {
        let root = tempfile::tempdir().unwrap();
        let installed = root.path().join("installed");
        let portable = root.path().join("portable");
        std::fs::create_dir_all(installed.join("subdirectory")).unwrap();
        std::fs::create_dir(&portable).unwrap();
        let image = installed.join(super::super::SERVICE_EXE);
        std::fs::write(&image, b"identity fixture, never executed").unwrap();
        let copy = portable.join(super::super::SERVICE_EXE);
        std::fs::copy(&image, &copy).unwrap();
        let alias = installed
            .join("subdirectory")
            .join("..")
            .join("MWB-INPUT-SERVICE.EXE");
        assert!(same_image(&image, &alias).unwrap());
        let extended = image.canonicalize().unwrap();
        let command = format!("\"{}\" --service", extended.display());
        assert!(same_image(&service_command_image(&command).unwrap(), &image).unwrap());
        assert!(!same_image(&image, &copy).unwrap());
    }

    fn restricted_logon_identity() -> (Handle, String) {
        let mut source = HANDLE::default();
        unsafe {
            OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_QUERY | TOKEN_DUPLICATE,
                &mut source,
            )
            .unwrap();
        }
        let source = Handle(source);
        let logon = logon_sid(source.0).unwrap();
        let data = token_data(source.0, TokenGroups).unwrap();
        let groups = unsafe { &*data.as_ptr().cast::<TOKEN_GROUPS>() };
        let entries = unsafe {
            std::slice::from_raw_parts(groups.Groups.as_ptr(), groups.GroupCount as usize)
        };
        let sid = entries
            .iter()
            .find(|g| g.Attributes & 0xC0000000 == 0xC0000000)
            .unwrap()
            .Sid;
        let mut restricted = HANDLE::default();
        let mut identity = HANDLE::default();
        unsafe {
            // Restrict the ACL check to this logon SID even when the test runner
            // is an administrator. No live process/token permissions are changed.
            CreateRestrictedToken(
                source.0,
                DISABLE_MAX_PRIVILEGE,
                None,
                None,
                Some(&[SID_AND_ATTRIBUTES {
                    Sid: sid,
                    Attributes: 0,
                }]),
                &mut restricted,
            )
            .unwrap();
        }
        let restricted = Handle(restricted);
        unsafe {
            DuplicateTokenEx(
                restricted.0,
                TOKEN_QUERY,
                None,
                SecurityImpersonation,
                TokenImpersonation,
                &mut identity,
            )
            .unwrap();
        }
        (Handle(identity), logon)
    }

    fn process_access(descriptor: &Descriptor, identity: HANDLE, access: u32) -> bool {
        let mapping = GENERIC_MAPPING {
            GenericRead: PROCESS_QUERY_LIMITED_INFORMATION.0 | PROCESS_SYNCHRONIZE.0,
            GenericWrite: PROCESS_VM_WRITE.0,
            GenericExecute: PROCESS_SYNCHRONIZE.0,
            GenericAll: PROCESS_ALL_ACCESS.0,
        };
        let mut privileges = [0usize; 64];
        let mut length = size_of_val(&privileges) as u32;
        let mut granted = 0;
        let mut allowed = windows::core::BOOL::default();
        unsafe {
            AccessCheck(
                descriptor.0,
                identity,
                access,
                &mapping,
                Some(privileges.as_mut_ptr().cast()),
                &mut length,
                &mut granted,
                &mut allowed,
            )
            .unwrap();
        }
        allowed.as_bool()
    }

    #[test]
    fn worker_permissions_allow_identity_queries_but_reject_process_injection_and_parent_spoofing()
    {
        let (identity, logon) = restricted_logon_identity();
        let descriptor = worker_process_descriptor(&logon).unwrap();
        assert!(process_access(
            &descriptor,
            identity.0,
            PROCESS_QUERY_LIMITED_INFORMATION.0 | PROCESS_SYNCHRONIZE.0
        ));
        assert!(!process_access(&descriptor, identity.0, PROCESS_VM_WRITE.0));
        assert!(!process_access(
            &descriptor,
            identity.0,
            PROCESS_CREATE_PROCESS.0
        ));
        let other = worker_process_descriptor("S-1-5-5-0-0").unwrap();
        assert!(!process_access(
            &other,
            identity.0,
            PROCESS_QUERY_LIMITED_INFORMATION.0
        ));
    }

    #[test]
    fn worker_lineage_uses_the_os_parent_and_rejects_unrelated_service_pids() {
        let pid = unsafe { GetCurrentProcessId() };
        let parent = parent_process(pid).unwrap();
        verify_worker_parent(pid, parent).unwrap();
        assert!(verify_worker_parent(pid, pid).is_err());
        assert!(verify_worker_parent(pid, 0).is_err());
    }

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

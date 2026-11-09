use super::{
    CLIENT_EXE, SERVICE_NAME,
    desktop::DesktopInput,
    identity::{self, Handle},
    pipe::{self, LEASE, Pipe},
    pipe_name,
    session::Session,
    wire::{Command, Reply, Request, Response, VERSION},
};
use anyhow::{Context, Result, bail, ensure};
use std::{
    io::Write,
    mem::size_of,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use windows::{
    Win32::{
        Foundation::*,
        Security::*,
        System::{
            Diagnostics::ToolHelp::*,
            JobObjects::*,
            Pipes::GetNamedPipeClientProcessId,
            RemoteDesktop::{ProcessIdToSessionId, WTSGetActiveConsoleSessionId},
            Services::*,
            SystemInformation::GetSystemDirectoryW,
            Threading::*,
        },
    },
    core::{PCWSTR, PWSTR},
};

static STOP: AtomicBool = AtomicBool::new(false);
static STATUS: AtomicUsize = AtomicUsize::new(0);

fn log(error: &anyhow::Error) {
    record(&format!("{error:#}"));
}

fn record(message: &str) {
    // Never log input packets, key values, credentials or clipboard contents.
    eprintln!("{message}");
    if let Ok(path) = identity::sibling("input-service.log") {
        // Bound the diagnostic file. Only SYSTEM/admin can write in this folder.
        let append = std::fs::metadata(&path).is_ok_and(|m| m.len() < 1024 * 1024);
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .append(append)
            .truncate(!append)
            .open(path)
        {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let _ = writeln!(file, "{now}: {message}");
        }
    }
}

pub fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [flag, client] if flag == "--check-installation" => {
            super::installation::check(std::path::Path::new(client))
        }
        [flag] if flag == "--service" => {
            let name = pipe::wide(SERVICE_NAME);
            let entries = [
                SERVICE_TABLE_ENTRYW {
                    lpServiceName: PWSTR(name.as_ptr().cast_mut()),
                    lpServiceProc: Some(service_main),
                },
                SERVICE_TABLE_ENTRYW::default(),
            ];
            unsafe {
                StartServiceCtrlDispatcherW(entries.as_ptr())?;
            }
            Ok(())
        }
        [flag, session, parent, stop_event] if flag == "--worker" => {
            let result = worker(session.parse()?, parent.parse()?, stop_event);
            if let Err(error) = &result {
                log(error);
            }
            result
        }
        _ => bail!("请运行 MWB 安装包安装或卸载；开发环境也可使用 scripts/input-service.ps1"),
    }
}

fn status(state: SERVICE_STATUS_CURRENT_STATE, code: u32) {
    let handle = SERVICE_STATUS_HANDLE(STATUS.load(Ordering::SeqCst) as *mut _);
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: if state == SERVICE_RUNNING {
            SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN
        } else {
            0
        },
        dwWin32ExitCode: code,
        dwCheckPoint: u32::from(state == SERVICE_START_PENDING || state == SERVICE_STOP_PENDING),
        dwWaitHint: if state == SERVICE_START_PENDING || state == SERVICE_STOP_PENDING {
            5000
        } else {
            0
        },
        ..Default::default()
    };
    unsafe {
        let _ = SetServiceStatus(handle, &status);
    }
}

unsafe extern "system" fn control(
    code: u32,
    _: u32,
    _: *mut std::ffi::c_void,
    _: *mut std::ffi::c_void,
) -> u32 {
    match code {
        SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
            STOP.store(true, Ordering::SeqCst);
            status(SERVICE_STOP_PENDING, 0);
        }
        SERVICE_CONTROL_INTERROGATE => status(
            if STOP.load(Ordering::SeqCst) {
                SERVICE_STOP_PENDING
            } else {
                SERVICE_RUNNING
            },
            0,
        ),
        _ => return ERROR_CALL_NOT_IMPLEMENTED.0,
    }
    0
}

unsafe extern "system" fn service_main(_: u32, _: *mut PWSTR) {
    let name = pipe::wide(SERVICE_NAME);
    let Ok(handle) =
        (unsafe { RegisterServiceCtrlHandlerExW(PCWSTR(name.as_ptr()), Some(control), None) })
    else {
        return;
    };
    STATUS.store(handle.0 as usize, Ordering::SeqCst);
    status(SERVICE_START_PENDING, 0);
    let result = supervise();
    let code = if let Err(error) = result {
        log(&error);
        ERROR_GEN_FAILURE.0
    } else {
        0
    };
    status(SERVICE_STOPPED, code);
}

fn enable_privileges(names: &[PCWSTR]) -> Result<()> {
    ensure!(
        identity::is_system(unsafe { GetCurrentProcess() })?,
        "输入服务必须由 LocalSystem 运行"
    );
    let mut token = HANDLE::default();
    unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_QUERY | TOKEN_ADJUST_PRIVILEGES,
            &mut token,
        )?;
    }
    let token = Handle(token);
    for &name in names {
        let mut luid = LUID::default();
        unsafe {
            LookupPrivilegeValueW(None, name, &mut luid)?;
            let privileges = TOKEN_PRIVILEGES {
                PrivilegeCount: 1,
                Privileges: [LUID_AND_ATTRIBUTES {
                    Luid: luid,
                    Attributes: SE_PRIVILEGE_ENABLED,
                }],
            };
            SetLastError(ERROR_SUCCESS);
            AdjustTokenPrivileges(token.0, false, Some(&privileges), 0, None, None)?;
            ensure!(GetLastError() == ERROR_SUCCESS, "输入服务缺少会话启动权限");
        }
    }
    Ok(())
}

/// Preserve the interactive SYSTEM security context, as the original service
/// does. Changing only TokenSessionId on a session-0 service token does not
/// reproduce the token of the target session's logon process.
fn session_input_token(session: u32) -> Result<Handle> {
    ensure!(
        session != 0 && session == unsafe { WTSGetActiveConsoleSessionId() },
        "只能为当前物理控制台创建输入代理"
    );
    let mut directory = vec![0u16; 32768];
    let length = unsafe { GetSystemDirectoryW(Some(&mut directory)) } as usize;
    ensure!(
        length > 0 && length < directory.len(),
        "无法读取 Windows 系统目录"
    );
    let image =
        std::path::PathBuf::from(String::from_utf16(&directory[..length])?).join("winlogon.exe");
    let snapshot = Handle(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)? });
    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    unsafe { Process32FirstW(snapshot.0, &mut entry)? };
    loop {
        let end = entry
            .szExeFile
            .iter()
            .position(|c| *c == 0)
            .unwrap_or(entry.szExeFile.len());
        if String::from_utf16_lossy(&entry.szExeFile[..end]).eq_ignore_ascii_case("winlogon.exe") {
            let mut actual_session = 0;
            if unsafe { ProcessIdToSessionId(entry.th32ProcessID, &mut actual_session) }.is_ok()
                && actual_session == session
            {
                // Verify the full system image, account and session, and keep
                // its handle alive while opening the token to prevent PID reuse.
                let process =
                    identity::verify_peer(entry.th32ProcessID, session, &image, true, None)
                        .context("核验当前会话的 Winlogon")?;
                let mut source = HANDLE::default();
                unsafe {
                    OpenProcessToken(process.0, TOKEN_DUPLICATE | TOKEN_QUERY, &mut source)
                        .context("读取当前会话的 Winlogon 令牌")?;
                }
                let source = Handle(source);
                let mut token = HANDLE::default();
                unsafe {
                    DuplicateTokenEx(
                        source.0,
                        TOKEN_QUERY | TOKEN_DUPLICATE | TOKEN_ASSIGN_PRIMARY | TOKEN_ADJUST_DEFAULT,
                        None,
                        SecurityImpersonation,
                        TokenPrimary,
                        &mut token,
                    )
                    .context("创建会话输入代理令牌")?;
                }
                let token = Handle(token);
                // UIAccess is a separate token attribute from SYSTEM identity
                // and integrity. Set it on the new primary token before launch,
                // using the supervisor's enabled SeTcbPrivilege. Never modify
                // the Winlogon token itself or grant this token to the client.
                let ui_access = 1u32;
                unsafe {
                    SetTokenInformation(
                        token.0,
                        TokenUIAccess,
                        (&ui_access as *const u32).cast(),
                        size_of::<u32>() as u32,
                    )
                    .context("为输入代理令牌启用 UIAccess")?;
                }
                identity::input_token_info(token.0)?.validate_worker(session)?;
                return Ok(token);
            }
        }
        if unsafe { Process32NextW(snapshot.0, &mut entry) }.is_err() {
            break;
        }
    }
    bail!("当前物理控制台的 Winlogon 尚未就绪")
}

struct Worker {
    session: u32,
    user: String,
    process: Handle,
    stop: Handle,
    _job: Handle,
}
impl Worker {
    fn launch(session: u32, user: String) -> Result<Self> {
        let exe = std::env::current_exe()?;
        let exe_text = exe.to_str().context("输入服务路径无效")?;
        let executable = pipe::wide(exe_text);
        let directory = pipe::wide(exe.parent().unwrap().to_str().context("输入服务目录无效")?);
        let parent = unsafe { GetCurrentProcessId() };
        let stop_name = format!(
            r"Global\MWB-Rust.Input.Stop.{parent}.{}",
            crate::config::new_pairing_key()?
        );
        let stop_name_wide = pipe::wide(&stop_name);
        let stop =
            Handle(unsafe { CreateEventW(None, true, false, PCWSTR(stop_name_wide.as_ptr()))? });
        let mut command = pipe::wide(&format!(
            "\"{exe_text}\" --worker {session} {parent} {stop_name}"
        ));
        let token = session_input_token(session)?;
        // Clients query only the worker's image/session and SCM-owned parent.
        // Its SYSTEM token keeps the default SYSTEM/administrator permissions.
        let process_sd = identity::worker_process_descriptor(&user)?;
        let job = Handle(unsafe { CreateJobObjectW(None, None)? });
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )?;
        }
        // Bootstrap on the secure desktop, like the original Winlogon agent.
        // The windowless input thread can subsequently follow Default/Winlogon
        // after removing its hooks; the service itself remains in session 0.
        let mut desktop = pipe::wide(r"winsta0\winlogon");
        let startup = STARTUPINFOW {
            cb: size_of::<STARTUPINFOW>() as u32,
            lpDesktop: PWSTR(desktop.as_mut_ptr()),
            ..Default::default()
        };
        let mut process = PROCESS_INFORMATION::default();
        unsafe {
            CreateProcessAsUserW(
                Some(token.0),
                PCWSTR(executable.as_ptr()),
                Some(PWSTR(command.as_mut_ptr())),
                Some(&process_sd.attributes()),
                None,
                false,
                CREATE_SUSPENDED | CREATE_NO_WINDOW,
                None,
                PCWSTR(directory.as_ptr()),
                &startup,
                &mut process,
            )
            .context("在 Winlogon 桌面启动会话输入代理")?;
        }
        let child = Handle(process.hProcess);
        let child_thread = Handle(process.hThread);
        let started = (|| -> Result<()> {
            unsafe {
                AssignProcessToJobObject(job.0, child.0)?;
                ensure!(
                    ResumeThread(child_thread.0) != u32::MAX,
                    "无法启动会话输入代理"
                );
            }
            Ok(())
        })();
        if let Err(error) = started {
            unsafe {
                let _ = TerminateProcess(child.0, 1);
            }
            return Err(error);
        }
        Ok(Self {
            session,
            user,
            process: child,
            stop,
            _job: job,
        })
    }
    fn alive(&self) -> bool {
        unsafe { WaitForSingleObject(self.process.0, 0) == WAIT_TIMEOUT }
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        unsafe {
            let _ = SetEvent(self.stop.0);
            // Give the agent time to cancel I/O and release pressed keys before
            // the job's kill-on-close fallback handles an unresponsive process.
            let _ = WaitForSingleObject(self.process.0, 3500);
        }
    }
}

fn supervise() -> Result<()> {
    enable_privileges(&[
        SE_TCB_NAME,
        SE_ASSIGNPRIMARYTOKEN_NAME,
        SE_INCREASE_QUOTA_NAME,
        SE_DEBUG_NAME,
    ])?;
    status(SERVICE_RUNNING, 0);
    let mut worker: Option<Worker> = None;
    let mut retry = Instant::now();
    while !STOP.load(Ordering::SeqCst) {
        let session = unsafe { WTSGetActiveConsoleSessionId() };
        let user = identity::session_user(session).ok();
        if worker
            .as_ref()
            .is_some_and(|w| w.session != session || Some(&w.user) != user.as_ref() || !w.alive())
        {
            worker = None;
        }
        if worker.is_none()
            && let Some(user) = user
            && Instant::now() >= retry
        {
            match Worker::launch(session, user) {
                Ok(started) => worker = Some(started),
                Err(error) => log(&error),
            }
            retry = Instant::now() + Duration::from_secs(3);
        }
        thread::sleep(Duration::from_millis(250));
    }
    drop(worker);
    Ok(())
}

fn worker(session: u32, parent: u32, stop_name: &str) -> Result<()> {
    // The worker only queries the console logon identity. Process/token launch
    // privileges are required by its session-0 supervisor, not this input loop.
    enable_privileges(&[SE_TCB_NAME])?;
    ensure!(
        identity::current_session()? == session && session != 0,
        "输入代理会话无效"
    );
    let token = identity::process_token(unsafe { GetCurrentProcess() })?;
    identity::input_token_info(token.0)?.validate_worker(session)?;
    let service = identity::verify_peer(parent, 0, &std::env::current_exe()?, true, None)?;
    let stop_name = pipe::wide(stop_name);
    let stop = Handle(unsafe {
        OpenEventW(
            SYNCHRONIZATION_ACCESS_RIGHTS(0x00100000),
            false,
            PCWSTR(stop_name.as_ptr()),
        )?
    });
    let user = identity::session_user(session)?;
    identity::allow_worker_verification(&user)?;
    let pipe = Pipe::create(&pipe_name(session), &user)?;
    record(&format!(
        "input worker ready [{}]",
        super::desktop::diagnostics()
    ));
    let stopping = || unsafe {
        WaitForSingleObject(stop.0, 0) != WAIT_TIMEOUT
            || WaitForSingleObject(service.0, 0) != WAIT_TIMEOUT
            || WTSGetActiveConsoleSessionId() != session
    };
    while !stopping() && identity::session_user(session).ok().as_deref() == Some(&user) {
        match pipe.accept(Duration::from_millis(250)) {
            Err(error) if pipe::timed_out(&error) => continue,
            Err(error) => return Err(error),
            Ok(()) => {}
        }
        let result = (|| -> Result<()> {
            let mut pid = 0;
            unsafe {
                GetNamedPipeClientProcessId(pipe.handle.0, &mut pid)?;
            }
            let client = identity::verify_peer(
                pid,
                session,
                &identity::sibling(CLIENT_EXE)?,
                false,
                Some(&user),
            )?;
            let mut control = Session::new(DesktopInput::new()?);
            while !stopping() && unsafe { WaitForSingleObject(client.0, 0) == WAIT_TIMEOUT } {
                pipe::pump();
                let request: Request = pipe.receive(LEASE)?;
                // An always-ready pipe can complete synchronously. Explicitly
                // pump here too so high-rate input cannot starve takeover hooks.
                pipe::pump();
                // Reject packets when a console user changes even if the old
                // client survives for a moment during logoff.
                ensure!(
                    !stopping() && identity::session_user(session)? == user,
                    "登录会话已切换"
                );
                let sequence = request.sequence;
                let release = matches!(request.command, Command::Release);
                let result = control.handle(request);
                let failed = result.is_err();
                let reply = match result {
                    Ok(reply) => reply,
                    Err(error) => {
                        // Input failures are returned as a reply rather than an
                        // outer I/O error, so record them here before teardown.
                        log(&error);
                        Reply::Error(format!("{error:#}"))
                    }
                };
                pipe.send(&Response {
                    version: VERSION,
                    sequence,
                    result: reply,
                })?;
                if release || failed {
                    break;
                }
            }
            Ok(())
        })();
        // Session drops first: release all keys/buttons even on malformed IPC,
        // process exit, watchdog expiry, service stop or a failed reply.
        pipe.disconnect();
        if let Err(error) = result {
            let closed = error
                .downcast_ref::<windows::core::Error>()
                .is_some_and(|e| e.code() == ERROR_BROKEN_PIPE.to_hresult());
            if !closed {
                log(&error);
            }
        }
    }
    Ok(())
}

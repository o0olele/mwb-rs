use super::{
    identity::{Descriptor, Handle},
    wire::{self, MAX_MESSAGE},
};
use anyhow::{Result, bail, ensure};
use serde::{Serialize, de::DeserializeOwned};
use std::time::{Duration, Instant};
use windows::{
    Win32::{
        Foundation::*,
        Storage::FileSystem::*,
        System::{IO::*, Pipes::*, Threading::CreateEventW},
        UI::WindowsAndMessaging::*,
    },
    core::PCWSTR,
};

pub(super) const TIMEOUT: Duration = Duration::from_millis(750);
pub(super) const LEASE: Duration = Duration::from_millis(1500);

pub(super) fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

pub(super) fn pump() {
    unsafe {
        let mut message = MSG::default();
        // Low-level hooks are dispatched here even while overlapped pipe I/O is
        // pending. Never block a hook thread waiting for a remote packet.
        while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

pub(super) fn timed_out(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|e| e.kind() == std::io::ErrorKind::TimedOut)
}

pub(super) struct Pipe {
    pub handle: Handle,
}

impl Pipe {
    pub fn create(name: &str, logon: &str) -> Result<Self> {
        let descriptor = Descriptor::new(&format!("D:P(A;;GA;;;SY)(A;;0x0012019b;;;{logon})"))?;
        let name = wide(name);
        unsafe {
            let handle = CreateNamedPipeW(
                PCWSTR(name.as_ptr()),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                MAX_MESSAGE as u32,
                MAX_MESSAGE as u32,
                0,
                Some(&descriptor.attributes()),
            );
            let error = windows::core::Error::from_win32();
            if handle.is_invalid() {
                return Err(error.into());
            }
            Ok(Self {
                handle: Handle(handle),
            })
        }
    }

    pub fn open(name: &str) -> Result<Option<Self>> {
        let name = wide(name);
        let deadline = Instant::now() + Duration::from_millis(250);
        let result = loop {
            let result = unsafe {
                CreateFileW(
                    PCWSTR(name.as_ptr()),
                    (FILE_READ_DATA
                        | FILE_WRITE_DATA
                        | FILE_READ_ATTRIBUTES
                        | FILE_WRITE_ATTRIBUTES)
                        .0
                        | 0x00100000,
                    FILE_SHARE_MODE(0),
                    None,
                    OPEN_EXISTING,
                    FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                    None,
                )
            };
            if result
                .as_ref()
                .is_err_and(|e| e.code() == ERROR_PIPE_BUSY.to_hresult())
                && Instant::now() < deadline
            {
                // A just-released connection may still be unwinding its hooks.
                // Wait briefly for the single server instance to accept again.
                unsafe {
                    let _ = WaitNamedPipeW(PCWSTR(name.as_ptr()), 25);
                }
                continue;
            }
            break result;
        };
        let handle = match result {
            Ok(handle) => Handle(handle),
            Err(error) if error.code() == ERROR_FILE_NOT_FOUND.to_hresult() => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        unsafe {
            SetNamedPipeHandleState(handle.0, Some(&PIPE_READMODE_MESSAGE), None, None)?;
        }
        Ok(Some(Self { handle }))
    }

    fn operation(
        &self,
        timeout: Duration,
        start: impl FnOnce(&mut OVERLAPPED, &mut u32) -> windows::core::Result<()>,
    ) -> Result<u32> {
        let event = Handle(unsafe { CreateEventW(None, true, false, None)? });
        let mut overlapped = OVERLAPPED {
            hEvent: event.0,
            ..Default::default()
        };
        let mut transferred = 0;
        match start(&mut overlapped, &mut transferred) {
            Ok(()) => return Ok(transferred),
            Err(error) if error.code() == ERROR_IO_PENDING.to_hresult() => {}
            Err(error) => return Err(error.into()),
        }
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let wait = unsafe {
                MsgWaitForMultipleObjects(
                    Some(&[event.0]),
                    false,
                    remaining.as_millis().min(50) as u32,
                    QS_ALLINPUT,
                )
            };
            if wait == WAIT_OBJECT_0 {
                unsafe {
                    GetOverlappedResult(self.handle.0, &overlapped, &mut transferred, false)?;
                }
                return Ok(transferred);
            }
            if wait == WAIT_FAILED {
                break;
            }
            pump();
        }
        // Drain cancellation before the stack OVERLAPPED or caller's buffer is
        // freed. If completion raced the deadline, keep the completed message.
        unsafe {
            let _ = CancelIoEx(self.handle.0, Some(&overlapped));
            if GetOverlappedResult(self.handle.0, &overlapped, &mut transferred, true).is_ok() {
                return Ok(transferred);
            }
        }
        Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "输入服务响应超时").into())
    }

    pub fn accept(&self, timeout: Duration) -> Result<()> {
        self.operation(timeout, |overlapped, _| unsafe {
            match ConnectNamedPipe(self.handle.0, Some(overlapped)) {
                Err(error) if error.code() == ERROR_PIPE_CONNECTED.to_hresult() => Ok(()),
                result => result,
            }
        })?;
        Ok(())
    }

    pub fn disconnect(&self) {
        unsafe {
            let _ = DisconnectNamedPipe(self.handle.0);
        }
    }

    pub fn receive<T: DeserializeOwned>(&self, timeout: Duration) -> Result<T> {
        let mut bytes = [0u8; MAX_MESSAGE];
        let count = self.operation(timeout, |overlapped, count| unsafe {
            ReadFile(
                self.handle.0,
                Some(&mut bytes),
                Some(count),
                Some(overlapped),
            )
        })?;
        ensure!(count > 0, "输入服务连接已关闭");
        wire::decode(&bytes[..count as usize])
    }

    pub fn send(&self, value: &impl Serialize) -> Result<()> {
        let bytes = wire::encode(value)?;
        let count = self.operation(TIMEOUT, |overlapped, count| unsafe {
            WriteFile(self.handle.0, Some(&bytes), Some(count), Some(overlapped))
        })?;
        if count as usize != bytes.len() {
            bail!("输入服务消息未完整写入");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input_service::{
        identity,
        wire::{Command, Reply, Request, Response, VERSION},
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, GetCurrentProcessId};

    #[test]
    fn local_pipe_roundtrip_and_cancellation_have_bounded_lifetimes() {
        let sid = identity::logon_sid(
            identity::process_token(unsafe { GetCurrentProcess() })
                .unwrap()
                .0,
        )
        .unwrap();
        let name = format!(
            r"\\.\pipe\MWB-Rust.Test.{}.{}",
            unsafe { GetCurrentProcessId() },
            crate::config::new_pairing_key().unwrap()
        );
        let server = Pipe::create(&name, &sid).unwrap();
        assert!(timed_out(
            &server.accept(Duration::from_millis(20)).unwrap_err()
        ));
        let worker = std::thread::spawn(move || {
            server.accept(TIMEOUT).unwrap();
            let request: Request = server.receive(TIMEOUT).unwrap();
            assert_eq!(request.sequence, 42);
            server
                .send(&Response {
                    version: VERSION,
                    sequence: request.sequence,
                    result: Reply::Ok,
                })
                .unwrap();
            let _: Request = server.receive(TIMEOUT).unwrap();
        });
        let client = Pipe::open(&name).unwrap().unwrap();
        client
            .send(&Request {
                version: VERSION,
                sequence: 42,
                command: Command::Hello,
            })
            .unwrap();
        let response: Response = client.receive(TIMEOUT).unwrap();
        assert_eq!(response.sequence, 42);
        assert!(matches!(response.result, Reply::Ok));
        client
            .send(&Request {
                version: VERSION,
                sequence: 43,
                command: Command::Release,
            })
            .unwrap();
        worker.join().unwrap();
    }
}

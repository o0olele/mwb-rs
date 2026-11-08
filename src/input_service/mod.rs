//! Optional, receive-only input broker. No network, clipboard, files or pairing
//! secrets are handled by the privileged process.
mod client;
mod desktop;
mod identity;
mod pipe;
mod service;
mod session;
mod wire;

pub(crate) use client::Client;
pub use service::run;

const SERVICE_NAME: &str = "MwbRustInput";
const SERVICE_EXE: &str = "mwb-input-service.exe";
const CLIENT_EXE: &str = "mwb.exe";

/// Best-effort, read-only startup guidance. An absent or inaccessible optional
/// service must not prevent ordinary desktop sharing.
pub fn installation_notice() -> Option<String> {
    let installed = identity::installed_client_path().ok()??;
    let current = std::env::current_exe().ok()?;
    if identity::same_image(&current, &installed).ok()? {
        return None;
    }
    Some(format!(
        "本机管理员窗口和锁屏控制需使用安装版。请关闭此窗口后运行：{}",
        installed.display()
    ))
}

fn pipe_name(session: u32) -> String {
    format!(r"\\.\pipe\MWB-Rust.Input.v1.{session}")
}

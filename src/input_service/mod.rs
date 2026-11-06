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

fn pipe_name(session: u32) -> String {
    format!(r"\\.\pipe\MWB-Rust.Input.v1.{session}")
}

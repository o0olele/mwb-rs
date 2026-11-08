#[cfg(windows)]
mod receiver;
#[cfg(windows)]
mod windows;
use std::{error::Error, fmt};
#[cfg(windows)]
pub use windows::*;

/// A failed OS injection, distinct from malformed network input.
#[derive(Debug)]
pub(crate) struct InputInjectionError {
    input: &'static str,
    inserted: u32,
    code: u32,
}

impl InputInjectionError {
    pub(crate) fn new(input: &'static str, inserted: u32, code: u32) -> Self {
        Self {
            input,
            inserted,
            code,
        }
    }

    pub(crate) fn nothing_inserted(&self) -> bool {
        self.inserted == 0
    }
}

impl fmt::Display for InputInjectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Windows {}输入注入失败：SendInput 返回 {}/1，Win32 错误码 {}",
            self.input, self.inserted, self.code
        )?;
        if self.code == 0 {
            write!(f, "（系统未提供更多错误信息）")
        } else {
            write!(
                f,
                "（{}）",
                std::io::Error::from_raw_os_error(self.code as i32)
            )
        }
    }
}

impl Error for InputInjectionError {}

#[derive(Debug)]
pub(crate) struct InputServiceError(pub String);
impl fmt::Display for InputServiceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl Error for InputServiceError {}

#[derive(Debug)]
pub(crate) struct LocalInputTakeover;
impl fmt::Display for LocalInputTakeover {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("被控端的本地键鼠已接管")
    }
}
impl Error for LocalInputTakeover {}

use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

pub fn validate_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.encode_utf16().count() <= 255
            && !name.ends_with(['.', ' '])
            && !name
                .chars()
                .any(|c| c.is_control() || "<>:\"/\\|?*".contains(c)),
        "文件名无效"
    );
    let stem = name
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end()
        .to_ascii_uppercase();
    ensure!(
        !matches!(
            stem.as_str(),
            "." | ".." | "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$"
        ) && !((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.len() == 4
            && stem.as_bytes()[3].is_ascii_digit()),
        "不允许的 Windows 文件名"
    );
    Ok(())
}

pub struct IncomingFile {
    file: Option<File>,
    temp: PathBuf,
    directory: PathBuf,
    name: String,
    expected: u64,
    pub received: u64,
    hash: Sha256,
}
impl IncomingFile {
    pub fn create(directory: &Path, name: &str, id: &str, expected: u64) -> Result<Self> {
        validate_name(name)?;
        ensure!(super::wire::valid_id(id), "传输标识无效");
        fs::create_dir_all(directory).context("无法创建接收目录")?;
        let directory = directory.canonicalize()?;
        let temp = directory.join(format!(".mwb-{id}.part"));
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .context("无法创建临时接收文件")?;
        Ok(Self {
            file: Some(file),
            temp,
            directory,
            name: name.into(),
            expected,
            received: 0,
            hash: Sha256::new(),
        })
    }
    pub fn write(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        ensure!(
            offset == self.received
                && !data.is_empty()
                && self
                    .received
                    .checked_add(data.len() as u64)
                    .is_some_and(|n| n <= self.expected),
            "文件块顺序或长度错误"
        );
        self.file
            .as_mut()
            .context("文件已关闭")?
            .write_all(data)
            .context("文件写入失败，请检查磁盘空间和权限")?;
        self.hash.update(data);
        self.received += data.len() as u64;
        Ok(())
    }
    pub fn finish(mut self, digest: &str) -> Result<PathBuf> {
        ensure!(
            self.received == self.expected && hex::encode(self.hash.clone().finalize()) == digest,
            "文件长度或 SHA-256 校验失败"
        );
        self.file.as_mut().context("文件已关闭")?.sync_all()?;
        self.file.take();
        // Commit without replacement. A concurrent transfer cannot overwrite an existing file.
        for number in 0..10000 {
            let name = if number == 0 {
                self.name.clone()
            } else {
                let p = Path::new(&self.name);
                let stem = p.file_stem().unwrap_or_default().to_string_lossy();
                let ext = p
                    .extension()
                    .map(|e| format!(".{}", e.to_string_lossy()))
                    .unwrap_or_default();
                format!("{stem} ({number}){ext}")
            };
            let destination = self.directory.join(name);
            match move_without_replacement(&self.temp, &destination) {
                Ok(()) => return Ok(destination),
                Err(_) if destination.exists() => continue,
                Err(e) => return Err(e).context("无法完成接收文件"),
            }
        }
        bail!("同名文件过多，请选择其他目录")
    }
}
impl Drop for IncomingFile {
    fn drop(&mut self) {
        self.file.take();
        let _ = fs::remove_file(&self.temp);
    }
}
#[cfg(windows)]
fn move_without_replacement(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    let a: Vec<_> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let b: Vec<_> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe {
        windows::Win32::Storage::FileSystem::MoveFileW(
            windows::core::PCWSTR(a.as_ptr()),
            windows::core::PCWSTR(b.as_ptr()),
        )
    }
    .map_err(|e| std::io::Error::from_raw_os_error(e.code().0 & 0xffff))
}
#[cfg(not(windows))]
fn move_without_replacement(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::hard_link(from, to)?;
    fs::remove_file(from)
}

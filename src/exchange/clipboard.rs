use super::{
    Command,
    wire::{ClipboardVersion, Kind},
};
use crate::engine::Runtime;
use anyhow::{Result, bail, ensure};
use sha2::{Digest, Sha256};
use std::{
    borrow::Cow,
    path::PathBuf,
    sync::{
        Arc,
        atomic::Ordering,
        mpsc::{Receiver, SyncSender},
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Debug)]
pub struct Content {
    pub kind: Kind,
    pub data: Arc<Vec<u8>>,
}
impl Content {
    #[cfg(test)]
    pub fn image(width: u32, height: u32, data: Vec<u8>) -> Self {
        Self {
            kind: Kind::Image { width, height },
            data: Arc::new(data),
        }
    }
    pub fn text(text: String) -> Self {
        Self {
            kind: Kind::Text,
            data: Arc::new(text.into_bytes()),
        }
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(!self.kind.is_file(), "剪贴板内容类型无效");
        self.kind.validate(self.data.len() as u64)?;
        if self.kind == Kind::Text {
            std::str::from_utf8(&self.data)?;
        }
        Ok(())
    }
    fn digest(&self) -> Vec<u8> {
        let mut hash = Sha256::new();
        hash.update(serde_json::to_vec(&self.kind).expect("serializable clipboard kind"));
        hash.update(self.data.as_slice());
        hash.finalize().to_vec()
    }
}
pub(crate) trait Backend: Send + 'static {
    fn sequence(&mut self) -> u64;
    fn read(&mut self) -> Result<Option<Content>>;
    fn write(&mut self, content: &Content) -> Result<()>;
    fn files(&mut self) -> Result<Vec<PathBuf>> {
        bail!("剪贴板中没有可发送的文件")
    }
}
pub(crate) struct SystemClipboard;
impl Backend for SystemClipboard {
    fn sequence(&mut self) -> u64 {
        unsafe { windows::Win32::System::DataExchange::GetClipboardSequenceNumber() as u64 }
    }
    fn read(&mut self) -> Result<Option<Content>> {
        let mut board = arboard::Clipboard::new()?;
        let content = match board.get_image() {
            Ok(image) => Content {
                kind: Kind::Image {
                    width: image.width.try_into()?,
                    height: image.height.try_into()?,
                },
                data: Arc::new(image.bytes.into_owned()),
            },
            Err(arboard::Error::ContentNotAvailable) => match board.get_text() {
                Ok(text) => Content::text(text),
                Err(arboard::Error::ContentNotAvailable) => return Ok(None),
                Err(e) => return Err(e.into()),
            },
            Err(e) => return Err(e.into()),
        };
        content.validate()?;
        Ok(Some(content))
    }
    fn write(&mut self, content: &Content) -> Result<()> {
        content.validate()?;
        let mut board = arboard::Clipboard::new()?;
        match content.kind {
            Kind::Text => board.set_text(std::str::from_utf8(&content.data)?)?,
            Kind::Image { width, height } => board.set_image(arboard::ImageData {
                width: width as usize,
                height: height as usize,
                bytes: Cow::Borrowed(&content.data),
            })?,
            Kind::File { .. } => bail!("文件不写入系统剪贴板"),
        }
        Ok(())
    }
    fn files(&mut self) -> Result<Vec<PathBuf>> {
        use windows::Win32::{
            System::DataExchange::{CloseClipboard, GetClipboardData, OpenClipboard},
            UI::Shell::{DragQueryFileW, HDROP},
        };
        struct Opened;
        impl Drop for Opened {
            fn drop(&mut self) {
                unsafe {
                    let _ = CloseClipboard();
                }
            }
        }
        unsafe {
            OpenClipboard(None)?;
            let _opened = Opened;
            let data = GetClipboardData(15)?; // CF_HDROP: paths copied in Explorer.
            let drop = HDROP(data.0);
            let count = DragQueryFileW(drop, u32::MAX, None);
            ensure!(count > 0 && count <= 64, "请选择 1–64 个文件");
            let mut files = Vec::new();
            for index in 0..count {
                let length = DragQueryFileW(drop, index, None);
                ensure!(length > 0 && length <= 32767, "文件路径过长");
                let mut text = vec![0; length as usize + 1];
                let written = DragQueryFileW(drop, index, Some(&mut text));
                ensure!(written == length, "读取剪贴板文件失败");
                files.push(PathBuf::from(String::from_utf16(&text[..length as usize])?));
            }
            Ok(files)
        }
    }
}
pub(crate) enum Request {
    Read {
        target: String,
    },
    Files {
        target: String,
    },
    Apply {
        version: Option<ClipboardVersion>,
        content: Content,
    },
}

pub(super) fn launch(
    rt: Arc<Runtime>,
    mut backend: Box<dyn Backend>,
    requests: Receiver<Request>,
    commands: SyncSender<Command>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut seen = backend.sequence();
        let mut digest = Vec::new();
        let mut poll = Instant::now();
        let mut last_error = Instant::now() - Duration::from_secs(5);
        while !rt.stop.load(Ordering::SeqCst) {
            let active = rt.sharing_enabled()
                && rt.exchange.ready()
                && rt.exchange.auto_clipboard.load(Ordering::SeqCst);
            if poll.elapsed() >= Duration::from_millis(150) {
                poll = Instant::now();
                let sequence = backend.sequence();
                if !active {
                    seen = sequence;
                } else if sequence != seen {
                    match backend.read() {
                        Ok(content) if backend.sequence() == sequence => {
                            seen = sequence;
                            if let Some(content) = content
                                && content.digest() != digest
                            {
                                digest = content.digest();
                                let version = {
                                    let mut version = rt.exchange.clipboard_version.lock().unwrap();
                                    version.clock = rt
                                        .exchange
                                        .clipboard_clock
                                        .fetch_add(1, Ordering::SeqCst)
                                        .saturating_add(1);
                                    version.origin = rt.local.id.clone();
                                    version.clone()
                                };
                                if commands
                                    .try_send(Command::Clipboard {
                                        target: None,
                                        content,
                                        version: Some(version),
                                    })
                                    .is_err()
                                {
                                    rt.log("剪贴板同步队列忙，请稍后重新复制");
                                }
                            }
                        }
                        Ok(_) => {}
                        Err(e) if last_error.elapsed() > Duration::from_secs(5) => {
                            last_error = Instant::now();
                            rt.log(format!("读取剪贴板失败，键鼠连接保持：{e:#}"));
                        }
                        Err(_) => {}
                    }
                }
            }
            for request in requests.try_iter().take(4) {
                let result = (|| -> Result<()> {
                    match request {
                        Request::Read { target } => {
                            ensure!(rt.sharing_enabled(), "请先开启共享");
                            let content = backend
                                .read()?
                                .ok_or_else(|| anyhow::anyhow!("剪贴板中没有文本或图片"))?;
                            commands
                                .try_send(Command::Clipboard {
                                    target: Some(target),
                                    content,
                                    version: None,
                                })
                                .map_err(|_| anyhow::anyhow!("剪贴板队列忙"))?;
                        }
                        Request::Files { target } => {
                            commands
                                .try_send(Command::Files {
                                    target,
                                    paths: backend.files()?,
                                })
                                .map_err(|_| anyhow::anyhow!("文件队列忙"))?;
                        }
                        Request::Apply { version, content } => {
                            if !rt.sharing_enabled() || (version.is_some() && !active) {
                                return Ok(());
                            }
                            if let Some(ref version) = version {
                                if *rt.exchange.clipboard_version.lock().unwrap() != *version {
                                    return Ok(());
                                }
                                // A local copy after our last observation takes precedence over an in-flight transfer.
                                if backend.sequence() != seen {
                                    poll = Instant::now() - Duration::from_secs(1);
                                    return Ok(());
                                }
                            }
                            let mut result = backend.write(&content);
                            for _ in 0..4 {
                                if result.is_ok() || rt.stop.load(Ordering::SeqCst) {
                                    break;
                                }
                                thread::sleep(Duration::from_millis(25));
                                if backend.sequence() != seen {
                                    break;
                                }
                                result = backend.write(&content);
                            }
                            result?;
                            seen = backend.sequence();
                            digest = content.digest();
                        }
                    }
                    Ok(())
                })();
                if let Err(e) = result {
                    rt.log(format!("剪贴板操作未完成，键鼠连接保持：{e:#}"));
                }
            }
            thread::sleep(Duration::from_millis(20));
        }
    })
}

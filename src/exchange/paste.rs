//! Lazy, verified disk caches for native Windows file paste. Paths never cross the network.
use super::{
    files,
    wire::{self, ClipboardVersion},
};
use crate::engine::Runtime;
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::PathBuf,
    sync::{Arc, Condvar, Mutex, Weak, atomic::Ordering},
    time::{Duration, SystemTime},
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub size: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub token: String,
    pub files: Vec<Entry>,
}
impl Manifest {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            wire::valid_id(&self.token) && !self.files.is_empty() && self.files.len() <= 64,
            "文件剪贴板列表无效"
        );
        for file in &self.files {
            files::validate_name(&file.name)?;
        }
        Ok(())
    }
}

pub struct Export {
    pub manifest: Manifest,
    pub version: ClipboardVersion,
    paths: Vec<(PathBuf, Option<SystemTime>)>,
}
impl Export {
    pub fn new(paths: Vec<PathBuf>, version: ClipboardVersion) -> Result<Self> {
        ensure!(
            !paths.is_empty() && paths.len() <= 64,
            "每批请选择 1–64 个文件"
        );
        let mut entries = Vec::new();
        let mut sources = Vec::new();
        for path in paths {
            let metadata =
                fs::metadata(&path).with_context(|| format!("无法读取 {}", path.display()))?;
            ensure!(
                metadata.is_file(),
                "跨设备粘贴暂不支持文件夹：{}",
                path.display()
            );
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .context("文件名无法解码")?
                .to_string();
            files::validate_name(&name)?;
            entries.push(Entry {
                name,
                size: metadata.len(),
            });
            sources.push((path, metadata.modified().ok()));
        }
        Ok(Self {
            manifest: Manifest {
                token: wire::new_id(),
                files: entries,
            },
            version,
            paths: sources,
        })
    }
    pub fn open(&self, token: &str, index: u32) -> Result<(fs::File, Entry)> {
        ensure!(token == self.manifest.token, "文件剪贴板已更新，请重新复制");
        let index = index as usize;
        let (path, modified) = self.paths.get(index).context("文件索引无效")?;
        let entry = self.manifest.files[index].clone();
        let file = fs::File::open(path).context("源文件不可读，请重新复制")?;
        let meta = file.metadata()?;
        ensure!(
            meta.is_file() && meta.len() == entry.size && meta.modified().ok() == *modified,
            "源文件在复制后发生变化，请重新复制"
        );
        Ok((file, entry))
    }
}

#[derive(Clone)]
pub struct RemoteFiles(Arc<RemoteInner>);
struct RemoteInner {
    rt: Weak<Runtime>,
    source: String,
    manifest: Manifest,
    version: ClipboardVersion,
    slots: Mutex<Vec<Option<Arc<Fetch>>>>,
}
impl RemoteFiles {
    pub fn new(
        rt: &Arc<Runtime>,
        source: String,
        manifest: Manifest,
        version: ClipboardVersion,
    ) -> Result<Self> {
        manifest.validate()?;
        ensure!(
            source == version.origin && wire::valid_id(&source),
            "文件剪贴板来源无效"
        );
        let slots = Mutex::new(vec![None; manifest.files.len()]);
        Ok(Self(Arc::new(RemoteInner {
            rt: Arc::downgrade(rt),
            source,
            manifest,
            version,
            slots,
        })))
    }
    pub fn entries(&self) -> &[Entry] {
        &self.0.manifest.files
    }
    pub fn fetch(&self, index: usize) -> Result<Arc<Fetch>> {
        let entry = self.entries().get(index).context("文件索引无效")?.clone();
        let mut slots = self.0.slots.lock().unwrap();
        if let Some(slot) = &slots[index]
            && !slot.failed()
        {
            return Ok(slot.clone());
        }
        let rt = self.0.rt.upgrade().context("共享程序已退出")?;
        ensure!(
            rt.sharing_enabled()
                && rt.exchange.auto_clipboard.load(Ordering::SeqCst)
                && rt.exchange.ready(),
            "文件粘贴需要连接并开启自动剪贴板共享"
        );
        ensure!(
            *rt.exchange.clipboard_version.lock().unwrap() == self.0.version,
            "剪贴板已更新，请重新复制文件"
        );
        let id = wire::new_id();
        let slot = Arc::new(Fetch {
            directory: std::env::temp_dir()
                .join("MWB-Rust-clipboard")
                .join(&rt.local.id)
                .join(&id),
            id,
            source: self.0.source.clone(),
            clipboard: self.0.manifest.token.clone(),
            index: index as u32,
            entry,
            rt: self.0.rt.clone(),
            state: Mutex::new(State::Pending),
            changed: Condvar::new(),
        });
        if let Err(e) = rt.exchange.fetch(slot.clone()) {
            slot.fail(format!("{e:#}"));
            return Err(e);
        }
        slots[index] = Some(slot.clone());
        Ok(slot)
    }
    pub fn cancel(&self) {
        for slot in self.0.slots.lock().unwrap().iter().flatten() {
            slot.cancel();
        }
    }
}
enum State {
    Pending,
    Ready(PathBuf),
    Failed(String),
}
pub struct Fetch {
    pub id: String,
    pub source: String,
    pub clipboard: String,
    pub index: u32,
    pub entry: Entry,
    pub directory: PathBuf,
    rt: Weak<Runtime>,
    state: Mutex<State>,
    changed: Condvar,
}
impl Fetch {
    pub fn failed(&self) -> bool {
        matches!(*self.state.lock().unwrap(), State::Failed(_))
    }
    pub fn fail(&self, reason: String) {
        let mut state = self.state.lock().unwrap();
        if matches!(*state, State::Pending) {
            *state = State::Failed(reason);
            self.changed.notify_all();
        }
    }
    pub fn complete(&self, path: PathBuf) -> bool {
        let mut state = self.state.lock().unwrap();
        if matches!(*state, State::Pending) {
            *state = State::Ready(path);
            self.changed.notify_all();
            true
        } else {
            let _ = fs::remove_file(path);
            false
        }
    }
    pub fn cancel(&self) {
        let mut state = self.state.lock().unwrap();
        if matches!(*state, State::Pending) {
            *state = State::Failed("粘贴已取消".into());
            self.changed.notify_all();
            if let Some(rt) = self.rt.upgrade() {
                let _ = rt.exchange.cancel(self.id.clone());
            }
        }
    }
    pub fn wait(&self, mut pump: impl FnMut()) -> Result<PathBuf> {
        loop {
            let state = self.state.lock().unwrap();
            match &*state {
                State::Ready(path) => return Ok(path.clone()),
                State::Failed(reason) => bail!("{reason}"),
                State::Pending => {}
            }
            if self
                .rt
                .upgrade()
                .is_none_or(|rt| rt.stop.load(Ordering::SeqCst))
            {
                drop(state);
                self.fail("工作组已停止".into());
                continue;
            }
            let (state, _) = self
                .changed
                .wait_timeout(state, Duration::from_millis(20))
                .unwrap();
            drop(state);
            // Native streams keep their STA pumping while the data worker receives the file.
            pump();
        }
    }
}
impl Drop for Fetch {
    fn drop(&mut self) {
        if let State::Ready(path) = &*self.state.lock().unwrap() {
            let _ = fs::remove_file(path);
        }
        // Only this generated task directory is removed, and only when empty.
        let _ = fs::remove_dir(&self.directory);
    }
}

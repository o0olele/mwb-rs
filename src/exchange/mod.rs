//! Clipboard and file exchange, isolated from the latency-sensitive input dispatcher.
pub(crate) mod clipboard;
mod files;
mod net;
pub(crate) mod paste;
#[cfg(test)]
mod tests;
#[cfg(windows)]
pub(crate) mod windows_clipboard;
mod wire;
use crate::{config::Config, engine::Runtime};
use anyhow::{Context, Result, bail, ensure};
use clipboard::{Backend, Content, Request};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, VecDeque},
    fs::File,
    io::Read,
    net::{SocketAddr, TcpListener},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU16, AtomicU64, AtomicUsize, Ordering},
        mpsc::{self, SyncSender},
    },
    thread,
    time::{Duration, Instant},
};
use wire::{ClipboardVersion, Event, Kind, Packet};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Waiting,
    Offered,
    Sending,
    Receiving,
    Verifying,
    Complete,
    Cancelled,
    Failed,
}
impl Phase {
    pub fn terminal(self) -> bool {
        matches!(self, Self::Complete | Self::Cancelled | Self::Failed)
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Waiting => "等待对方接收",
            Self::Offered => "等待接收",
            Self::Sending => "正在发送",
            Self::Receiving => "正在接收",
            Self::Verifying => "正在校验",
            Self::Complete => "已完成",
            Self::Cancelled => "已取消",
            Self::Failed => "失败",
        }
    }
}
#[derive(Clone, Debug)]
pub struct Transfer {
    pub id: String,
    pub peer: String,
    pub name: String,
    pub size: u64,
    pub transferred: u64,
    pub incoming: bool,
    pub phase: Phase,
    pub path: Option<PathBuf>,
    pub detail: String,
}
pub struct Exchange {
    pub auto_clipboard: AtomicBool,
    pub(crate) rate: Arc<AtomicU64>,
    port: AtomicU16,
    pub(crate) tickets: net::Tickets,
    pub(crate) join: net::JoinSlot,
    pub(crate) clipboard_version: Mutex<ClipboardVersion>,
    pub(crate) clipboard_clock: AtomicU64,
    exports: Mutex<Option<paste::Export>>,
    commands: Mutex<Option<SyncSender<Command>>>,
    clipboard: Mutex<Option<SyncSender<Request>>>,
    transfers: Mutex<VecDeque<Transfer>>,
    ready: AtomicBool,
    connections: AtomicUsize,
}
enum Command {
    Fetch(Arc<paste::Fetch>),
    Files {
        target: String,
        paths: Vec<PathBuf>,
    },
    Accept {
        id: String,
        directory: PathBuf,
    },
    Cancel(String),
    Clipboard {
        target: Option<String>,
        content: Content,
        version: Option<ClipboardVersion>,
    },
}
impl Exchange {
    pub fn new(config: &Config) -> Self {
        Self {
            auto_clipboard: AtomicBool::new(config.auto_clipboard),
            rate: Arc::new(AtomicU64::new(
                u64::from(config.transfer_rate_mib) * 1024 * 1024,
            )),
            port: AtomicU16::new(0),
            tickets: Arc::new(Mutex::new(Default::default())),
            join: Arc::new(Mutex::new(None)),
            clipboard_version: Mutex::new(ClipboardVersion::default()),
            clipboard_clock: AtomicU64::new(0),
            exports: Mutex::new(None),
            commands: Mutex::new(None),
            clipboard: Mutex::new(None),
            transfers: Mutex::new(VecDeque::new()),
            ready: AtomicBool::new(false),
            connections: AtomicUsize::new(0),
        }
    }
    pub fn snapshot(&self) -> Vec<Transfer> {
        self.transfers.lock().unwrap().iter().cloned().collect()
    }
    pub fn ready(&self) -> bool {
        self.ready.load(Ordering::SeqCst)
    }
    #[cfg(test)]
    pub(crate) fn connection_count(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }
    pub fn set_rate_mib(&self, rate: u32) {
        self.rate
            .store(u64::from(rate) * 1024 * 1024, Ordering::SeqCst);
    }
    fn command(&self, command: Command) -> Result<()> {
        self.commands
            .lock()
            .unwrap()
            .as_ref()
            .context("请先连接工作组")?
            .try_send(command)
            .map_err(|_| anyhow::anyhow!("文件/剪贴板任务队列忙，请稍后重试"))
    }
    pub fn send_files(&self, target: String, paths: Vec<PathBuf>) -> Result<()> {
        ensure!(self.ready(), "文件通道尚未就绪");
        ensure!(
            !paths.is_empty() && paths.len() <= 64,
            "每批请选择 1–64 个文件"
        );
        self.command(Command::Files { target, paths })
    }
    pub fn accept(&self, id: String, directory: PathBuf) -> Result<()> {
        self.command(Command::Accept { id, directory })
    }
    pub fn cancel(&self, id: String) -> Result<()> {
        self.command(Command::Cancel(id))
    }
    fn fetch(&self, fetch: Arc<paste::Fetch>) -> Result<()> {
        self.command(Command::Fetch(fetch))
    }
    pub fn send_clipboard(&self, target: String) -> Result<()> {
        self.clip_request(Request::Read { target })
    }
    pub fn send_clipboard_files(&self, target: String) -> Result<()> {
        self.clip_request(Request::Files { target })
    }
    pub(crate) fn text(&self, target: String, text: String) -> Result<()> {
        let content = Content::text(text);
        content.validate()?;
        self.command(Command::Clipboard {
            target: Some(target),
            content,
            version: None,
        })
    }
    fn clip_request(&self, request: Request) -> Result<()> {
        ensure!(self.ready(), "文件/剪贴板通道尚未就绪");
        self.clipboard
            .lock()
            .unwrap()
            .as_ref()
            .context("剪贴板线程未启动")?
            .try_send(request)
            .map_err(|_| anyhow::anyhow!("剪贴板队列忙"))
    }
    pub(crate) fn register(&self, device: String, parent: Arc<AtomicBool>) -> (u16, String) {
        let token = wire::new_id();
        self.tickets
            .lock()
            .unwrap()
            .insert(token.clone(), net::Ticket { device, parent });
        (self.port.load(Ordering::SeqCst), token)
    }
    pub(crate) fn unregister(&self, device: &str) {
        self.tickets
            .lock()
            .unwrap()
            .retain(|_, t| t.device != device);
    }
    pub(crate) fn join(
        &self,
        address: SocketAddr,
        ticket: String,
        parent: Arc<AtomicBool>,
    ) -> Result<()> {
        ensure!(
            address.port() != 0 && wire::valid_id(&ticket),
            "文件通道信息无效"
        );
        *self.join.lock().unwrap() = Some(net::Join {
            address,
            ticket,
            parent,
        });
        Ok(())
    }
    fn add(&self, transfer: Transfer) {
        let mut all = self.transfers.lock().unwrap();
        while all.len() >= 100 {
            if let Some(i) = all.iter().position(|t| t.phase.terminal()) {
                all.remove(i);
            } else {
                break;
            }
        }
        all.push_front(transfer);
    }
    fn update(&self, id: &str, update: impl FnOnce(&mut Transfer)) {
        if let Some(t) = self
            .transfers
            .lock()
            .unwrap()
            .iter_mut()
            .find(|t| t.id == id)
        {
            update(t);
        }
    }
}

pub(crate) fn launch(
    rt: Arc<Runtime>,
    key: [u8; 32],
    host: bool,
    backend: Box<dyn Backend>,
) -> Result<thread::JoinHandle<()>> {
    let listener = if host {
        let listener = TcpListener::bind(("0.0.0.0", 0)).context("无法创建文件通道")?;
        listener.set_nonblocking(true)?;
        rt.exchange
            .port
            .store(listener.local_addr()?.port(), Ordering::SeqCst);
        Some(listener)
    } else {
        None
    };
    let (commands, receive) = mpsc::sync_channel(64);
    let (clip_tx, clip_rx) = mpsc::sync_channel(8);
    let (events, incoming) = mpsc::sync_channel(32);
    *rt.exchange.commands.lock().unwrap() = Some(commands.clone());
    *rt.exchange.clipboard.lock().unwrap() = Some(clip_tx.clone());
    let clipboard = clipboard::launch(rt.clone(), backend, clip_rx, commands);
    let network = net::launch(rt.clone(), key, listener, events);
    Ok(thread::spawn(move || {
        let mut worker = Worker {
            rt: rt.clone(),
            host,
            links: BTreeMap::new(),
            outgoing: BTreeMap::new(),
            incoming: BTreeMap::new(),
            fetches: BTreeMap::new(),
            routes: VecDeque::new(),
            clipboard: clip_tx,
            pump_cursor: 0,
        };
        while !rt.stop.load(Ordering::SeqCst) {
            for command in receive.try_iter().take(8) {
                if let Err(e) = worker.command(command) {
                    rt.log(format!("文件/剪贴板操作未完成：{e:#}"));
                }
            }
            worker.flush_routes();
            // Back pressure is confined to this data worker and its bounded sockets.
            for _ in 0..32 {
                if worker.routes.len() >= 64 {
                    break;
                }
                let Ok(event) = incoming.try_recv() else {
                    break;
                };
                worker.network(event);
            }
            worker.pump();
            let closed: Vec<_> = worker
                .links
                .iter()
                .filter(|(_, (_, l))| l.closed.load(Ordering::SeqCst))
                .map(|(id, _)| id.clone())
                .collect();
            for id in closed {
                worker.disconnected(&id);
            }
            thread::sleep(Duration::from_millis(2));
        }
        for (_, link) in worker.links.values() {
            link.closed.store(true, Ordering::SeqCst);
        }
        for id in worker
            .outgoing
            .keys()
            .chain(worker.incoming.keys())
            .chain(worker.fetches.keys())
        {
            rt.exchange.update(id, |t| {
                t.phase = Phase::Cancelled;
                t.detail = "工作组已停止".into();
            });
        }
        for (fetch, _) in worker.fetches.values() {
            fetch.fail("工作组已停止".into());
        }
        *rt.exchange.exports.lock().unwrap() = None;
        drop(worker); // closes source files and removes unfinished receiving files
        *rt.exchange.commands.lock().unwrap() = None;
        *rt.exchange.clipboard.lock().unwrap() = None;
        rt.exchange.ready.store(false, Ordering::SeqCst);
        rt.exchange.connections.store(0, Ordering::SeqCst);
        rt.exchange.tickets.lock().unwrap().clear();
        *rt.exchange.join.lock().unwrap() = None;
        let _ = network.join();
        let _ = clipboard.join();
    }))
}

enum Source {
    File(File),
    Memory { bytes: Arc<Vec<u8>>, offset: usize },
}
impl Source {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::File(file) => file.read(buffer),
            Self::Memory { bytes, offset } => {
                let n = buffer.len().min(bytes.len() - *offset);
                buffer[..n].copy_from_slice(&bytes[*offset..*offset + n]);
                *offset += n;
                Ok(n)
            }
        }
    }
}
struct Outgoing {
    peer: String,
    kind: Kind,
    size: u64,
    source: Source,
    offset: u64,
    hash: Sha256,
    accepted: bool,
    finished: bool,
    pending: Option<Packet>,
    last: Instant,
}
enum Sink {
    File(Box<files::IncomingFile>),
    Memory(Vec<u8>),
}
struct Incoming {
    peer: String,
    kind: Kind,
    size: u64,
    version: Option<ClipboardVersion>,
    sink: Option<Sink>,
    received: u64,
    hash: Sha256,
    last: Instant,
}
struct Worker {
    rt: Arc<Runtime>,
    host: bool,
    links: BTreeMap<String, (u64, net::Link)>,
    outgoing: BTreeMap<String, Outgoing>,
    incoming: BTreeMap<String, Incoming>,
    fetches: BTreeMap<String, (Arc<paste::Fetch>, Instant)>,
    routes: VecDeque<Packet>,
    clipboard: SyncSender<Request>,
    pump_cursor: usize,
}
impl Worker {
    fn packet(&self, target: &str, event: Event) -> Packet {
        Packet::new(&self.rt.local.id, target, event)
    }
    fn send(&self, packet: &Packet) -> bool {
        let key = if self.host {
            packet.header.target.as_str()
        } else {
            ""
        };
        self.links
            .get(key)
            .is_some_and(|(_, link)| link.send(packet))
    }
    fn queue(&mut self, packet: Packet) {
        // Keep each destination FIFO even when a slow receiver fills its socket queue.
        if (self
            .routes
            .iter()
            .any(|p| p.header.target == packet.header.target)
            || !self.send(&packet))
            && self.routes.len() < 128
        {
            self.routes.push_back(packet);
        }
    }
    fn flush_routes(&mut self) {
        let mut blocked = std::collections::HashSet::new();
        for _ in 0..self.routes.len() {
            let Some(packet) = self.routes.pop_front() else {
                break;
            };
            let key = if self.host {
                packet.header.target.as_str()
            } else {
                ""
            };
            if self.links.contains_key(key) && (blocked.contains(key) || !self.send(&packet)) {
                blocked.insert(key.to_string());
                self.routes.push_back(packet);
            }
        }
    }
    fn connected(&self, target: &str) -> bool {
        target != self.rt.local.id
            && self
                .rt
                .snapshot()
                .layout
                .get(target)
                .is_some_and(|d| d.online)
            && self.links.contains_key(if self.host { target } else { "" })
    }
    fn offer(
        &mut self,
        peer: String,
        kind: Kind,
        size: u64,
        source: Source,
        version: Option<ClipboardVersion>,
    ) -> Result<()> {
        self.offer_as(wire::new_id(), peer, kind, size, source, version)
    }
    fn offer_as(
        &mut self,
        id: String,
        peer: String,
        kind: Kind,
        size: u64,
        source: Source,
        version: Option<ClipboardVersion>,
    ) -> Result<()> {
        ensure!(self.connected(&peer), "目标设备离线或文件通道尚未就绪");
        ensure!(self.outgoing.len() < 64, "正在等待或发送的任务过多");
        ensure!(
            !self.outgoing.contains_key(&id)
                && !self.incoming.contains_key(&id)
                && !self.fetches.contains_key(&id),
            "重复的文件请求"
        );
        kind.validate(size)?;
        if let Kind::File { name } = &kind {
            self.rt.exchange.add(Transfer {
                id: id.clone(),
                peer: peer.clone(),
                name: name.clone(),
                size,
                transferred: 0,
                incoming: false,
                phase: Phase::Waiting,
                path: None,
                detail: String::new(),
            });
        }
        let packet = self.packet(
            &peer,
            Event::Offer {
                id: id.clone(),
                kind: kind.clone(),
                size,
                version,
            },
        );
        self.outgoing.insert(
            id,
            Outgoing {
                peer,
                kind,
                size,
                source,
                offset: 0,
                hash: Sha256::new(),
                accepted: false,
                finished: false,
                pending: Some(packet),
                last: Instant::now(),
            },
        );
        Ok(())
    }
    fn command(&mut self, command: Command) -> Result<()> {
        match command {
            Command::Fetch(fetch) => {
                let result = (|| -> Result<()> {
                    ensure!(
                        self.rt.sharing_enabled()
                            && self.rt.exchange.auto_clipboard.load(Ordering::SeqCst),
                        "剪贴板共享已关闭"
                    );
                    ensure!(self.connected(&fetch.source), "文件来源设备离线");
                    ensure!(
                        self.fetches.len() < 64 && !fetch.failed(),
                        "粘贴任务已取消或任务过多"
                    );
                    self.rt.exchange.add(Transfer {
                        id: fetch.id.clone(),
                        peer: fetch.source.clone(),
                        name: fetch.entry.name.clone(),
                        size: fetch.entry.size,
                        transferred: 0,
                        incoming: true,
                        phase: Phase::Receiving,
                        path: None,
                        detail: "正在为 Windows 粘贴准备文件".into(),
                    });
                    self.queue(self.packet(
                        &fetch.source,
                        Event::Pull {
                            id: fetch.id.clone(),
                            clipboard: fetch.clipboard.clone(),
                            index: fetch.index,
                        },
                    ));
                    self.fetches
                        .insert(fetch.id.clone(), (fetch.clone(), Instant::now()));
                    Ok(())
                })();
                if let Err(e) = &result {
                    fetch.fail(format!("{e:#}"));
                }
                result?;
            }
            Command::Files { target, paths } => {
                ensure!(self.connected(&target), "目标设备的文件通道尚未就绪");
                ensure!(
                    !paths.is_empty()
                        && paths.len() <= 64
                        && self.outgoing.len() + paths.len() <= 64,
                    "每批最多 64 个文件；请等待现有任务完成"
                );
                // Validate and open all selected sources before offering any of this batch.
                let mut files = Vec::new();
                for path in paths {
                    let name = path
                        .file_name()
                        .context("文件名缺失")?
                        .to_str()
                        .context("文件名无法解码")?
                        .to_string();
                    files::validate_name(&name)?;
                    let file = File::open(&path)
                        .with_context(|| format!("无法读取 {}", path.display()))?;
                    let metadata = file.metadata()?;
                    ensure!(
                        metadata.is_file(),
                        "请选择文件，暂不支持文件夹：{}",
                        path.display()
                    );
                    files.push((name, file, metadata.len()));
                }
                for (name, file, size) in files {
                    self.offer(
                        target.clone(),
                        Kind::File { name },
                        size,
                        Source::File(file),
                        None,
                    )?;
                }
            }
            Command::Accept { id, directory } => {
                let transfer = self.incoming.get_mut(&id).context("该传输已结束")?;
                ensure!(transfer.sink.is_none(), "已经接收此文件");
                let Kind::File { name } = &transfer.kind else {
                    bail!("任务不是文件")
                };
                let result = files::IncomingFile::create(&directory, name, &id, transfer.size);
                match result {
                    Ok(file) => {
                        transfer.sink = Some(Sink::File(Box::new(file)));
                        transfer.last = Instant::now();
                    }
                    Err(e) => {
                        self.fail(&id, format!("{e:#}"), Phase::Failed);
                        return Ok(());
                    }
                }
                let peer = transfer.peer.clone();
                self.rt.exchange.update(&id, |t| t.phase = Phase::Receiving);
                self.queue(self.packet(&peer, Event::Accept { id }));
            }
            Command::Cancel(id) => self.fail(&id, "用户取消传输".into(), Phase::Cancelled),
            Command::Clipboard {
                target,
                content,
                version,
            } => {
                ensure!(self.rt.sharing_enabled(), "请先开启共享");
                content.validate()?;
                if version.is_some() && !self.rt.exchange.auto_clipboard.load(Ordering::SeqCst) {
                    return Ok(());
                }
                let targets = target.map(|t| vec![t]).unwrap_or_else(|| {
                    self.rt
                        .snapshot()
                        .layout
                        .devices
                        .iter()
                        .filter(|d| d.online && d.info.id != self.rt.local.id)
                        .map(|d| d.info.id.clone())
                        .collect()
                });
                for target in targets {
                    // Supersede queued clipboard snapshots; file transfers keep their position.
                    let old: Vec<_> = self
                        .outgoing
                        .iter()
                        .filter(|(_, t)| t.peer == target && !t.kind.is_file())
                        .map(|(id, _)| id.clone())
                        .collect();
                    for id in old {
                        self.fail(&id, "剪贴板已更新".into(), Phase::Cancelled);
                    }
                    self.offer(
                        target,
                        content.kind.clone(),
                        content.data.len() as u64,
                        Source::Memory {
                            bytes: content.data.clone(),
                            offset: 0,
                        },
                        version.clone(),
                    )?;
                }
            }
        }
        Ok(())
    }
    fn network(&mut self, event: net::Event) {
        match event {
            net::Event::Connected {
                device,
                serial,
                link,
            } => {
                if let Some((_, old)) = self.links.insert(device.clone(), (serial, link)) {
                    old.closed.store(true, Ordering::SeqCst);
                }
                if self.host {
                    self.queue(self.packet(
                        &device,
                        Event::Clock {
                            clock: self.rt.exchange.clipboard_clock.load(Ordering::SeqCst),
                        },
                    ));
                    self.rt.exchange.ready.store(true, Ordering::SeqCst);
                }
                self.rt
                    .exchange
                    .connections
                    .store(self.links.len(), Ordering::SeqCst);
            }
            net::Event::Disconnected { device, serial } => {
                if self.links.get(&device).is_some_and(|(id, _)| *id == serial) {
                    self.disconnected(&device);
                }
            }
            net::Event::Packet {
                device,
                serial,
                mut packet,
            } => {
                if self.links.get(&device).is_none_or(|(id, _)| *id != serial) {
                    return;
                }
                if self.host {
                    if matches!(packet.header.event, Event::Clock { .. }) {
                        return;
                    }
                    packet.header.source = device;
                }
                if let Event::Offer {
                    version: Some(version),
                    ..
                } = &packet.header.event
                    && version.origin == packet.header.source
                    && version.clock < u64::MAX - 1
                {
                    self.rt
                        .exchange
                        .clipboard_clock
                        .fetch_max(version.clock, Ordering::SeqCst);
                }
                if packet.header.target != self.rt.local.id {
                    if self.host {
                        if self.connected(&packet.header.target) {
                            self.queue(packet);
                        } else if let Some(id) = packet.header.event.id() {
                            self.queue(Packet::new(
                                &packet.header.target,
                                &packet.header.source,
                                Event::Cancel {
                                    id: id.into(),
                                    reason: "目标设备文件通道离线".into(),
                                },
                            ));
                        }
                    }
                    return;
                }
                let peer = packet.header.source.clone();
                let id = packet.header.event.id().map(str::to_string);
                if let Err(e) = self.receive(packet) {
                    let detail: String = format!("{e:#}").chars().take(512).collect();
                    if let Some(id) = id {
                        // Only the owner of a transfer can mutate or cancel it.
                        let owned = self.outgoing.get(&id).is_some_and(|t| t.peer == peer)
                            || self.incoming.get(&id).is_some_and(|t| t.peer == peer)
                            || self.fetches.get(&id).is_some_and(|(f, _)| f.source == peer);
                        if owned {
                            self.fail(&id, detail.clone(), Phase::Failed);
                        } else {
                            self.queue(self.packet(
                                &peer,
                                Event::Cancel {
                                    id,
                                    reason: detail.clone(),
                                },
                            ));
                        }
                    }
                    self.rt
                        .log(format!("数据传输未完成，键鼠连接保持：{detail}"));
                }
            }
        }
    }
    fn disconnected(&mut self, peer: &str) {
        self.links.remove(peer);
        self.rt
            .exchange
            .connections
            .store(self.links.len(), Ordering::SeqCst);
        let ids: Vec<_> = self
            .outgoing
            .iter()
            .filter(|(_, t)| !self.host || t.peer == peer)
            .map(|(id, _)| id.clone())
            .chain(
                self.incoming
                    .iter()
                    .filter(|(_, t)| !self.host || t.peer == peer)
                    .map(|(id, _)| id.clone()),
            )
            .chain(
                self.fetches
                    .iter()
                    .filter(|(_, (f, _))| !self.host || f.source == peer)
                    .map(|(id, _)| id.clone()),
            )
            .collect();
        for id in ids {
            self.fail(&id, "文件通道中断，请重新发送".into(), Phase::Failed);
        }
        self.rt
            .exchange
            .ready
            .store(!self.links.is_empty(), Ordering::SeqCst);
    }
    fn fail(&mut self, id: &str, reason: String, phase: Phase) {
        let mut peer = self
            .outgoing
            .remove(id)
            .map(|t| t.peer)
            .or_else(|| self.incoming.remove(id).map(|t| t.peer));
        if let Some((fetch, _)) = self.fetches.remove(id) {
            fetch.fail(reason.clone());
            peer = Some(fetch.source.clone());
        }
        self.routes.retain(|p| p.header.event.id() != Some(id));
        if let Some(peer) = peer {
            self.queue(self.packet(
                &peer,
                Event::Cancel {
                    id: id.into(),
                    reason: reason.chars().take(512).collect(),
                },
            ));
        }
        self.rt.exchange.update(id, |t| {
            t.phase = phase;
            t.detail = reason;
        });
    }
    fn receive(&mut self, packet: Packet) -> Result<()> {
        let peer = packet.header.source;
        ensure!(wire::valid_id(&peer), "发送设备无效");
        match packet.header.event {
            Event::Pull {
                id,
                clipboard,
                index,
            } => {
                ensure!(
                    self.rt.sharing_enabled()
                        && self.rt.exchange.auto_clipboard.load(Ordering::SeqCst),
                    "源设备已关闭剪贴板共享"
                );
                let (file, entry) = {
                    let exports = self.rt.exchange.exports.lock().unwrap();
                    let export = exports.as_ref().context("文件剪贴板已失效，请重新复制")?;
                    ensure!(
                        *self.rt.exchange.clipboard_version.lock().unwrap() == export.version,
                        "剪贴板已更新，请重新复制文件"
                    );
                    export.open(&clipboard, index)?
                };
                self.offer_as(
                    id,
                    peer,
                    Kind::File { name: entry.name },
                    entry.size,
                    Source::File(file),
                    None,
                )?;
            }
            Event::Clock { clock } => {
                ensure!(!self.host && clock < u64::MAX - 1, "剪贴板时钟无效");
                self.rt
                    .exchange
                    .clipboard_clock
                    .fetch_max(clock, Ordering::SeqCst);
                self.rt.exchange.ready.store(true, Ordering::SeqCst);
            }
            Event::Offer {
                id,
                kind,
                size,
                version,
            } => {
                ensure!(
                    !self.incoming.contains_key(&id)
                        && !self.outgoing.contains_key(&id)
                        && self.incoming.len() < 64,
                    "重复传输或待接收任务过多"
                );
                kind.validate(size)?;
                if let Some((fetch, _)) = self.fetches.get(&id) {
                    ensure!(
                        fetch.source == peer
                            && kind
                                == (Kind::File {
                                    name: fetch.entry.name.clone()
                                })
                            && size == fetch.entry.size
                            && !fetch.failed(),
                        "文件粘贴响应与请求不符"
                    );
                }
                let sink = if let Kind::File { name } = &kind {
                    ensure!(version.is_none(), "文件不支持自动剪贴板版本");
                    if let Some((fetch, _)) = self.fetches.get(&id) {
                        Some(Sink::File(Box::new(files::IncomingFile::create(
                            &fetch.directory,
                            "content.bin",
                            &id,
                            size,
                        )?)))
                    } else {
                        self.rt.exchange.add(Transfer {
                            id: id.clone(),
                            peer: peer.clone(),
                            name: name.clone(),
                            size,
                            transferred: 0,
                            incoming: true,
                            phase: Phase::Offered,
                            path: None,
                            detail: String::new(),
                        });
                        None
                    }
                } else {
                    ensure!(self.rt.sharing_enabled(), "共享已暂停");
                    ensure!(
                        !matches!(kind, Kind::FileList | Kind::ClearFiles) || version.is_some(),
                        "文件剪贴板缺少版本"
                    );
                    if let Some(version) = &version {
                        ensure!(
                            self.rt.exchange.auto_clipboard.load(Ordering::SeqCst)
                                && version.origin == peer
                                && version.clock > 0,
                            "自动剪贴板已关闭或版本无效"
                        );
                        {
                            let mut latest = self.rt.exchange.clipboard_version.lock().unwrap();
                            ensure!(version > &*latest, "旧剪贴板内容已忽略");
                            *latest = version.clone();
                        }
                        let old: Vec<_> = self
                            .incoming
                            .iter()
                            .filter(|(_, t)| !t.kind.is_file())
                            .map(|(id, _)| id.clone())
                            .collect();
                        for id in old {
                            self.fail(&id, "剪贴板已更新".into(), Phase::Cancelled);
                        }
                    }
                    let reserved: u64 = self
                        .incoming
                        .values()
                        .filter(|t| !t.kind.is_file())
                        .map(|t| t.size)
                        .sum();
                    ensure!(reserved + size <= 256 * 1024 * 1024, "剪贴板接收缓冲区忙");
                    Some(Sink::Memory(Vec::new()))
                };
                let accepted = sink.is_some();
                self.incoming.insert(
                    id.clone(),
                    Incoming {
                        peer: peer.clone(),
                        kind,
                        size,
                        version,
                        sink,
                        received: 0,
                        hash: Sha256::new(),
                        last: Instant::now(),
                    },
                );
                if accepted {
                    self.queue(self.packet(&peer, Event::Accept { id }));
                }
            }
            Event::Accept { id } => {
                let t = self.outgoing.get_mut(&id).context("传输已结束")?;
                ensure!(t.peer == peer && !t.accepted, "文件接收确认无效");
                t.accepted = true;
                t.last = Instant::now();
                self.rt.exchange.update(&id, |t| t.phase = Phase::Sending);
            }
            Event::Chunk { id, offset } => {
                let t = self.incoming.get_mut(&id).context("传输已结束")?;
                ensure!(
                    t.peer == peer
                        && offset == t.received
                        && !packet.data.is_empty()
                        && offset
                            .checked_add(packet.data.len() as u64)
                            .is_some_and(|n| n <= t.size),
                    "文件块来源、顺序或长度错误"
                );
                match t.sink.as_mut().context("接收端尚未同意传输")? {
                    Sink::File(file) => file.write(offset, &packet.data)?,
                    Sink::Memory(bytes) => bytes.extend_from_slice(&packet.data),
                }
                t.received += packet.data.len() as u64;
                t.hash.update(&packet.data);
                t.last = Instant::now();
                self.rt.exchange.update(&id, |v| v.transferred = t.received);
            }
            Event::Finish { id, digest } => {
                let t = self.incoming.get(&id).context("传输已结束")?;
                ensure!(
                    t.peer == peer && t.received == t.size && t.sink.is_some(),
                    "传输尚未完成"
                );
                ensure!(
                    hex::encode(t.hash.clone().finalize()) == digest,
                    "数据 SHA-256 校验失败"
                );
                let t = self.incoming.remove(&id).unwrap();
                self.rt.exchange.update(&id, |v| v.phase = Phase::Verifying);
                let result = match t.sink.unwrap() {
                    Sink::File(file) => file.finish(&digest).and_then(|path| {
                        let pasted = if let Some((fetch, _)) = self.fetches.remove(&id) {
                            ensure!(fetch.complete(path.clone()), "粘贴已取消");
                            true
                        } else {
                            false
                        };
                        self.rt.exchange.update(&id, |v| {
                            v.phase = Phase::Complete;
                            if pasted {
                                v.detail = "已校验，交给 Windows 完成粘贴".into();
                            } else {
                                v.path = Some(path);
                            }
                        });
                        Ok(())
                    }),
                    Sink::Memory(bytes) => {
                        let content = Content {
                            kind: t.kind,
                            data: Arc::new(bytes),
                        };
                        content.validate().and_then(|()| {
                            self.clipboard
                                .try_send(Request::Apply {
                                    version: t.version,
                                    content,
                                })
                                .map_err(|_| anyhow::anyhow!("剪贴板写入队列忙"))
                        })
                    }
                };
                if let Err(e) = result {
                    let detail = format!("{e:#}");
                    if let Some((fetch, _)) = self.fetches.remove(&id) {
                        fetch.fail(detail.clone());
                    }
                    self.rt.exchange.update(&id, |v| {
                        v.phase = Phase::Failed;
                        v.detail = detail.clone();
                    });
                    self.queue(self.packet(&peer, Event::Cancel { id, reason: detail }));
                } else {
                    self.queue(self.packet(&peer, Event::Done { id }));
                }
            }
            Event::Done { id } => {
                if let Some(t) = self.outgoing.get(&id) {
                    ensure!(t.peer == peer && t.finished, "完成确认无效");
                    self.outgoing.remove(&id);
                    self.rt.exchange.update(&id, |v| {
                        v.phase = Phase::Complete;
                        v.transferred = v.size;
                    });
                }
            }
            Event::Cancel { id, reason } => {
                ensure!(reason.len() <= 4096, "取消原因过长");
                let fetching = self.fetches.get(&id).is_some_and(|(f, _)| f.source == peer);
                if self.incoming.get(&id).is_some_and(|t| t.peer == peer) {
                    self.incoming.remove(&id);
                } else if self.outgoing.get(&id).is_some_and(|t| t.peer == peer) {
                    self.outgoing.remove(&id);
                } else if !fetching {
                    return Ok(());
                }
                if let Some((fetch, _)) = self.fetches.remove(&id) {
                    fetch.fail(reason.clone());
                }
                self.routes
                    .retain(|p| p.header.event.id() != Some(id.as_str()));
                self.rt.exchange.update(&id, |v| {
                    v.phase = Phase::Cancelled;
                    v.detail = reason;
                });
            }
            _ => bail!("不允许的数据通道消息"),
        }
        Ok(())
    }
    fn pump(&mut self) {
        // A relayed source can go offline while our connection to the hub stays up.
        // Cancel immediately on topology updates instead of leaving Explorer waiting for a timeout.
        if !self.fetches.is_empty() {
            let layout = self.rt.snapshot().layout;
            let offline: Vec<_> = self
                .fetches
                .iter()
                .filter(|(_, (f, _))| layout.get(&f.source).is_none_or(|d| !d.online))
                .map(|(id, _)| id.clone())
                .collect();
            for id in offline {
                self.fail(
                    &id,
                    "文件来源设备已离线，请重连后重新粘贴".into(),
                    Phase::Failed,
                );
            }
        }
        let expired: Vec<_> = self
            .fetches
            .iter()
            .filter(|(id, (f, started))| {
                f.failed()
                    || (!self.incoming.contains_key(*id)
                        && started.elapsed() > Duration::from_secs(60))
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in expired {
            self.fail(&id, "粘贴已取消或请求超时".into(), Phase::Failed);
        }
        let mut ids: Vec<_> = self.outgoing.keys().cloned().collect();
        if !ids.is_empty() {
            let start = self.pump_cursor % ids.len();
            ids.rotate_left(start);
            self.pump_cursor = self.pump_cursor.wrapping_add(1);
        }
        for id in ids {
            let result = (|| -> Result<()> {
                let t = self.outgoing.get_mut(&id).unwrap();
                ensure!(
                    t.last.elapsed() < Duration::from_secs(if t.accepted { 60 } else { 600 }),
                    "传输等待超时"
                );
                if t.pending.is_none() && t.accepted && !t.finished {
                    if t.offset < t.size {
                        let mut bytes =
                            vec![0; (t.size - t.offset).min(wire::CHUNK as u64) as usize];
                        let n = t.source.read(&mut bytes)?;
                        ensure!(n > 0, "源文件在传输中被缩短，请重新发送");
                        bytes.truncate(n);
                        t.hash.update(&bytes);
                        let mut packet = Packet::new(
                            &self.rt.local.id,
                            &t.peer,
                            Event::Chunk {
                                id: id.clone(),
                                offset: t.offset,
                            },
                        );
                        packet.data = bytes;
                        t.offset += n as u64;
                        t.pending = Some(packet);
                    } else {
                        if let Source::File(file) = &t.source {
                            ensure!(
                                file.metadata()?.len() == t.size,
                                "源文件大小在传输中变化，请重新发送"
                            );
                        }
                        t.pending = Some(Packet::new(
                            &self.rt.local.id,
                            &t.peer,
                            Event::Finish {
                                id: id.clone(),
                                digest: hex::encode(t.hash.clone().finalize()),
                            },
                        ));
                    }
                }
                let pending = t.pending.clone();
                if let Some(packet) = pending
                    && self.send(&packet)
                {
                    let t = self.outgoing.get_mut(&id).unwrap();
                    if matches!(packet.header.event, Event::Finish { .. }) {
                        t.finished = true;
                    }
                    t.pending = None;
                    t.last = Instant::now();
                    self.rt.exchange.update(&id, |v| v.transferred = t.offset);
                }
                Ok(())
            })();
            if let Err(e) = result {
                self.fail(&id, format!("{e:#}"), Phase::Failed);
            }
        }
        let expired: Vec<_> = self
            .incoming
            .iter()
            .filter(|(_, t)| {
                t.last.elapsed() > Duration::from_secs(if t.sink.is_some() { 60 } else { 600 })
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in expired {
            self.fail(&id, "传输等待超时".into(), Phase::Failed);
        }
    }
}

pub fn format_size(bytes: u64) -> String {
    for (unit, size) in [
        ("TiB", 1u64 << 40),
        ("GiB", 1 << 30),
        ("MiB", 1 << 20),
        ("KiB", 1 << 10),
    ] {
        if bytes >= size {
            return format!("{:.1} {unit}", bytes as f64 / size as f64);
        }
    }
    format!("{bytes} B")
}

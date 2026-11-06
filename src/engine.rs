use crate::{
    config::Config,
    layout::{DeviceInfo, Layout, Placement, Point},
    motion::{RemoteMotion, SWITCH_DELAY},
    network::{self, Event, Link},
    platform::{self, InputInjectionError},
    protocol::{Control, InputEvent, MAX_TEXT_BYTES, Message, MouseMode},
};
use anyhow::{Result, bail, ensure};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, SyncSender},
    },
    thread,
    time::{Duration, Instant},
};
#[derive(Clone, Default)]
pub struct Status {
    pub listening: bool,
    pub connected: bool,
    pub reconnecting: bool,
    pub peer_name: String,
    pub address: String,
    pub detail: String,
    pub events: VecDeque<String>,
    pub layout: Layout,
    pub revision: u64,
    pub host: bool,
    pub control: Option<Control>,
}
enum Command {
    #[cfg(test)]
    DropLinks,
    Take {
        target: String,
        point: Point,
    },
    Motion(u64, i32, i32),
    Input(u64, InputEvent),
    Arrange(u64, Vec<Placement>),
    Clipboard(String, String),
}
pub struct Runtime {
    pub status: Mutex<Status>,
    pub local: DeviceInfo,
    pub running: AtomicBool,
    pub stop: AtomicBool,
    pub enabled: AtomicBool,
    paused: AtomicBool,
    pub remote: AtomicBool,
    pub incoming: AtomicBool,
    pub keyboard: AtomicBool,
    pub edge: AtomicBool,
    pub relative_mouse: AtomicBool,
    pub auto_reconnect: AtomicBool,
    pub block_corners: AtomicBool,
    pub sent: AtomicU64,
    pub received: AtomicU64,
    pub(crate) ever_connected: AtomicBool,
    capture_epoch: AtomicU64,
    emergency: AtomicBool,
    switch_after: AtomicU64,
    commands: Mutex<Option<SyncSender<Command>>>,
}
impl Runtime {
    pub fn new(config: &Config) -> Arc<Self> {
        Self::with_info(
            config,
            platform::device_info(&config.device_id, &config.name),
        )
    }
    fn with_info(config: &Config, local: DeviceInfo) -> Arc<Self> {
        let mut layout = config.layout.clone();
        for d in &mut layout.devices {
            d.online = false;
        }
        if layout.upsert(local.clone(), true).is_err() {
            layout = Layout::default();
            layout
                .upsert(local.clone(), true)
                .expect("valid local screen");
        }
        Arc::new(Self {
            status: Mutex::new(Status {
                detail: "尚未启动工作组".into(),
                layout,
                ..Status::default()
            }),
            local,
            running: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            enabled: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            remote: AtomicBool::new(false),
            incoming: AtomicBool::new(false),
            keyboard: AtomicBool::new(config.keyboard),
            edge: AtomicBool::new(config.edge_switch),
            relative_mouse: AtomicBool::new(config.relative_mouse),
            auto_reconnect: AtomicBool::new(config.auto_reconnect),
            block_corners: AtomicBool::new(config.block_corners),
            sent: AtomicU64::new(0),
            received: AtomicU64::new(0),
            ever_connected: AtomicBool::new(false),
            capture_epoch: AtomicU64::new(0),
            emergency: AtomicBool::new(false),
            switch_after: AtomicU64::new(0),
            commands: Mutex::new(None),
        })
    }
    pub fn snapshot(&self) -> Status {
        self.status.lock().unwrap().clone()
    }
    pub fn log(&self, text: impl Into<String>) {
        let text = text.into();
        let mut s = self.status.lock().unwrap();
        s.detail = text.clone();
        s.events.push_front(text);
        s.events.truncate(20);
    }
    fn enqueue(&self, cmd: Command) -> bool {
        let Ok(sender) = self.commands.try_lock() else {
            self.fail_open();
            return false;
        };
        let Some(sender) = sender.as_ref() else {
            return false;
        };
        if sender.try_send(cmd).is_ok() {
            true
        } else {
            self.fail_open();
            false
        }
    }
    fn fail_open(&self) {
        self.paused.store(true, Ordering::SeqCst);
        self.enabled.store(false, Ordering::SeqCst);
        self.return_local();
    }
    pub fn sharing_enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
            && !self.paused.load(Ordering::SeqCst)
            && !self.stop.load(Ordering::SeqCst)
    }
    pub fn return_local(&self) {
        self.remote.store(false, Ordering::SeqCst);
        self.incoming.store(false, Ordering::SeqCst);
        self.emergency.store(true, Ordering::SeqCst);
        self.block_switch();
    }
    pub fn reclaim(&self) {
        self.paused.store(true, Ordering::SeqCst);
        self.enabled.store(false, Ordering::SeqCst);
        self.return_local();
    }
    pub fn resume(&self) -> Result<()> {
        ensure!(
            self.snapshot().connected && !self.stop.load(Ordering::SeqCst),
            "请先连接工作组"
        );
        self.paused.store(false, Ordering::SeqCst);
        self.enabled.store(true, Ordering::SeqCst);
        Ok(())
    }
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        self.reclaim();
    }
    pub(crate) fn can_switch(&self) -> bool {
        tick() >= self.switch_after.load(Ordering::SeqCst)
    }
    fn block_switch(&self) {
        self.switch_after
            .store(tick() + SWITCH_DELAY.as_millis() as u64, Ordering::SeqCst);
    }
    pub fn control(&self, id: &str) -> Result<()> {
        let state = self.snapshot();
        let d = state
            .layout
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("设备不存在"))?;
        ensure!(d.online && id != self.local.id, "请选择在线的其他设备");
        ensure!(
            self.request_control(id.to_string(), d.info.primary().center()),
            "共享未开启或正在切换"
        );
        Ok(())
    }
    fn request_control(&self, target: String, point: Point) -> bool {
        if !self.sharing_enabled() || self.incoming.load(Ordering::SeqCst) || !self.can_switch() {
            return false;
        }
        if self.enqueue(Command::Take { target, point }) {
            self.remote.store(true, Ordering::SeqCst);
            self.block_switch();
            true
        } else {
            false
        }
    }
    pub(crate) fn edge_at(&self, p: Point, delta: Point) -> bool {
        if !self.sharing_enabled() || !self.edge.load(Ordering::SeqCst) || !self.can_switch() {
            return false;
        }
        let next = self.status.try_lock().ok().and_then(|s| {
            s.layout.crossing(
                &self.local.id,
                p,
                delta,
                self.block_corners.load(Ordering::SeqCst),
            )
        });
        next.is_some_and(|(id, p)| self.request_control(id, p))
    }
    pub(crate) fn capture(&self, dx: i32, dy: i32) -> bool {
        self.enqueue(Command::Motion(
            self.capture_epoch.load(Ordering::SeqCst),
            dx,
            dy,
        ))
    }
    pub(crate) fn send_input(&self, event: InputEvent) -> bool {
        self.enqueue(Command::Input(
            self.capture_epoch.load(Ordering::SeqCst),
            event,
        ))
    }
    pub fn clipboard(&self, target: String, text: String) -> Result<()> {
        ensure!(self.sharing_enabled(), "请先连接并开启共享");
        ensure!(text.len() <= MAX_TEXT_BYTES, "剪贴板文本超过 32 KB");
        ensure!(
            serde_json::to_vec(&Message::SendClipboard {
                target: target.clone(),
                text: text.clone()
            })?
            .len()
                + 16
                <= crate::transport::MAX_FRAME,
            "文本转义后超过单帧限制"
        );
        ensure!(
            self.enqueue(Command::Clipboard(target, text)),
            "发送队列不可用"
        );
        Ok(())
    }
    pub fn arrange(&self, revision: u64, positions: Vec<Placement>) -> Result<()> {
        let state = self.snapshot();
        state.layout.positioned(&positions)?;
        if self.running.load(Ordering::SeqCst) {
            ensure!(state.host || state.connected, "请等待工作组连接");
            ensure!(
                self.enqueue(Command::Arrange(revision, positions)),
                "布局队列不可用"
            );
        } else {
            let mut s = self.status.lock().unwrap();
            ensure!(s.revision == revision, "布局已变化，请重试");
            s.layout = s.layout.positioned(&positions)?;
            s.revision += 1;
        }
        Ok(())
    }
}
fn tick() -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_millis() as u64
}
pub(crate) trait SessionInput: Send {
    fn enable(&mut self, rt: Arc<Runtime>) -> Result<()>;
    fn enter(&mut self, point: Point, mode: MouseMode) -> Result<()>;
    fn apply(&mut self, event: InputEvent, keyboard: bool) -> Result<()>;
    fn position(&mut self) -> Result<Point>;
    fn release_all(&mut self);
    fn park(&mut self) -> Result<()>;
    fn clipboard(&mut self, text: String) -> Result<()> {
        arboard::Clipboard::new()?.set_text(text)?;
        Ok(())
    }
}
impl SessionInput for platform::Injector {
    fn enable(&mut self, rt: Arc<Runtime>) -> Result<()> {
        platform::install_hooks(rt)
    }
    fn enter(&mut self, p: Point, m: MouseMode) -> Result<()> {
        self.enter_at(p, m)
    }
    fn apply(&mut self, e: InputEvent, k: bool) -> Result<()> {
        self.apply_event(e, k)
    }
    fn position(&mut self) -> Result<Point> {
        platform::cursor_position()
    }
    fn release_all(&mut self) {
        platform::Injector::release_all(self)
    }
    fn park(&mut self) -> Result<()> {
        platform::park_cursor()
    }
}
pub fn start(
    rt: Arc<Runtime>,
    config: Config,
    key: [u8; 32],
    target: Option<String>,
) -> Result<()> {
    start_with_input(rt, config, key, target, platform::Injector::default())
}
fn start_with_input(
    rt: Arc<Runtime>,
    config: Config,
    key: [u8; 32],
    target: Option<String>,
    mut input: impl SessionInput + 'static,
) -> Result<()> {
    ensure!(
        rt.running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok(),
        "工作组已运行，请先停止连接"
    );
    rt.stop.store(false, Ordering::SeqCst);
    rt.paused.store(false, Ordering::SeqCst);
    rt.emergency.store(false, Ordering::SeqCst);
    let (tx, rx) = mpsc::sync_channel(4096);
    let (etx, erx) = mpsc::sync_channel(4096);
    let host = target.is_none();
    let network = match network::launch(rt.clone(), &config, key, target, etx) {
        Ok(n) => n,
        Err(e) => {
            rt.running.store(false, Ordering::SeqCst);
            return Err(e);
        }
    };
    *rt.commands.lock().unwrap() = Some(tx);
    rt.status.lock().unwrap().host = host;
    rt.log(if host {
        "工作组已创建，等待其他设备加入"
    } else {
        "正在加入工作组…"
    });
    thread::spawn(move || {
        let state = rt.snapshot();
        let mut group = Group {
            rt: rt.clone(),
            host,
            peers: BTreeMap::new(),
            layout: state.layout,
            revision: state.revision,
            control: None,
            epoch: 0,
            motion: None,
            pending: None,
            last_cursor: Instant::now(),
        };
        while !rt.stop.load(Ordering::SeqCst) {
            if rt.emergency.swap(false, Ordering::SeqCst) {
                input.release_all();
                group.pending = None;
                if let Some(c) = group.control.clone() {
                    let _ = group.submit(Message::Release { epoch: c.epoch }, &mut input);
                }
            }
            for event in erx.try_iter().take(512) {
                group.network_event(event, &mut input);
            }
            for cmd in rx.try_iter().take(256) {
                if let Err(e) = group.command(cmd, &mut input) {
                    rt.log(format!("操作未完成：{e:#}"));
                }
            }
            if group
                .pending
                .is_some_and(|t| t.elapsed() > Duration::from_secs(3))
            {
                rt.reclaim();
                group.pending = None;
                rt.log("控制切换超时，已回到本机");
            }
            if rt.incoming.load(Ordering::SeqCst)
                && rt.sharing_enabled()
                && group.last_cursor.elapsed() >= Duration::from_millis(8)
            {
                group.last_cursor = Instant::now();
                if let Some(c) = group.control.clone()
                    && c.mode == MouseMode::Relative
                {
                    match input.position() {
                        Ok(point) => {
                            let _ = group.submit(
                                Message::Cursor {
                                    epoch: c.epoch,
                                    point,
                                },
                                &mut input,
                            );
                        }
                        Err(e) => {
                            rt.reclaim();
                            rt.log(format!("读取光标失败，已暂停共享：{e:#}"));
                        }
                    }
                }
            }
            thread::sleep(Duration::from_millis(2));
        }
        input.release_all();
        for p in group.peers.values() {
            p.link.closed.store(true, Ordering::SeqCst);
        }
        *rt.commands.lock().unwrap() = None;
        drop(erx);
        let _ = network.join();
        rt.enabled.store(false, Ordering::SeqCst);
        rt.remote.store(false, Ordering::SeqCst);
        rt.incoming.store(false, Ordering::SeqCst);
        {
            let mut s = rt.status.lock().unwrap();
            s.connected = false;
            s.listening = false;
            s.reconnecting = false;
            s.control = None;
            for d in &mut s.layout.devices {
                d.online = d.info.id == rt.local.id;
            }
        }
        rt.log("工作组连接已停止");
        rt.running.store(false, Ordering::SeqCst);
    });
    Ok(())
}
struct Peer {
    device: DeviceInfo,
    link: Link,
}
struct Group {
    rt: Arc<Runtime>,
    host: bool,
    peers: BTreeMap<u64, Peer>,
    layout: Layout,
    revision: u64,
    control: Option<Control>,
    epoch: u64,
    motion: Option<RemoteMotion>,
    pending: Option<Instant>,
    last_cursor: Instant,
}
impl Group {
    fn publish(&self) {
        let mut s = self.rt.status.lock().unwrap();
        s.layout = self.layout.clone();
        s.revision = self.revision;
        s.connected = !self.peers.is_empty();
        s.control = self.control.clone();
    }
    fn topology(&self) -> Message {
        Message::Topology {
            revision: self.revision,
            layout: self.layout.clone(),
        }
    }
    fn broadcast(&self, message: Message) {
        for p in self.peers.values() {
            p.link.send(message.clone());
        }
    }
    fn enable(&self, input: &mut impl SessionInput) -> bool {
        if !self.rt.paused.load(Ordering::SeqCst) {
            match input.enable(self.rt.clone()) {
                Ok(()) => {
                    self.rt.enabled.store(true, Ordering::SeqCst);
                }
                Err(e) => {
                    self.rt.reclaim();
                    self.rt.log(format!("连接成功，但共享启动失败：{e:#}"));
                    return false;
                }
            }
        }
        true
    }
    fn network_event(&mut self, event: Event, input: &mut impl SessionInput) {
        match event {
            Event::Connected {
                connection,
                device,
                link,
            } => {
                let result = (|| -> Result<()> {
                    ensure!(
                        !self.peers.values().any(|p| p.device.id == device.id),
                        "该设备已连接"
                    );
                    ensure!(
                        self.host || self.peers.is_empty(),
                        "客户端只能加入一个工作组"
                    );
                    if self.host {
                        self.layout.upsert(device.clone(), true)?;
                        self.revision += 1;
                    }
                    self.peers.insert(
                        connection,
                        Peer {
                            device: device.clone(),
                            link: link.clone(),
                        },
                    );
                    self.rt.ever_connected.store(true, Ordering::SeqCst);
                    {
                        let mut s = self.rt.status.lock().unwrap();
                        s.peer_name = device.name.clone();
                        s.reconnecting = false;
                    }
                    self.publish();
                    let enabled_without_error = self.enable(input);
                    if self.host {
                        self.broadcast(self.topology());
                        link.send(Message::Control(self.control.clone()));
                    }
                    if enabled_without_error {
                        self.rt.log(format!(
                            "{} 已连接{}",
                            device.name,
                            if self.rt.sharing_enabled() {
                                ""
                            } else {
                                "，共享保持暂停"
                            }
                        ));
                    }
                    Ok(())
                })();
                if let Err(e) = result {
                    link.closed.store(true, Ordering::SeqCst);
                    self.rt.log(format!("拒绝连接：{e:#}"));
                }
            }
            Event::Message {
                connection,
                message,
            } => {
                if let Some(peer) = self.peers.get(&connection) {
                    let source = peer.device.id.clone();
                    let result = if self.host {
                        self.hub_message(&source, message, input)
                    } else {
                        self.client_message(message, input)
                    };
                    if let Err(e) = result {
                        self.rt.log(format!("协议错误：{e:#}"));
                        if let Some(p) = self.peers.get(&connection) {
                            p.link.reject();
                        }
                    }
                }
            }
            Event::Disconnected { connection, detail } => {
                if let Some(peer) = self.peers.remove(&connection) {
                    if self.host {
                        if let Some(d) = self
                            .layout
                            .devices
                            .iter_mut()
                            .find(|d| d.info.id == peer.device.id)
                        {
                            d.online = false;
                        }
                        if self.control.as_ref().is_some_and(|c| {
                            c.controller == peer.device.id || c.target == peer.device.id
                        }) {
                            self.set_control(None, input);
                        }
                        self.revision += 1;
                        self.broadcast(self.topology());
                    } else {
                        self.set_control(None, input);
                        for d in &mut self.layout.devices {
                            d.online = d.info.id == self.rt.local.id;
                        }
                    }
                    if self.peers.is_empty() {
                        self.rt.enabled.store(false, Ordering::SeqCst);
                    }
                    self.publish();
                }
                self.rt.log(detail);
            }
            Event::Status { detail, retrying } => {
                self.rt.status.lock().unwrap().reconnecting = retrying;
                self.rt.log(detail);
            }
        }
    }
    fn command(&mut self, cmd: Command, input: &mut impl SessionInput) -> Result<()> {
        match cmd {
            #[cfg(test)]
            Command::DropLinks => {
                for peer in self.peers.values() {
                    peer.link.closed.store(true, Ordering::SeqCst);
                }
                Ok(())
            }
            Command::Take { target, point } => self.take(target, point, input),
            Command::Motion(epoch, dx, dy) => {
                if self
                    .control
                    .as_ref()
                    .is_some_and(|c| c.epoch == epoch && c.controller == self.rt.local.id)
                {
                    self.motion(dx, dy, input)
                } else {
                    Ok(())
                }
            }
            Command::Input(epoch, event) => {
                if self.rt.sharing_enabled()
                    && self.rt.remote.load(Ordering::SeqCst)
                    && self.pending.is_none()
                    && let Some(c) = self.control.clone()
                    && c.controller == self.rt.local.id
                    && c.epoch == epoch
                {
                    self.submit(
                        Message::Input {
                            epoch: c.epoch,
                            event,
                        },
                        input,
                    )?;
                }
                Ok(())
            }
            Command::Arrange(revision, positions) => self.submit(
                Message::Arrange {
                    revision,
                    positions,
                },
                input,
            ),
            Command::Clipboard(target, text) => {
                self.submit(Message::SendClipboard { target, text }, input)
            }
        }
    }
    fn take(&mut self, target: String, point: Point, input: &mut impl SessionInput) -> Result<()> {
        if !self.rt.sharing_enabled() {
            self.rt.remote.store(false, Ordering::SeqCst);
            return Ok(());
        }
        self.pending = Some(Instant::now());
        let mode = if self.rt.relative_mouse.load(Ordering::SeqCst) {
            MouseMode::Relative
        } else {
            MouseMode::ScreenRelative
        };
        self.submit(
            Message::TakeControl {
                target,
                point,
                mode,
            },
            input,
        )
    }
    fn submit(&mut self, message: Message, input: &mut impl SessionInput) -> Result<()> {
        if self.host {
            let source = self.rt.local.id.clone();
            self.hub_message(&source, message, input)
        } else {
            let peer = self
                .peers
                .values()
                .next()
                .ok_or_else(|| anyhow::anyhow!("连接中心已离线"))?;
            ensure!(peer.link.send(message), "连接队列不可用");
            Ok(())
        }
    }
    fn deliver(
        &mut self,
        target: &str,
        message: Message,
        input: &mut impl SessionInput,
    ) -> Result<()> {
        if target == self.rt.local.id {
            self.client_message(message, input)
        } else {
            let peer = self
                .peers
                .values()
                .find(|p| p.device.id == target)
                .ok_or_else(|| anyhow::anyhow!("目标设备离线"))?;
            ensure!(peer.link.send(message), "目标设备队列不可用");
            Ok(())
        }
    }
    fn hub_message(
        &mut self,
        source: &str,
        message: Message,
        input: &mut impl SessionInput,
    ) -> Result<()> {
        match message {
            Message::TakeControl {
                target,
                point,
                mode,
            } => {
                let d = self
                    .layout
                    .get(&target)
                    .ok_or_else(|| anyhow::anyhow!("目标设备不存在"))?;
                ensure!(
                    d.online && point.x.is_finite() && point.y.is_finite(),
                    "控制目标无效"
                );
                let point = d.info.clamp(point);
                self.epoch = self.epoch.wrapping_add(1).max(1);
                let c = Control {
                    controller: source.into(),
                    target,
                    point,
                    mode,
                    epoch: self.epoch,
                };
                self.set_control(Some(c), input);
            }
            Message::Release { epoch } => {
                if self.control.as_ref().is_some_and(|c| {
                    c.epoch == epoch && (c.controller == source || c.target == source)
                }) {
                    self.set_control(None, input);
                }
            }
            Message::Input { epoch, event } => {
                if let Some(c) = self.control.clone()
                    && c.epoch == epoch
                    && c.controller == source
                    && c.target != c.controller
                {
                    self.deliver(&c.target, Message::Input { epoch, event }, input)?;
                }
            }
            Message::Cursor { epoch, point } => {
                if let Some(c) = self.control.clone()
                    && c.epoch == epoch
                    && c.target == source
                    && c.mode == MouseMode::Relative
                {
                    ensure!(
                        point.x.is_finite()
                            && point.y.is_finite()
                            && self
                                .layout
                                .get(source)
                                .is_some_and(|d| d.info.clamp(point) == point),
                        "远端光标坐标无效"
                    );
                    self.deliver(&c.controller, Message::Cursor { epoch, point }, input)?;
                }
            }
            Message::Arrange {
                revision,
                positions,
            } => {
                let result = (|| -> Result<Layout> {
                    ensure!(
                        revision == self.revision,
                        "其他设备已修改布局，请重新拖动并保存"
                    );
                    self.layout.positioned(&positions)
                })();
                match result {
                    Ok(layout) => {
                        self.set_control(None, input);
                        self.layout = layout;
                        self.revision += 1;
                        self.publish();
                        self.broadcast(self.topology());
                        self.rt.log("自由布局已保存并同步到工作组");
                    }
                    Err(e) => {
                        self.deliver(source, Message::Notice(format!("布局未保存：{e:#}")), input)?;
                        self.deliver(source, self.topology(), input)?;
                    }
                }
            }
            Message::SendClipboard { target, text } => {
                ensure!(text.len() <= MAX_TEXT_BYTES, "剪贴板文本超过 32 KB");
                self.deliver(
                    &target,
                    Message::DeliverClipboard {
                        source: source.into(),
                        text,
                    },
                    input,
                )?;
            }
            _ => bail!("客户端发送了不允许的消息"),
        }
        Ok(())
    }
    fn set_control(&mut self, control: Option<Control>, input: &mut impl SessionInput) {
        // Central epochs serialize competing controllers and reject stale input.
        self.broadcast(Message::Control(control.clone()));
        if let Err(e) = self.apply_control(control, input) {
            self.rt.reclaim();
            self.rt.log(format!("输入定位失败，已暂停共享：{e:#}"));
        }
    }
    fn apply_control(
        &mut self,
        control: Option<Control>,
        input: &mut impl SessionInput,
    ) -> Result<()> {
        input.release_all();
        self.pending = None;
        self.motion = None;
        self.rt.capture_epoch.store(0, Ordering::SeqCst);
        self.rt.remote.store(false, Ordering::SeqCst);
        self.rt.incoming.store(false, Ordering::SeqCst);
        self.rt.block_switch();
        self.control = control.clone();
        self.publish();
        if let Some(c) = control {
            let target = self
                .layout
                .get(&c.target)
                .ok_or_else(|| anyhow::anyhow!("控制设备不在布局中"))?;
            ensure!(
                target.online
                    && self.layout.get(&c.controller).is_some_and(|d| d.online)
                    && c.epoch > 0
                    && c.point.x.is_finite()
                    && c.point.y.is_finite()
                    && target.info.clamp(c.point) == c.point,
                "控制状态无效"
            );
            if (c.target == self.rt.local.id || c.controller == self.rt.local.id)
                && !self.rt.sharing_enabled()
            {
                self.rt.emergency.store(true, Ordering::SeqCst);
                return Ok(());
            }
            if c.target == self.rt.local.id {
                input.enter(c.point, c.mode)?;
                self.rt
                    .incoming
                    .store(c.controller != self.rt.local.id, Ordering::SeqCst);
            } else if c.controller == self.rt.local.id {
                input.park()?;
                self.rt.capture_epoch.store(c.epoch, Ordering::SeqCst);
                self.motion = Some(RemoteMotion::new(
                    self.rt.local.primary(),
                    target.info.clone(),
                    c.point,
                    c.mode,
                ));
                self.rt.remote.store(true, Ordering::SeqCst);
            }
        }
        Ok(())
    }
    fn client_message(&mut self, message: Message, input: &mut impl SessionInput) -> Result<()> {
        match message {
            Message::Topology { revision, layout } => {
                layout.validate()?;
                ensure!(
                    layout
                        .get(&self.rt.local.id)
                        .is_some_and(|d| d.online && d.info == self.rt.local),
                    "工作组中的本机屏幕信息不匹配"
                );
                self.layout = layout;
                self.revision = revision;
                self.publish();
            }
            Message::Control(c) => {
                if let Err(e) = self.apply_control(c, input) {
                    if e.is::<InputInjectionError>() {
                        input.release_all();
                        self.rt.reclaim();
                        self.rt.log(format!("输入注入失败，连接保持：{e:#}"));
                    } else {
                        return Err(e);
                    }
                }
            }
            Message::Input { epoch, event } => {
                if self.rt.sharing_enabled()
                    && self.rt.incoming.load(Ordering::SeqCst)
                    && self
                        .control
                        .as_ref()
                        .is_some_and(|c| c.epoch == epoch && c.target == self.rt.local.id)
                    && let Err(e) = input.apply(event, self.rt.keyboard.load(Ordering::SeqCst))
                {
                    if e.is::<InputInjectionError>() {
                        input.release_all();
                        self.rt.reclaim();
                        self.rt.log(format!("输入注入失败，连接保持：{e:#}"));
                    } else {
                        return Err(e);
                    }
                }
            }
            Message::Cursor { epoch, point } => {
                if self
                    .control
                    .as_ref()
                    .is_some_and(|c| c.epoch == epoch && c.controller == self.rt.local.id)
                    && let Some(m) = self.motion.as_mut()
                {
                    m.point = point;
                    self.try_cross(input)?;
                }
            }
            Message::DeliverClipboard { source: _, text } => {
                ensure!(text.len() <= MAX_TEXT_BYTES, "剪贴板文本过长");
                if self.rt.sharing_enabled() {
                    match input.clipboard(text) {
                        Ok(()) => self.rt.log("已收到剪贴板文本"),
                        Err(e) => self.rt.log(format!("剪贴板写入失败，连接保持：{e:#}")),
                    }
                }
            }
            Message::Notice(text) => {
                ensure!(text.len() <= 2048, "提示消息过长");
                self.rt.log(text);
            }
            _ => bail!("连接中心发送了不允许的消息"),
        }
        Ok(())
    }
    fn motion(&mut self, dx: i32, dy: i32, input: &mut impl SessionInput) -> Result<()> {
        ensure!(
            dx.unsigned_abs() <= 32768 && dy.unsigned_abs() <= 32768,
            "鼠标位移无效"
        );
        if !self.rt.sharing_enabled()
            || !self.rt.remote.load(Ordering::SeqCst)
            || self.pending.is_some()
        {
            return Ok(());
        }
        let Some(m) = self.motion.as_mut() else {
            return Ok(());
        };
        let p = m.advance(dx, dy);
        let mode = m.mode;
        if self.try_cross(input)? {
            return Ok(());
        }
        if let Some(c) = self.control.clone() {
            let event = if mode == MouseMode::Relative {
                InputEvent::Relative { dx, dy }
            } else {
                InputEvent::Absolute {
                    x: p.x.round() as i32,
                    y: p.y.round() as i32,
                }
            };
            self.submit(
                Message::Input {
                    epoch: c.epoch,
                    event,
                },
                input,
            )?;
        }
        Ok(())
    }
    fn try_cross(&mut self, input: &mut impl SessionInput) -> Result<bool> {
        if self.pending.is_some() || !self.rt.can_switch() || !self.rt.edge.load(Ordering::SeqCst) {
            return Ok(false);
        }
        let next = self
            .control
            .as_ref()
            .zip(self.motion.as_ref())
            .and_then(|(c, m)| {
                self.layout.crossing(
                    &c.target,
                    m.point,
                    m.direction,
                    self.rt.block_corners.load(Ordering::SeqCst),
                )
            });
        if let Some((id, p)) = next {
            self.take(id, p, input)?;
            return Ok(true);
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests;

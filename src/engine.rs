use anyhow::{Context, Result, bail, ensure};
use std::{
    collections::VecDeque,
    io::{ErrorKind, Read, Write},
    net::{Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread,
    time::{Duration, Instant},
};

use crate::{
    config::Config,
    motion::SWITCH_DELAY,
    platform::{Injector, InputInjectionError},
    protocol::{MAX_TEXT_BYTES, Message, MouseMode, VERSION},
    transport,
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
}

pub struct Runtime {
    pub status: Mutex<Status>,
    pub running: AtomicBool,
    pub stop: AtomicBool,
    pub enabled: AtomicBool,
    paused: AtomicBool,
    pub remote: AtomicBool,
    pub incoming: AtomicBool,
    pub keyboard: AtomicBool,
    pub edge: AtomicBool,
    pub right: AtomicBool,
    pub relative_mouse: AtomicBool,
    pub auto_reconnect: AtomicBool,
    pub block_corners: AtomicBool,
    pub(crate) capture_epoch: AtomicU64,
    pub(crate) capture_y: AtomicU16,
    pub(crate) capture_relative: AtomicBool,
    switch_after: AtomicU64,
    pub abort: AtomicBool,
    pub emergency: AtomicBool,
    pub sent: AtomicU64,
    pub received: AtomicU64,
    outgoing: Mutex<Option<SyncSender<Message>>>,
    active_socket: Mutex<Option<TcpStream>>,
}

impl Runtime {
    pub fn new(config: &Config) -> Arc<Self> {
        Arc::new(Self {
            status: Mutex::new(Status {
                detail: "尚未启动连接".into(),
                ..Status::default()
            }),
            running: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            enabled: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            remote: AtomicBool::new(false),
            incoming: AtomicBool::new(false),
            keyboard: AtomicBool::new(config.keyboard),
            edge: AtomicBool::new(config.edge_switch),
            right: AtomicBool::new(config.peer_on_right),
            relative_mouse: AtomicBool::new(config.relative_mouse),
            auto_reconnect: AtomicBool::new(config.auto_reconnect),
            block_corners: AtomicBool::new(config.block_corners),
            capture_epoch: AtomicU64::new(0),
            capture_y: AtomicU16::new(32768),
            capture_relative: AtomicBool::new(false),
            switch_after: AtomicU64::new(0),
            abort: AtomicBool::new(false),
            emergency: AtomicBool::new(false),
            sent: AtomicU64::new(0),
            received: AtomicU64::new(0),
            outgoing: Mutex::new(None),
            active_socket: Mutex::new(None),
        })
    }

    pub fn snapshot(&self) -> Status {
        self.status.lock().unwrap().clone()
    }

    pub fn log(&self, text: impl Into<String>) {
        let text = text.into();
        let mut state = self.status.lock().unwrap();
        state.detail = text.clone();
        state.events.push_front(text);
        state.events.truncate(8);
    }

    // Called from low-level hooks: never wait for the network or a contended mutex.
    pub fn send(&self, msg: Message) -> bool {
        let Ok(guard) = self.outgoing.try_lock() else {
            self.fail_open();
            return false;
        };
        let Some(tx) = guard.as_ref() else {
            // Offline settings changes and local returns are not queue failures.
            self.remote.store(false, Ordering::SeqCst);
            self.enabled.store(false, Ordering::SeqCst);
            return false;
        };
        if tx.try_send(msg).is_ok() {
            return true;
        }
        self.fail_open();
        false
    }

    fn fail_open(&self) {
        self.paused.store(true, Ordering::SeqCst);
        self.remote.store(false, Ordering::SeqCst);
        self.enabled.store(false, Ordering::SeqCst);
        self.abort.store(true, Ordering::SeqCst);
    }

    pub fn return_local(&self) {
        self.block_switch();
        self.remote.store(false, Ordering::SeqCst);
        self.incoming.store(false, Ordering::SeqCst);
        self.emergency.store(true, Ordering::SeqCst);
        self.send(Message::Leave);
    }

    pub(crate) fn can_switch(&self) -> bool {
        switch_tick() >= self.switch_after.load(Ordering::SeqCst)
    }

    fn block_switch(&self) {
        self.switch_after.store(
            switch_tick() + SWITCH_DELAY.as_millis() as u64,
            Ordering::SeqCst,
        );
    }

    pub(crate) fn begin_control(&self, x: u16, y: u16) -> bool {
        if !self.sharing_enabled()
            || self.incoming.load(Ordering::SeqCst)
            || self.remote.load(Ordering::SeqCst)
            || !self.can_switch()
        {
            return false;
        }
        let relative = self.relative_mouse.load(Ordering::SeqCst);
        self.capture_y.store(y, Ordering::SeqCst);
        self.capture_relative.store(relative, Ordering::SeqCst);
        self.capture_epoch.fetch_add(1, Ordering::SeqCst);
        self.remote.store(true, Ordering::SeqCst);
        self.block_switch();
        self.send(Message::Enter {
            from_left: self.right.load(Ordering::SeqCst),
            x,
            y,
            mode: if relative {
                MouseMode::Relative
            } else {
                MouseMode::ScreenRelative
            },
        })
    }

    pub fn reclaim(&self) {
        self.paused.store(true, Ordering::SeqCst);
        self.enabled.store(false, Ordering::SeqCst);
        self.return_local();
    }

    pub fn resume(&self) -> Result<()> {
        ensure!(
            self.snapshot().connected
                && !self.stop.load(Ordering::SeqCst)
                && !self.abort.load(Ordering::SeqCst),
            "请先连接另一台电脑"
        );
        self.paused.store(false, Ordering::SeqCst);
        self.enabled.store(true, Ordering::SeqCst);
        Ok(())
    }

    pub fn sharing_enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
            && !self.paused.load(Ordering::SeqCst)
            && !self.stop.load(Ordering::SeqCst)
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        self.reclaim();
        if let Some(socket) = self.active_socket.lock().unwrap().as_ref() {
            let _ = socket.shutdown(Shutdown::Both);
        }
    }
}

fn switch_tick() -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_millis() as u64
}

pub fn start(
    runtime: Arc<Runtime>,
    config: Config,
    key: [u8; 32],
    target: Option<String>,
) -> Result<()> {
    start_with_input(runtime, config, key, target, Injector::default())
}

fn start_with_input(
    runtime: Arc<Runtime>,
    config: Config,
    key: [u8; 32],
    target: Option<String>,
    mut injector: impl SessionInput + Send + 'static,
) -> Result<()> {
    ensure!(
        runtime
            .running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok(),
        "连接正在运行，请先停止"
    );
    // Bind synchronously so a port conflict is reported before the UI says it is listening.
    let listener = (|| -> Result<_> {
        let listener =
            TcpListener::bind(("0.0.0.0", config.port)).context("无法监听端口，可能已被占用")?;
        listener.set_nonblocking(true)?;
        Ok(listener)
    })();
    let listener = match listener {
        Ok(listener) => listener,
        Err(err) => {
            runtime.running.store(false, Ordering::SeqCst);
            return Err(err);
        }
    };
    runtime.stop.store(false, Ordering::SeqCst);
    runtime.abort.store(false, Ordering::SeqCst);
    // An explicit start is a new pairing run. Automatic retries never clear pause.
    runtime.paused.store(false, Ordering::SeqCst);
    runtime.emergency.store(false, Ordering::SeqCst);
    runtime.status.lock().unwrap().listening = true;
    runtime.log(format!("正在监听 TCP {}，等待另一台电脑", config.port));
    let rt = runtime.clone();
    thread::spawn(move || {
        let mut retry = RetrySchedule::default();
        while !rt.stop.load(Ordering::SeqCst) {
            let automatic = rt.auto_reconnect.load(Ordering::SeqCst);
            if let Some(target) = target.as_deref()
                && retry.ready(Instant::now(), automatic)
            {
                rt.status.lock().unwrap().reconnecting = retry.attempts > 0;
                rt.log(format!("正在连接 {target}…"));
                let outcome = match connect(target, config.port, &rt.stop) {
                    Ok(stream) => {
                        run_session_with_input(&rt, stream, &config, &key, true, &mut injector)
                    }
                    Err(err) => {
                        rt.log(format!("连接失败：{err:#}"));
                        SessionOutcome {
                            established: false,
                            retry: true,
                        }
                    }
                };
                retry.after_session(outcome, Instant::now());
                report_retry(&rt, &retry, target);
            }
            if rt.stop.load(Ordering::SeqCst) {
                break;
            }
            match listener.accept() {
                Ok((stream, _)) => {
                    let outcome =
                        run_session_with_input(&rt, stream, &config, &key, false, &mut injector);
                    // An unauthenticated inbound connection must not cancel the
                    // user's pending outbound retry or change its backoff.
                    if outcome.established
                        && let Some(target) = target.as_deref()
                    {
                        retry.after_session(outcome, Instant::now());
                        report_retry(&rt, &retry, target);
                    }
                }
                Err(err) if err.kind() == ErrorKind::WouldBlock => {}
                Err(err) => {
                    rt.log(format!("监听失败：{err}"));
                    break;
                }
            }
            rt.status.lock().unwrap().reconnecting = target.is_some()
                && !retry.blocked
                && retry.attempts > 0
                && rt.auto_reconnect.load(Ordering::SeqCst);
            thread::sleep(Duration::from_millis(20));
        }
        rt.enabled.store(false, Ordering::SeqCst);
        rt.remote.store(false, Ordering::SeqCst);
        rt.incoming.store(false, Ordering::SeqCst);
        {
            let mut status = rt.status.lock().unwrap();
            status.listening = false;
            status.reconnecting = false;
        }
        rt.log("连接已停止");
        rt.running.store(false, Ordering::SeqCst);
    });
    Ok(())
}

struct SessionOutcome {
    established: bool,
    retry: bool,
}

#[derive(Default)]
struct RetrySchedule {
    attempts: u32,
    due: Option<Instant>,
    blocked: bool,
}

impl RetrySchedule {
    fn ready(&self, now: Instant, automatic: bool) -> bool {
        !self.blocked
            && (self.attempts == 0 || (automatic && self.due.is_some_and(|due| now >= due)))
    }

    fn after_session(&mut self, outcome: SessionOutcome, now: Instant) {
        if outcome.established {
            self.attempts = 0;
        }
        self.attempts = self.attempts.saturating_add(1);
        self.blocked = !outcome.retry;
        self.due = Some(now + self.delay());
    }

    fn delay(&self) -> Duration {
        Duration::from_secs((1u64 << self.attempts.saturating_sub(1).min(5)).min(30))
    }
}

fn report_retry(rt: &Runtime, retry: &RetrySchedule, target: &str) {
    if rt.stop.load(Ordering::SeqCst) {
        return;
    }
    if retry.blocked {
        rt.log("自动重连已停止：请检查配对密钥或两端版本，停止连接后重试（详见连接记录）");
    } else if rt.auto_reconnect.load(Ordering::SeqCst) {
        rt.log(format!(
            "{} 秒后重连 {target}（第 {} 次），可点击停止连接取消",
            retry.delay().as_secs(),
            retry.attempts
        ));
    }
}

fn connect(target: &str, port: u16, stop: &AtomicBool) -> Result<TcpStream> {
    let addresses: Vec<SocketAddr> = if target.parse::<std::net::IpAddr>().is_ok() {
        (target, port).to_socket_addrs()?.collect()
    } else if target.contains(':') {
        target.to_socket_addrs()?.collect()
    } else {
        (target, port).to_socket_addrs()?.collect()
    };
    for addr in addresses {
        ensure!(!stop.load(Ordering::SeqCst), "连接已取消");
        if let Ok(stream) = TcpStream::connect_timeout(&addr, Duration::from_secs(3)) {
            return Ok(stream);
        }
    }
    bail!("无法连接 {target}；请确认对方已启动监听、地址正确且防火墙允许 TCP {port}")
}

// Keep network/control tests on the production state machine without installing
// global hooks or moving the real desktop pointer in a test process.
trait SessionInput {
    fn enable(&mut self, runtime: Arc<Runtime>) -> Result<()>;
    fn enter(&mut self, from_left: bool, x: u16, y: u16, mode: MouseMode) -> Result<()>;
    fn apply(
        &mut self,
        message: Message,
        keyboard: bool,
        block_corners: bool,
    ) -> Result<Option<u16>>;
    fn poll_return(&mut self, _block_corners: bool) -> Result<Option<u16>> {
        Ok(None)
    }
    fn set_clipboard(&mut self, text: String) -> Result<()> {
        arboard::Clipboard::new()?.set_text(text)?;
        Ok(())
    }
    fn return_cursor(&mut self, peer_on_right: bool, y: u16) -> Result<()>;
    fn release_all(&mut self);
    fn park_cursor(&mut self);
}

impl SessionInput for Injector {
    fn enable(&mut self, runtime: Arc<Runtime>) -> Result<()> {
        crate::platform::install_hooks(runtime)
    }
    fn enter(&mut self, from_left: bool, x: u16, y: u16, mode: MouseMode) -> Result<()> {
        Injector::enter(self, from_left, x, y, mode)
    }
    fn apply(
        &mut self,
        message: Message,
        keyboard: bool,
        block_corners: bool,
    ) -> Result<Option<u16>> {
        Injector::apply(self, message, keyboard, block_corners)
    }
    fn poll_return(&mut self, block_corners: bool) -> Result<Option<u16>> {
        Injector::poll_return(self, block_corners)
    }
    fn return_cursor(&mut self, peer_on_right: bool, y: u16) -> Result<()> {
        Injector::return_cursor(self, peer_on_right, y)
    }
    fn release_all(&mut self) {
        Injector::release_all(self);
    }
    fn park_cursor(&mut self) {
        // A peer relinquishing control must not turn a failed cosmetic warp
        // into another input-failure notification loop.
        let _ = crate::platform::park_cursor();
    }
}

fn run_session_with_input(
    rt: &Arc<Runtime>,
    mut stream: TcpStream,
    config: &Config,
    key: &[u8; 32],
    initiator: bool,
    injector: &mut impl SessionInput,
) -> SessionOutcome {
    rt.log("正在验证配对密钥…");
    let result = (|| -> Result<()> {
        *rt.active_socket.lock().unwrap() = Some(stream.try_clone()?);
        ensure!(!rt.stop.load(Ordering::SeqCst), "连接已取消");
        let mut noise =
            transport::handshake_with_cancel(&mut stream, key, initiator, Some(&rt.stop))?;
        ensure!(!rt.stop.load(Ordering::SeqCst), "连接已取消");
        stream.set_nonblocking(true)?;
        let address = stream.peer_addr()?.to_string();
        let (tx, rx) = mpsc::sync_channel(2048);
        *rt.outgoing.lock().unwrap() = Some(tx);
        rt.abort.store(false, Ordering::SeqCst);
        session(rt, &mut stream, &mut noise, &rx, config, address, injector)
    })();
    rt.enabled.store(false, Ordering::SeqCst);
    rt.remote.store(false, Ordering::SeqCst);
    rt.incoming.store(false, Ordering::SeqCst);
    *rt.outgoing.lock().unwrap() = None;
    *rt.active_socket.lock().unwrap() = None;
    injector.release_all();
    let established = {
        let mut status = rt.status.lock().unwrap();
        let established = status.connected;
        status.connected = false;
        established
    };
    // IO loss/timeout and fail-open queue aborts may retry. Authentication,
    // framing and protocol errors need user attention, never a tight retry loop.
    let retry = result
        .as_ref()
        .err()
        .is_none_or(|err| err.is::<std::io::Error>());
    let _ = stream.shutdown(Shutdown::Both);
    match result {
        Ok(()) => rt.log("已断开，控制权已回到本机"),
        Err(err) => rt.log(format!("连接结束：{err:#}")),
    }
    SessionOutcome { established, retry }
}

fn session(
    rt: &Arc<Runtime>,
    stream: &mut TcpStream,
    noise: &mut snow::TransportState,
    rx: &Receiver<Message>,
    config: &Config,
    address: String,
    injector: &mut impl SessionInput,
) -> Result<()> {
    let mut pending = transport::encode(
        noise,
        &Message::Hello {
            version: VERSION,
            name: config.name.clone(),
        },
    )?;
    let mut written = 0;
    let mut incoming = Vec::new();
    let mut buffer = [0; 8192];
    let mut last_receive = Instant::now();
    let mut last_ping = Instant::now();
    let mut hello = false;
    while !rt.stop.load(Ordering::SeqCst) && !rt.abort.load(Ordering::SeqCst) {
        if rt.emergency.swap(false, Ordering::SeqCst) {
            injector.release_all();
        }
        if rt.sharing_enabled() && rt.incoming.load(Ordering::SeqCst) {
            let result = injector.poll_return(rt.block_corners.load(Ordering::SeqCst));
            if let Some(Some(y)) = input_result(rt, injector, result)? {
                finish_return(rt, injector, y);
            }
        }
        if written == pending.len() {
            pending.clear();
            written = 0;
            for msg in rx.try_iter().take(256) {
                pending.extend(transport::encode(noise, &msg)?);
                rt.sent.fetch_add(1, Ordering::Relaxed);
            }
            if last_ping.elapsed() >= Duration::from_secs(1) {
                pending.extend(transport::encode(noise, &Message::Ping)?);
                last_ping = Instant::now();
            }
        }
        if written < pending.len() {
            match stream.write(&pending[written..]) {
                Ok(0) => {
                    return Err(std::io::Error::new(ErrorKind::UnexpectedEof, "对方已断开").into());
                }
                Ok(n) => written += n,
                Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                Err(e) => return Err(e.into()),
            }
        }
        match stream.read(&mut buffer) {
            Ok(0) => return Err(std::io::Error::new(ErrorKind::UnexpectedEof, "对方已断开").into()),
            Ok(n) => {
                incoming.extend_from_slice(&buffer[..n]);
                for msg in transport::decode_available(noise, &mut incoming)? {
                    last_receive = Instant::now();
                    rt.received.fetch_add(1, Ordering::Relaxed);
                    if !hello {
                        if let Message::Hello { version, name } = msg {
                            ensure!(
                                version == VERSION,
                                "两端协议版本不同，请将两台电脑都更新到 0.2.0 或更高版本"
                            );
                            ensure!(name.len() <= 256, "设备名称过长");
                            let input_result = if rt.paused.load(Ordering::SeqCst) {
                                Ok(())
                            } else {
                                injector.enable(rt.clone())
                            };
                            if input_result.is_err() {
                                rt.paused.store(true, Ordering::SeqCst);
                            }
                            rt.enabled.store(input_result.is_ok(), Ordering::SeqCst);
                            // Recheck after enabling so a concurrent emergency
                            // return cannot be undone by handshake completion.
                            if rt.paused.load(Ordering::SeqCst) {
                                rt.enabled.store(false, Ordering::SeqCst);
                            }
                            {
                                let mut s = rt.status.lock().unwrap();
                                s.connected = true;
                                s.reconnecting = false;
                                s.peer_name = name.clone();
                                s.address = address.clone();
                            }
                            match input_result {
                                Ok(()) if rt.paused.load(Ordering::SeqCst) => rt.log(format!(
                                    "已连接到 {name}，共享保持暂停；点击恢复共享继续"
                                )),
                                Ok(()) => rt.log(format!("已连接到 {name}，键鼠共享已自动开启")),
                                Err(err) => {
                                    rt.log(format!("已连接到 {name}，自动开启共享失败：{err:#}"))
                                }
                            }
                            hello = true;
                            continue;
                        }
                        bail!("缺少设备信息");
                    }
                    match msg {
                        Message::Ping => {}
                        Message::Hello { .. } => bail!("重复的设备握手"),
                        Message::Leave => {
                            rt.block_switch();
                            rt.remote.store(false, Ordering::SeqCst);
                            rt.incoming.store(false, Ordering::SeqCst);
                            injector.release_all();
                            injector.park_cursor();
                        }
                        Message::Return { y } if rt.remote.swap(false, Ordering::SeqCst) => {
                            rt.block_switch();
                            injector.release_all();
                            let result = injector.return_cursor(rt.right.load(Ordering::SeqCst), y);
                            input_result(rt, injector, result)?;
                        }
                        Message::Return { .. } => {}
                        Message::Enter {
                            from_left,
                            x,
                            y,
                            mode,
                        } if rt.sharing_enabled() => {
                            rt.block_switch();
                            rt.remote.store(false, Ordering::SeqCst);
                            rt.incoming.store(true, Ordering::SeqCst);
                            let result = injector.enter(from_left, x, y, mode);
                            input_result(rt, injector, result)?;
                        }
                        Message::Clipboard(text) if rt.sharing_enabled() => {
                            ensure!(text.len() <= MAX_TEXT_BYTES, "剪贴板文本超过 32 KB");
                            match injector.set_clipboard(text) {
                                Ok(()) => rt.log("已收到对方发送的剪贴板文本"),
                                Err(err) => {
                                    rt.log(format!("写入剪贴板失败，连接保持，可重新发送：{err:#}"))
                                }
                            }
                        }
                        Message::Enter { .. } => {
                            rt.send(Message::Leave);
                        }
                        other if rt.sharing_enabled() && rt.incoming.load(Ordering::SeqCst) => {
                            let result = injector.apply(
                                other,
                                rt.keyboard.load(Ordering::SeqCst),
                                rt.block_corners.load(Ordering::SeqCst),
                            );
                            if let Some(Some(y)) = input_result(rt, injector, result)? {
                                finish_return(rt, injector, y);
                            }
                        }
                        _ => {}
                    }
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => {}
            Err(e) => return Err(e.into()),
        }
        if last_receive.elapsed() >= Duration::from_secs(5) {
            return Err(
                std::io::Error::new(ErrorKind::TimedOut, "连接超时，已释放所有按键").into(),
            );
        }
        thread::sleep(Duration::from_millis(2));
    }
    Ok(())
}

fn finish_return(rt: &Runtime, injector: &mut impl SessionInput, y: u16) {
    rt.incoming.store(false, Ordering::SeqCst);
    rt.block_switch();
    injector.release_all();
    rt.send(Message::Return { y });
}

fn input_result<T>(
    rt: &Runtime,
    injector: &mut impl SessionInput,
    result: Result<T>,
) -> Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(err) if err.is::<InputInjectionError>() => {
            injector.release_all();
            rt.reclaim();
            rt.log(format!("输入注入失败，已暂停共享，连接保持：{err:#}"));
            Ok(None)
        }
        Err(err) => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::motion::{CaptureMotion, DesktopBounds, MotionStep, PixelPoint, PointerMotion};

    #[derive(Default)]
    struct InputProbe {
        enables: AtomicU64,
        enters: AtomicU64,
        releases: AtomicU64,
        events: Mutex<Vec<Message>>,
        entries: Mutex<Vec<(bool, u16, u16, MouseMode)>>,
        returns: Mutex<Vec<(bool, u16)>>,
        return_y: Mutex<Option<u16>>,
        positions: Mutex<Vec<PixelPoint>>,
        fail_enter: AtomicBool,
        fail_apply: AtomicBool,
        fail_return: AtomicBool,
        fail_clipboard: AtomicBool,
        clipboard: Mutex<Vec<String>>,
    }

    struct TestInput {
        probe: Arc<InputProbe>,
        fail_enable: bool,
        motion: PointerMotion,
        bounds: DesktopBounds,
    }

    impl SessionInput for TestInput {
        fn enable(&mut self, _: Arc<Runtime>) -> Result<()> {
            self.probe.enables.fetch_add(1, Ordering::SeqCst);
            ensure!(!self.fail_enable, "模拟输入钩子安装失败");
            Ok(())
        }
        fn enter(&mut self, from_left: bool, x: u16, y: u16, mode: MouseMode) -> Result<()> {
            if self.probe.fail_enter.load(Ordering::SeqCst) {
                return Err(InputInjectionError::new("鼠标", 0, 87).into());
            }
            self.probe
                .entries
                .lock()
                .unwrap()
                .push((from_left, x, y, mode));
            self.probe.positions.lock().unwrap().push(self.motion.enter(
                self.bounds,
                from_left,
                y,
                mode,
                x,
            ));
            self.probe.enters.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn apply(&mut self, message: Message, _: bool, block_corners: bool) -> Result<Option<u16>> {
            if self.probe.fail_apply.load(Ordering::SeqCst) {
                return Err(InputInjectionError::new("键盘", 0, 5).into());
            }
            let result = match message {
                Message::MoveAbsolute { x, y } => {
                    let can_return = self
                        .bounds
                        .outside_corner(self.bounds.point_at(x, y).y, block_corners);
                    match self.motion.absolute(self.bounds, x, y, can_return)? {
                        MotionStep::MoveTo(point) => {
                            self.probe.positions.lock().unwrap().push(point);
                            None
                        }
                        MotionStep::ReturnLocal { y } => Some(y),
                    }
                }
                Message::Move { dx, dy } => {
                    self.motion.relative(dx, dy)?;
                    None
                }
                _ => None,
            };
            self.probe.events.lock().unwrap().push(message);
            Ok(result)
        }
        fn poll_return(&mut self, block_corners: bool) -> Result<Option<u16>> {
            Ok(self.probe.return_y.lock().unwrap().take().filter(|y| {
                self.bounds
                    .outside_corner(self.bounds.point_at(0, i32::from(*y)).y, block_corners)
            }))
        }
        fn set_clipboard(&mut self, text: String) -> Result<()> {
            ensure!(
                !self.probe.fail_clipboard.load(Ordering::SeqCst),
                "模拟剪贴板被占用"
            );
            self.probe.clipboard.lock().unwrap().push(text);
            Ok(())
        }
        fn return_cursor(&mut self, peer_on_right: bool, y: u16) -> Result<()> {
            if self.probe.fail_return.load(Ordering::SeqCst) {
                return Err(InputInjectionError::new("鼠标", 0, 5).into());
            }
            self.probe.returns.lock().unwrap().push((peer_on_right, y));
            Ok(())
        }
        fn release_all(&mut self) {
            self.probe.releases.fetch_add(1, Ordering::SeqCst);
        }
        fn park_cursor(&mut self) {}
    }

    struct Loopback {
        a: Arc<Runtime>,
        b: Arc<Runtime>,
        a_input: Arc<InputProbe>,
        b_input: Arc<InputProbe>,
        workers: Vec<thread::JoinHandle<()>>,
    }

    impl Loopback {
        fn new(receiver_fails: bool) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let a_config = Config {
                name: "DESKTOP-A".into(),
                ..Config::default()
            };
            let b_config = Config {
                name: "LAPTOP-B".into(),
                relative_mouse: true,
                ..Config::default()
            };
            let a = Runtime::new(&a_config);
            let b = Runtime::new(&b_config);
            let a_input = Arc::new(InputProbe::default());
            let b_input = Arc::new(InputProbe::default());
            let a_worker = a.clone();
            let mut a_backend = TestInput {
                probe: a_input.clone(),
                fail_enable: false,
                motion: PointerMotion::default(),
                bounds: DesktopBounds::new(0, 0, 1920, 1080),
            };
            let server = thread::spawn(move || {
                let (stream, _) = listener.accept().unwrap();
                run_session_with_input(
                    &a_worker,
                    stream,
                    &a_config,
                    &[23; 32],
                    false,
                    &mut a_backend,
                );
            });
            let b_worker = b.clone();
            let mut b_backend = TestInput {
                probe: b_input.clone(),
                fail_enable: receiver_fails,
                motion: PointerMotion::default(),
                bounds: DesktopBounds::new(-3840, -1080, 3840, 2160),
            };
            let client = thread::spawn(move || {
                let stream = TcpStream::connect(addr).unwrap();
                run_session_with_input(
                    &b_worker,
                    stream,
                    &b_config,
                    &[23; 32],
                    true,
                    &mut b_backend,
                );
            });
            let pair = Self {
                a,
                b,
                a_input,
                b_input,
                workers: vec![server, client],
            };
            wait_until(|| pair.a.snapshot().connected && pair.b.snapshot().connected);
            pair
        }

        fn shutdown(&mut self) {
            self.a.stop.store(true, Ordering::SeqCst);
            self.b.stop.store(true, Ordering::SeqCst);
            for worker in self.workers.drain(..) {
                worker.join().unwrap();
            }
        }
    }

    impl Drop for Loopback {
        fn drop(&mut self) {
            self.shutdown();
        }
    }

    fn wait_until(mut predicate: impl FnMut() -> bool) {
        let started = Instant::now();
        while !predicate() {
            assert!(
                started.elapsed() < Duration::from_secs(4),
                "连接状态未及时更新"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn enter_message(from_left: bool) -> Message {
        Message::Enter {
            from_left,
            x: if from_left { 136 } else { 65364 },
            y: 32768,
            mode: MouseMode::Relative,
        }
    }

    fn test_input(probe: Arc<InputProbe>) -> TestInput {
        TestInput {
            probe,
            fail_enable: false,
            motion: PointerMotion::default(),
            bounds: DesktopBounds::new(0, 0, 1920, 1080),
        }
    }

    fn accept_socket(listener: &TcpListener) -> TcpStream {
        listener.set_nonblocking(true).unwrap();
        let mut stream = None;
        wait_until(|| match listener.accept() {
            Ok((accepted, _)) => {
                stream = Some(accepted);
                true
            }
            Err(err) if err.kind() == ErrorKind::WouldBlock => false,
            Err(err) => panic!("accept failed: {err}"),
        });
        stream.unwrap()
    }

    struct ReconnectingClient {
        runtime: Arc<Runtime>,
        input: Arc<InputProbe>,
        peers: Vec<(Arc<Runtime>, thread::JoinHandle<()>)>,
    }

    impl ReconnectingClient {
        fn new(address: SocketAddr, automatic: bool) -> Self {
            let config = Config {
                port: 0,
                auto_reconnect: automatic,
                ..Config::default()
            };
            let runtime = Runtime::new(&config);
            let input = Arc::new(InputProbe::default());
            start_with_input(
                runtime.clone(),
                config,
                [23; 32],
                Some(address.to_string()),
                test_input(input.clone()),
            )
            .unwrap();
            Self {
                runtime,
                input,
                peers: Vec::new(),
            }
        }

        fn accept_peer(&mut self, listener: &TcpListener) -> Arc<Runtime> {
            let stream = accept_socket(listener);
            let config = Config::default();
            let runtime = Runtime::new(&config);
            let rt = runtime.clone();
            let worker = thread::spawn(move || {
                run_session_with_input(
                    &rt,
                    stream,
                    &config,
                    &[23; 32],
                    false,
                    &mut test_input(Arc::new(InputProbe::default())),
                );
            });
            self.peers.push((runtime.clone(), worker));
            wait_until(|| self.runtime.snapshot().connected && runtime.snapshot().connected);
            runtime
        }
    }

    impl Drop for ReconnectingClient {
        fn drop(&mut self) {
            self.runtime.stop();
            for (peer, worker) in self.peers.drain(..) {
                peer.stop();
                worker.join().unwrap();
            }
            wait_until(|| !self.runtime.running.load(Ordering::SeqCst));
        }
    }

    #[test]
    fn retry_backoff_is_bounded_resets_after_success_and_blocks_protocol_errors() {
        let mut retry = RetrySchedule::default();
        let now = Instant::now();
        assert!(retry.ready(now, false)); // Explicit first attempt always runs.
        for seconds in [1, 2, 4, 8, 16, 30, 30] {
            retry.after_session(
                SessionOutcome {
                    established: false,
                    retry: true,
                },
                now,
            );
            assert_eq!(retry.delay(), Duration::from_secs(seconds));
            assert!(!retry.ready(now, true));
            assert!(retry.ready(now + retry.delay(), true));
            assert!(!retry.ready(now + retry.delay(), false));
        }
        retry.after_session(
            SessionOutcome {
                established: true,
                retry: true,
            },
            now,
        );
        assert_eq!(retry.delay(), Duration::from_secs(1));
        retry.after_session(
            SessionOutcome {
                established: false,
                retry: false,
            },
            now,
        );
        assert!(!retry.ready(now + Duration::from_secs(60), true));
    }

    #[test]
    fn reconnect_restores_normal_sharing_but_preserves_explicit_pause() {
        for paused in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let mut client = ReconnectingClient::new(listener.local_addr().unwrap(), true);
            let peer = client.accept_peer(&listener);
            assert!(client.runtime.enabled.load(Ordering::SeqCst));
            assert!(peer.begin_control(136, 32768));
            assert!(peer.send(Message::Key {
                vk: 65,
                scan: 30,
                extended: false,
                down: true
            }));
            wait_until(|| !client.input.events.lock().unwrap().is_empty());
            let releases = client.input.releases.load(Ordering::SeqCst);
            if paused {
                client.runtime.reclaim();
            }
            peer.stop();
            wait_until(|| !client.runtime.snapshot().connected);
            assert!(!client.runtime.remote.load(Ordering::SeqCst));
            assert!(!client.runtime.incoming.load(Ordering::SeqCst));
            assert!(client.input.releases.load(Ordering::SeqCst) > releases);
            let next = client.accept_peer(&listener);
            assert_eq!(client.runtime.enabled.load(Ordering::SeqCst), !paused);
            assert_eq!(
                client.input.enables.load(Ordering::SeqCst),
                if paused { 1 } else { 2 }
            );
            if paused {
                assert!(client.runtime.snapshot().detail.contains("共享保持暂停"));
                assert!(next.begin_control(136, 32768));
                wait_until(|| !next.remote.load(Ordering::SeqCst));
                assert!(!client.runtime.incoming.load(Ordering::SeqCst));
                client.runtime.resume().unwrap();
            }
            wait_until(|| next.can_switch());
            assert!(next.begin_control(136, 32768));
            wait_until(|| client.runtime.incoming.load(Ordering::SeqCst));
        }
    }

    #[test]
    fn failed_initial_connect_can_retry_when_preference_is_enabled_and_stop_cancels_wait() {
        let reserved = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = reserved.local_addr().unwrap();
        drop(reserved);
        let mut client = ReconnectingClient::new(address, false);
        wait_until(|| client.runtime.snapshot().detail.contains("连接失败"));
        let listener = TcpListener::bind(address).unwrap();
        listener.set_nonblocking(true).unwrap();
        thread::sleep(Duration::from_millis(1100));
        assert_eq!(listener.accept().unwrap_err().kind(), ErrorKind::WouldBlock);
        client.runtime.auto_reconnect.store(true, Ordering::SeqCst);
        let peer = client.accept_peer(&listener);
        peer.stop();
        wait_until(|| client.runtime.snapshot().reconnecting);
        client.runtime.stop();
        wait_until(|| !client.runtime.running.load(Ordering::SeqCst));
        assert!(!client.runtime.snapshot().reconnecting);
        assert!(!client.runtime.snapshot().listening);
        thread::sleep(Duration::from_millis(1100));
        assert_eq!(listener.accept().unwrap_err().kind(), ErrorKind::WouldBlock);
    }

    #[test]
    fn stop_interrupts_an_incomplete_noise_handshake() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = ReconnectingClient::new(listener.local_addr().unwrap(), true);
        let _silent_peer = accept_socket(&listener);
        wait_until(|| client.runtime.active_socket.lock().unwrap().is_some());
        let stopped_at = Instant::now();
        client.runtime.stop();
        wait_until(|| !client.runtime.running.load(Ordering::SeqCst));
        assert!(stopped_at.elapsed() < Duration::from_secs(2));
        assert_eq!(client.input.enables.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn protocol_failure_stops_outbound_retries_without_enabling_input() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = ReconnectingClient::new(listener.local_addr().unwrap(), true);
        let mut socket = accept_socket(&listener);
        let mut noise = transport::handshake(&mut socket, &[23; 32], false).unwrap();
        let frame = transport::encode(
            &mut noise,
            &Message::Hello {
                version: 1,
                name: "old-peer".into(),
            },
        )
        .unwrap();
        socket.write_all(&frame).unwrap();
        wait_until(|| client.runtime.snapshot().detail.contains("自动重连已停止"));
        thread::sleep(Duration::from_millis(1100));
        assert_eq!(listener.accept().unwrap_err().kind(), ErrorKind::WouldBlock);
        assert_eq!(client.input.enables.load(Ordering::SeqCst), 0);
        assert!(!client.runtime.snapshot().reconnecting);
    }

    #[test]
    fn offline_local_return_is_not_a_queue_failure() {
        let rt = Runtime::new(&Config::default());
        rt.return_local();
        assert!(!rt.abort.load(Ordering::SeqCst));
        assert!(!rt.paused.load(Ordering::SeqCst));
        assert!(!rt.send(Message::Ping));
        assert!(rt.resume().is_err());
        // A control request racing with disconnect must not leave remote=true.
        rt.enabled.store(true, Ordering::SeqCst);
        wait_until(|| rt.can_switch());
        assert!(!rt.begin_control(136, 32768));
        assert!(!rt.remote.load(Ordering::SeqCst));
        assert!(!rt.sharing_enabled());
        assert!(!rt.paused.load(Ordering::SeqCst));
    }

    #[test]
    fn clipboard_failure_keeps_the_input_session_usable() {
        let pair = Loopback::new(false);
        pair.b_input.fail_clipboard.store(true, Ordering::SeqCst);
        assert!(pair.a.send(Message::Clipboard("first".into())));
        wait_until(|| pair.b.snapshot().detail.contains("写入剪贴板失败"));
        assert!(pair.b.snapshot().connected && pair.b.enabled.load(Ordering::SeqCst));
        assert!(pair.a.begin_control(136, 32768));
        assert!(pair.a.send(Message::MoveAbsolute { x: 32768, y: 32768 }));
        wait_until(|| !pair.b_input.events.lock().unwrap().is_empty());
        pair.b_input.fail_clipboard.store(false, Ordering::SeqCst);
        assert!(pair.a.send(Message::Clipboard("retry".into())));
        wait_until(|| !pair.b_input.clipboard.lock().unwrap().is_empty());
        assert_eq!(
            pair.b_input.clipboard.lock().unwrap().as_slice(),
            &["retry"]
        );
    }

    #[test]
    fn corner_guard_blocks_screen_and_relative_returns_until_leaving_corner() {
        for relative in [false, true] {
            let pair = Loopback::new(false);
            pair.a.relative_mouse.store(relative, Ordering::SeqCst);
            pair.b.block_corners.store(true, Ordering::SeqCst);
            assert!(pair.a.begin_control(136, 0));
            wait_until(|| pair.b.incoming.load(Ordering::SeqCst));
            if relative {
                *pair.b_input.return_y.lock().unwrap() = Some(0);
                wait_until(|| pair.b_input.return_y.lock().unwrap().is_none());
            } else {
                assert!(pair.a.send(Message::MoveAbsolute { x: -100, y: 0 }));
                wait_until(|| !pair.b_input.events.lock().unwrap().is_empty());
            }
            assert!(pair.b.incoming.load(Ordering::SeqCst));
            assert!(pair.a.remote.load(Ordering::SeqCst));
            if relative {
                *pair.b_input.return_y.lock().unwrap() = Some(32768);
            } else {
                assert!(pair.a.send(Message::MoveAbsolute { x: -100, y: 32768 }));
            }
            wait_until(|| !pair.a.remote.load(Ordering::SeqCst));
            assert!(!pair.b.incoming.load(Ordering::SeqCst));
            wait_until(|| !pair.a_input.returns.lock().unwrap().is_empty());
            assert_eq!(
                pair.a_input.returns.lock().unwrap().as_slice(),
                &[(true, 32768)]
            );
        }
    }

    #[test]
    fn controller_selects_mode_and_edge_return_restores_height() {
        let pair = Loopback::new(false);
        assert!(!pair.a.relative_mouse.load(Ordering::SeqCst));
        assert!(pair.b.relative_mouse.load(Ordering::SeqCst));
        assert!(pair.a.begin_control(136, 16384));
        wait_until(|| pair.b_input.enters.load(Ordering::SeqCst) == 1);
        assert_eq!(
            pair.b_input.entries.lock().unwrap().as_slice(),
            &[(true, 136, 16384, MouseMode::ScreenRelative)]
        );
        let mut capture = CaptureMotion::enter(
            DesktopBounds::new(0, 0, 1920, 1080),
            true,
            16384,
            MouseMode::ScreenRelative,
        );
        let position = capture.advance(100, 100).unwrap();
        assert!(pair.a.send(position.clone()));
        wait_until(|| !pair.b_input.events.lock().unwrap().is_empty());
        assert_eq!(pair.b_input.events.lock().unwrap().as_slice(), &[position]);
        {
            let positions = pair.b_input.positions.lock().unwrap();
            assert_eq!(positions[1].x - positions[0].x, 200);
            assert_eq!(positions[1].y - positions[0].y, 200);
        }
        assert!(pair.a.send(Message::MoveAbsolute { x: -1, y: 49152 }));
        wait_until(|| !pair.a_input.returns.lock().unwrap().is_empty());
        assert_eq!(
            pair.a_input.returns.lock().unwrap().as_slice(),
            &[(true, 49152)]
        );
        assert!(!pair.a.remote.load(Ordering::SeqCst));
        assert!(!pair.b.incoming.load(Ordering::SeqCst));
        assert!(pair.a.enabled.load(Ordering::SeqCst));
        assert!(!pair.a.can_switch());
        wait_until(|| pair.a.can_switch() && pair.b.can_switch());
        assert!(pair.b.begin_control(68, 32768));
        wait_until(|| pair.a_input.enters.load(Ordering::SeqCst) == 1);
        assert_eq!(
            pair.a_input.entries.lock().unwrap().as_slice(),
            &[(true, 68, 32768, MouseMode::Relative)]
        );
        let relative = Message::Move { dx: 12, dy: -5 };
        assert!(pair.b.send(relative.clone()));
        wait_until(|| !pair.a_input.events.lock().unwrap().is_empty());
        assert_eq!(pair.a_input.events.lock().unwrap().as_slice(), &[relative]);
        *pair.a_input.return_y.lock().unwrap() = Some(16384);
        wait_until(|| !pair.b_input.returns.lock().unwrap().is_empty());
        assert_eq!(
            pair.b_input.returns.lock().unwrap().as_slice(),
            &[(true, 16384)]
        );
    }

    #[test]
    fn loopback_auto_enables_both_peers_routes_mouse_and_cleans_up() {
        let mut pair = Loopback::new(false);
        let (a, b) = (pair.a.clone(), pair.b.clone());
        assert_eq!(a.snapshot().peer_name, "LAPTOP-B");
        assert_eq!(b.snapshot().peer_name, "DESKTOP-A");
        assert!(a.enabled.load(Ordering::SeqCst) && b.enabled.load(Ordering::SeqCst));
        assert_eq!(pair.a_input.enables.load(Ordering::SeqCst), 1);
        assert_eq!(pair.b_input.enables.load(Ordering::SeqCst), 1);
        // Being connected alone must not inject events before control transfer.
        assert!(a.send(Message::Key {
            vk: 65,
            scan: 30,
            extended: false,
            down: true
        }));
        wait_until(|| b.received.load(Ordering::SeqCst) >= 2);
        assert!(!b.incoming.load(Ordering::SeqCst));
        assert!(pair.b_input.events.lock().unwrap().is_empty());
        a.remote.store(true, Ordering::SeqCst);
        assert!(a.send(enter_message(true)));
        let movement = Message::Move { dx: 15, dy: -6 };
        assert!(a.send(movement.clone()));
        wait_until(|| !pair.b_input.events.lock().unwrap().is_empty());
        assert_eq!(pair.b_input.events.lock().unwrap().as_slice(), &[movement]);
        assert!(b.incoming.load(Ordering::SeqCst));

        // Moving the receiver's own mouse returns control without pausing sharing.
        b.return_local();
        wait_until(|| !a.remote.load(Ordering::SeqCst));
        assert!(a.enabled.load(Ordering::SeqCst) && b.enabled.load(Ordering::SeqCst));
        assert!(a.send(enter_message(true)));
        wait_until(|| pair.b_input.enters.load(Ordering::SeqCst) == 2);
        assert!(b.incoming.load(Ordering::SeqCst));
        pair.shutdown();
        assert!(!a.snapshot().connected && !b.snapshot().connected);
        assert!(!a.enabled.load(Ordering::SeqCst) && !b.enabled.load(Ordering::SeqCst));
        assert!(!a.remote.load(Ordering::SeqCst));
        assert!(!b.incoming.load(Ordering::SeqCst));
        assert!(pair.b_input.releases.load(Ordering::SeqCst) > 0);
    }

    #[test]
    fn explicit_pause_is_preserved_and_rejects_remote_input() {
        let pair = Loopback::new(false);
        pair.b.reclaim();
        let before = pair.b.received.load(Ordering::SeqCst);
        assert!(pair.a.send(enter_message(true)));
        assert!(pair.a.send(Message::Move { dx: 10, dy: 1 }));
        wait_until(|| pair.b.received.load(Ordering::SeqCst) >= before + 2);
        assert!(!pair.b.enabled.load(Ordering::SeqCst));
        assert!(!pair.b.incoming.load(Ordering::SeqCst));
        assert_eq!(pair.b_input.enters.load(Ordering::SeqCst), 0);
        assert!(pair.b_input.events.lock().unwrap().is_empty());
        let after = pair.b.received.load(Ordering::SeqCst);
        wait_until(|| pair.b.received.load(Ordering::SeqCst) > after);
        assert!(!pair.b.enabled.load(Ordering::SeqCst));
        assert_eq!(pair.b_input.enables.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn failed_hook_install_is_visible_and_does_not_enable_input() {
        let pair = Loopback::new(true);
        wait_until(|| pair.b.snapshot().detail.contains("自动开启共享失败"));
        assert!(pair.a.enabled.load(Ordering::SeqCst));
        assert!(!pair.b.enabled.load(Ordering::SeqCst));
        assert!(pair.b.snapshot().detail.contains("模拟输入钩子安装失败"));
        let before = pair.b.received.load(Ordering::SeqCst);
        assert!(pair.a.send(enter_message(true)));
        assert!(pair.a.send(Message::Move { dx: 10, dy: 1 }));
        wait_until(|| pair.b.received.load(Ordering::SeqCst) >= before + 2);
        assert_eq!(pair.b_input.enters.load(Ordering::SeqCst), 0);
        assert!(pair.b_input.events.lock().unwrap().is_empty());
    }

    #[test]
    fn injection_failure_pauses_sharing_keeps_connection_and_can_resume() {
        let pair = Loopback::new(false);
        pair.b_input.fail_enter.store(true, Ordering::SeqCst);
        assert!(pair.a.begin_control(136, 16384));
        wait_until(|| {
            !pair.b.enabled.load(Ordering::SeqCst) && pair.b.snapshot().detail.contains("错误码 87")
        });
        wait_until(|| !pair.a.remote.load(Ordering::SeqCst));
        assert!(pair.b.snapshot().detail.contains("已暂停共享，连接保持"));
        assert!(pair.b.snapshot().detail.contains("错误码 87"));
        assert!(pair.a.snapshot().connected && pair.b.snapshot().connected);
        let received = pair.b.received.load(Ordering::SeqCst);
        wait_until(|| pair.b.received.load(Ordering::SeqCst) > received);
        assert!(pair.b.snapshot().connected);
        assert!(pair.b_input.releases.load(Ordering::SeqCst) > 0);

        pair.b_input.fail_enter.store(false, Ordering::SeqCst);
        pair.b.resume().unwrap();
        wait_until(|| pair.a.can_switch());
        assert!(pair.a.begin_control(136, 16384));
        wait_until(|| pair.b.incoming.load(Ordering::SeqCst));
        pair.b_input.fail_apply.store(true, Ordering::SeqCst);
        assert!(pair.a.send(Message::Key {
            vk: 65,
            scan: 30,
            extended: false,
            down: true
        }));
        wait_until(|| {
            !pair.b.enabled.load(Ordering::SeqCst) && pair.b.snapshot().detail.contains("错误码 5")
        });
        wait_until(|| !pair.a.remote.load(Ordering::SeqCst));
        assert!(pair.b.snapshot().detail.contains("错误码 5"));
        assert!(pair.a.snapshot().connected && pair.b.snapshot().connected);

        pair.b_input.fail_apply.store(false, Ordering::SeqCst);
        pair.b.resume().unwrap();
        wait_until(|| pair.a.can_switch());
        assert!(pair.a.begin_control(136, 16384));
        wait_until(|| pair.b.incoming.load(Ordering::SeqCst));
        let movement = Message::MoveAbsolute { x: 20000, y: 16384 };
        assert!(pair.a.send(movement.clone()));
        wait_until(|| !pair.b_input.events.lock().unwrap().is_empty());
        assert_eq!(pair.b_input.events.lock().unwrap().as_slice(), &[movement]);
    }

    #[test]
    fn return_warp_failure_pauses_input_without_dropping_connection() {
        let pair = Loopback::new(false);
        pair.a_input.fail_return.store(true, Ordering::SeqCst);
        assert!(pair.a.begin_control(136, 16384));
        wait_until(|| pair.b.incoming.load(Ordering::SeqCst));
        *pair.b_input.return_y.lock().unwrap() = Some(32768);
        wait_until(|| {
            !pair.a.enabled.load(Ordering::SeqCst)
                && pair.a.snapshot().detail.contains("已暂停共享，连接保持")
        });
        assert!(pair.a.snapshot().detail.contains("已暂停共享，连接保持"));
        assert!(pair.a.snapshot().connected && pair.b.snapshot().connected);
        assert!(!pair.a.remote.load(Ordering::SeqCst));
    }

    #[test]
    fn malformed_mouse_input_still_ends_the_session() {
        let pair = Loopback::new(false);
        assert!(pair.a.begin_control(136, 16384));
        wait_until(|| pair.b.incoming.load(Ordering::SeqCst));
        assert!(pair.a.send(Message::MoveAbsolute {
            x: i32::MIN,
            y: 16384
        }));
        wait_until(|| {
            !pair.b.snapshot().connected && pair.b.snapshot().detail.contains("鼠标比例坐标无效")
        });
        assert!(pair.b.snapshot().detail.contains("鼠标比例坐标无效"));
        assert!(!pair.b.enabled.load(Ordering::SeqCst));
    }

    #[test]
    fn previous_protocol_is_rejected_before_input_is_enabled() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let config = Config::default();
        let runtime = Runtime::new(&config);
        let worker_runtime = runtime.clone();
        let probe = Arc::new(InputProbe::default());
        let mut input = TestInput {
            probe: probe.clone(),
            fail_enable: false,
            motion: PointerMotion::default(),
            bounds: DesktopBounds::new(0, 0, 1920, 1080),
        };
        let worker = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            run_session_with_input(
                &worker_runtime,
                stream,
                &config,
                &[42; 32],
                false,
                &mut input,
            );
        });
        let mut stream = TcpStream::connect(address).unwrap();
        let mut noise = transport::handshake(&mut stream, &[42; 32], true).unwrap();
        let frame = transport::encode(
            &mut noise,
            &Message::Hello {
                version: 1,
                name: "old-version".into(),
            },
        )
        .unwrap();
        stream.write_all(&frame).unwrap();
        worker.join().unwrap();
        assert!(runtime.snapshot().detail.contains("两端协议版本不同"));
        assert!(!runtime.snapshot().connected);
        assert!(!runtime.enabled.load(Ordering::SeqCst));
        assert_eq!(probe.enables.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn port_conflict_does_not_claim_listener_started() {
        let listener = TcpListener::bind("0.0.0.0:0").unwrap();
        let config = Config {
            port: listener.local_addr().unwrap().port(),
            ..Config::default()
        };
        let rt = Runtime::new(&config);
        assert!(start(rt.clone(), config, [0; 32], None).is_err());
        assert!(!rt.running.load(Ordering::SeqCst));
        assert!(!rt.snapshot().listening);
    }
}

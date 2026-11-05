use anyhow::{Context, Result, bail, ensure};
use std::{
    collections::VecDeque,
    io::{ErrorKind, Read, Write},
    net::{Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread,
    time::{Duration, Instant},
};

use crate::{
    config::Config,
    platform::Injector,
    protocol::{MAX_TEXT_BYTES, Message, VERSION},
    transport,
};

#[derive(Clone, Default)]
pub struct Status {
    pub listening: bool,
    pub connected: bool,
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
    pub remote: AtomicBool,
    pub incoming: AtomicBool,
    pub keyboard: AtomicBool,
    pub edge: AtomicBool,
    pub right: AtomicBool,
    pub abort: AtomicBool,
    pub emergency: AtomicBool,
    pub sent: AtomicU64,
    pub received: AtomicU64,
    outgoing: Mutex<Option<SyncSender<Message>>>,
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
            remote: AtomicBool::new(false),
            incoming: AtomicBool::new(false),
            keyboard: AtomicBool::new(config.keyboard),
            edge: AtomicBool::new(config.edge_switch),
            right: AtomicBool::new(config.peer_on_right),
            abort: AtomicBool::new(false),
            emergency: AtomicBool::new(false),
            sent: AtomicU64::new(0),
            received: AtomicU64::new(0),
            outgoing: Mutex::new(None),
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
        if let Some(tx) = guard.as_ref()
            && tx.try_send(msg).is_ok()
        {
            return true;
        }
        self.fail_open();
        false
    }

    fn fail_open(&self) {
        self.remote.store(false, Ordering::SeqCst);
        self.enabled.store(false, Ordering::SeqCst);
        self.abort.store(true, Ordering::SeqCst);
    }

    pub fn return_local(&self) {
        self.remote.store(false, Ordering::SeqCst);
        self.incoming.store(false, Ordering::SeqCst);
        self.emergency.store(true, Ordering::SeqCst);
        self.send(Message::Leave);
    }

    pub fn reclaim(&self) {
        self.enabled.store(false, Ordering::SeqCst);
        self.return_local();
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        self.reclaim();
    }
}

pub fn start(
    runtime: Arc<Runtime>,
    config: Config,
    key: [u8; 32],
    target: Option<String>,
) -> Result<()> {
    ensure!(
        !runtime.running.load(Ordering::SeqCst),
        "连接正在运行，请先停止"
    );
    // Bind synchronously so a port conflict is reported before the UI says it is listening.
    let listener =
        TcpListener::bind(("0.0.0.0", config.port)).context("无法监听端口，可能已被占用")?;
    listener.set_nonblocking(true)?;
    runtime.stop.store(false, Ordering::SeqCst);
    runtime.abort.store(false, Ordering::SeqCst);
    runtime.running.store(true, Ordering::SeqCst);
    runtime.status.lock().unwrap().listening = true;
    runtime.log(format!("正在监听 TCP {}，等待另一台电脑", config.port));
    let rt = runtime.clone();
    thread::spawn(move || {
        if let Some(target) = target {
            match connect(&target, config.port) {
                Ok(stream) => run_session(&rt, stream, &config, &key, true),
                Err(err) => rt.log(format!("连接失败：{err:#}")),
            }
        }
        while !rt.stop.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((stream, _)) => run_session(&rt, stream, &config, &key, false),
                Err(err) if err.kind() == ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(20))
                }
                Err(err) => {
                    rt.log(format!("监听失败：{err}"));
                    break;
                }
            }
        }
        rt.enabled.store(false, Ordering::SeqCst);
        rt.remote.store(false, Ordering::SeqCst);
        rt.incoming.store(false, Ordering::SeqCst);
        rt.status.lock().unwrap().listening = false;
        rt.running.store(false, Ordering::SeqCst);
        rt.log("连接已停止");
    });
    Ok(())
}

fn connect(target: &str, port: u16) -> Result<TcpStream> {
    let addresses: Vec<SocketAddr> = if target.parse::<std::net::IpAddr>().is_ok() {
        (target, port).to_socket_addrs()?.collect()
    } else if target.contains(':') {
        target.to_socket_addrs()?.collect()
    } else {
        (target, port).to_socket_addrs()?.collect()
    };
    for addr in addresses {
        if let Ok(stream) = TcpStream::connect_timeout(&addr, Duration::from_secs(3)) {
            return Ok(stream);
        }
    }
    bail!("无法连接 {target}；请确认对方已启动监听、地址正确且防火墙允许 TCP {port}")
}

fn run_session(
    rt: &Arc<Runtime>,
    stream: TcpStream,
    config: &Config,
    key: &[u8; 32],
    initiator: bool,
) {
    let mut injector = Injector::default();
    run_session_with_input(rt, stream, config, key, initiator, &mut injector);
}

// Keep network/control tests on the production state machine without installing
// global hooks or moving the real desktop pointer in a test process.
trait SessionInput {
    fn enable(&mut self, runtime: Arc<Runtime>) -> Result<()>;
    fn enter(&mut self, from_left: bool);
    fn apply(&mut self, message: Message, keyboard: bool) -> Result<bool>;
    fn release_all(&mut self);
    fn park_cursor(&mut self);
}

impl SessionInput for Injector {
    fn enable(&mut self, runtime: Arc<Runtime>) -> Result<()> {
        crate::platform::install_hooks(runtime)
    }
    fn enter(&mut self, from_left: bool) {
        Injector::enter(self, from_left);
    }
    fn apply(&mut self, message: Message, keyboard: bool) -> Result<bool> {
        Injector::apply(self, message, keyboard)
    }
    fn release_all(&mut self) {
        Injector::release_all(self);
    }
    fn park_cursor(&mut self) {
        crate::platform::park_cursor();
    }
}

fn run_session_with_input(
    rt: &Arc<Runtime>,
    mut stream: TcpStream,
    config: &Config,
    key: &[u8; 32],
    initiator: bool,
    injector: &mut impl SessionInput,
) {
    rt.log("正在验证配对密钥…");
    let result = (|| -> Result<()> {
        let mut noise = transport::handshake(&mut stream, key, initiator)?;
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
    injector.release_all();
    rt.status.lock().unwrap().connected = false;
    let _ = stream.shutdown(Shutdown::Both);
    match result {
        Ok(()) => rt.log("已断开，控制权已回到本机"),
        Err(err) => rt.log(format!("连接结束：{err:#}")),
    }
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
                Ok(0) => bail!("对方已断开"),
                Ok(n) => written += n,
                Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                Err(e) => return Err(e.into()),
            }
        }
        match stream.read(&mut buffer) {
            Ok(0) => bail!("对方已断开"),
            Ok(n) => {
                incoming.extend_from_slice(&buffer[..n]);
                for msg in transport::decode_available(noise, &mut incoming)? {
                    last_receive = Instant::now();
                    rt.received.fetch_add(1, Ordering::Relaxed);
                    if !hello {
                        if let Message::Hello { version, name } = msg {
                            ensure!(version == VERSION, "两端协议版本不同");
                            ensure!(name.len() <= 256, "设备名称过长");
                            let input_result = injector.enable(rt.clone());
                            rt.enabled.store(input_result.is_ok(), Ordering::SeqCst);
                            {
                                let mut s = rt.status.lock().unwrap();
                                s.connected = true;
                                s.peer_name = name.clone();
                                s.address = address.clone();
                            }
                            match input_result {
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
                            rt.remote.store(false, Ordering::SeqCst);
                            rt.incoming.store(false, Ordering::SeqCst);
                            injector.release_all();
                            injector.park_cursor();
                        }
                        Message::Enter { from_left } if rt.enabled.load(Ordering::SeqCst) => {
                            rt.remote.store(false, Ordering::SeqCst);
                            rt.incoming.store(true, Ordering::SeqCst);
                            injector.enter(from_left);
                        }
                        Message::Clipboard(text) if rt.enabled.load(Ordering::SeqCst) => {
                            ensure!(text.len() <= MAX_TEXT_BYTES, "剪贴板文本超过 32 KB");
                            arboard::Clipboard::new()?.set_text(text)?;
                            rt.log("已收到对方发送的剪贴板文本");
                        }
                        Message::Enter { .. } => {
                            rt.send(Message::Leave);
                        }
                        other
                            if rt.enabled.load(Ordering::SeqCst)
                                && rt.incoming.load(Ordering::SeqCst) =>
                        {
                            if !injector.apply(other, rt.keyboard.load(Ordering::SeqCst))? {
                                continue;
                            }
                            injector.release_all();
                            rt.incoming.store(false, Ordering::SeqCst);
                            rt.send(Message::Leave);
                        }
                        _ => {}
                    }
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => {}
            Err(e) => return Err(e.into()),
        }
        ensure!(
            last_receive.elapsed() < Duration::from_secs(5),
            "连接超时，已释放所有按键"
        );
        thread::sleep(Duration::from_millis(2));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct InputProbe {
        enables: AtomicU64,
        enters: AtomicU64,
        releases: AtomicU64,
        events: Mutex<Vec<Message>>,
    }

    struct TestInput {
        probe: Arc<InputProbe>,
        fail_enable: bool,
    }

    impl SessionInput for TestInput {
        fn enable(&mut self, _: Arc<Runtime>) -> Result<()> {
            self.probe.enables.fetch_add(1, Ordering::SeqCst);
            ensure!(!self.fail_enable, "模拟输入钩子安装失败");
            Ok(())
        }
        fn enter(&mut self, _: bool) {
            self.probe.enters.fetch_add(1, Ordering::SeqCst);
        }
        fn apply(&mut self, message: Message, _: bool) -> Result<bool> {
            self.probe.events.lock().unwrap().push(message);
            Ok(false)
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
        assert!(a.send(Message::Enter { from_left: true }));
        let movement = Message::Move { dx: 15, dy: -6 };
        assert!(a.send(movement.clone()));
        wait_until(|| !pair.b_input.events.lock().unwrap().is_empty());
        assert_eq!(pair.b_input.events.lock().unwrap().as_slice(), &[movement]);
        assert!(b.incoming.load(Ordering::SeqCst));

        // Moving the receiver's own mouse returns control without pausing sharing.
        b.return_local();
        wait_until(|| !a.remote.load(Ordering::SeqCst));
        assert!(a.enabled.load(Ordering::SeqCst) && b.enabled.load(Ordering::SeqCst));
        assert!(a.send(Message::Enter { from_left: true }));
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
        assert!(pair.a.send(Message::Enter { from_left: true }));
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
        assert!(pair.a.send(Message::Enter { from_left: true }));
        assert!(pair.a.send(Message::Move { dx: 10, dy: 1 }));
        wait_until(|| pair.b.received.load(Ordering::SeqCst) >= before + 2);
        assert_eq!(pair.b_input.enters.load(Ordering::SeqCst), 0);
        assert!(pair.b_input.events.lock().unwrap().is_empty());
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

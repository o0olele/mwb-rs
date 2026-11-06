//! Independent encrypted links; only the group loop owns input and routing state.
use crate::{
    config::Config,
    engine::Runtime,
    layout::DeviceInfo,
    protocol::{Message, VERSION},
    transport,
};
use anyhow::{Context, Result, bail, ensure};
use std::{
    io::{ErrorKind, Read, Write},
    net::{Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread,
    time::{Duration, Instant},
};
#[derive(Clone)]
pub(crate) struct Link {
    tx: SyncSender<Message>,
    pub closed: Arc<AtomicBool>,
    protocol_error: Arc<AtomicBool>,
}
impl Link {
    pub fn reject(&self) {
        self.protocol_error.store(true, Ordering::SeqCst);
        self.closed.store(true, Ordering::SeqCst);
    }
    pub fn send(&self, msg: Message) -> bool {
        if self.tx.try_send(msg).is_ok() {
            true
        } else {
            self.closed.store(true, Ordering::SeqCst);
            false
        }
    }
}
pub(crate) enum Event {
    Connected {
        connection: u64,
        device: DeviceInfo,
        link: Link,
        address: SocketAddr,
    },
    Message {
        connection: u64,
        message: Message,
    },
    Disconnected {
        connection: u64,
        detail: String,
    },
    Status {
        detail: String,
        retrying: bool,
    },
}
pub(crate) fn launch(
    rt: Arc<Runtime>,
    config: &Config,
    key: [u8; 32],
    target: Option<String>,
    events: SyncSender<Event>,
) -> Result<thread::JoinHandle<()>> {
    let listener = if target.is_none() {
        let listener =
            TcpListener::bind(("0.0.0.0", config.port)).context("无法监听端口，可能已被占用")?;
        listener.set_nonblocking(true)?;
        let mut s = rt.status.lock().unwrap();
        s.listening = true;
        s.address = listener.local_addr()?.to_string();
        Some(listener)
    } else {
        None
    };
    let port = config.port;
    Ok(thread::spawn(move || {
        if let Some(target) = target {
            let mut attempt = 0u32;
            while !rt.stop.load(Ordering::SeqCst) {
                let id = next_id();
                let result = connect(&target, port, &rt.stop)
                    .and_then(|socket| connection(&rt, socket, &key, true, id, &events));
                let retry = result
                    .as_ref()
                    .err()
                    .is_none_or(|e| e.is::<std::io::Error>());
                let detail = match result {
                    Ok(()) => "连接已结束".into(),
                    Err(e) => format!("连接结束：{e:#}"),
                };
                let _ = events.send(Event::Disconnected {
                    connection: id,
                    detail,
                });
                if rt.stop.load(Ordering::SeqCst) {
                    break;
                }
                if !retry {
                    let _ = events.send(Event::Status {
                        detail: "自动重连已停止，请检查密钥、版本或协议错误后重新加入".into(),
                        retrying: false,
                    });
                    break;
                }
                // A successful Hello resets failure backoff.
                if rt.ever_connected.swap(false, Ordering::SeqCst) {
                    attempt = 0;
                }
                let delay = Duration::from_secs((1u64 << attempt.min(5)).min(30));
                attempt = attempt.saturating_add(1);
                let due = Instant::now() + delay;
                let _ = events.send(Event::Status {
                    detail: format!("连接中断，{} 秒后重试；可停止连接取消", delay.as_secs()),
                    retrying: rt.auto_reconnect.load(Ordering::SeqCst),
                });
                while !rt.stop.load(Ordering::SeqCst)
                    && (Instant::now() < due || !rt.auto_reconnect.load(Ordering::SeqCst))
                {
                    thread::sleep(Duration::from_millis(20));
                }
            }
        } else if let Some(listener) = listener {
            let mut workers: Vec<thread::JoinHandle<()>> = Vec::new();
            while !rt.stop.load(Ordering::SeqCst) {
                workers.retain(|w| !w.is_finished());
                match listener.accept() {
                    Ok((socket, _)) if workers.len() < 32 => {
                        let rt = rt.clone();
                        let events = events.clone();
                        let id = next_id();
                        workers.push(thread::spawn(move || {
                            let result = connection(&rt, socket, &key, false, id, &events);
                            let detail = match result {
                                Ok(()) => "设备已断开".into(),
                                Err(e) => format!("设备连接结束：{e:#}"),
                            };
                            let _ = events.send(Event::Disconnected {
                                connection: id,
                                detail,
                            });
                        }));
                    }
                    Ok((socket, _)) => {
                        let _ = socket.shutdown(Shutdown::Both);
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10))
                    }
                    Err(e) => {
                        let _ = events.send(Event::Status {
                            detail: format!("监听失败：{e}"),
                            retrying: false,
                        });
                        break;
                    }
                }
            }
            for worker in workers {
                let _ = worker.join();
            }
        }
    }))
}
fn next_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::SeqCst)
}
fn connect(target: &str, port: u16, stop: &AtomicBool) -> Result<TcpStream> {
    let addresses: Vec<SocketAddr> =
        if target.parse::<std::net::IpAddr>().is_ok() || !target.contains(':') {
            (target, port).to_socket_addrs()?.collect()
        } else {
            target.to_socket_addrs()?.collect()
        };
    let mut error = std::io::Error::new(ErrorKind::AddrNotAvailable, "地址未解析到任何设备");
    for addr in addresses {
        if stop.load(Ordering::SeqCst) {
            bail!("连接已取消");
        }
        match TcpStream::connect_timeout(&addr, Duration::from_secs(3)) {
            Ok(s) => return Ok(s),
            Err(e) => error = e,
        }
    }
    Err(error).context(format!("无法连接 {target}"))
}
fn connection(
    rt: &Runtime,
    mut socket: TcpStream,
    key: &[u8; 32],
    initiator: bool,
    id: u64,
    events: &SyncSender<Event>,
) -> Result<()> {
    let mut noise = transport::handshake_with_cancel(&mut socket, key, initiator, Some(&rt.stop))?;
    socket.set_nonblocking(true)?;
    let (tx, rx) = mpsc::sync_channel(2048);
    let link = Link {
        tx,
        closed: Arc::new(AtomicBool::new(false)),
        protocol_error: Arc::new(AtomicBool::new(false)),
    };
    let mut pending = transport::encode(
        &mut noise,
        &Message::Hello {
            version: VERSION,
            device: rt.local.clone(),
        },
    )?;
    let mut written = 0;
    let mut incoming = Vec::new();
    let mut buffer = [0u8; 8192];
    let mut received = Instant::now();
    let mut ping = Instant::now();
    let mut hello = false;
    let result = (|| -> Result<()> {
        while !rt.stop.load(Ordering::SeqCst) && !link.closed.load(Ordering::SeqCst) {
            if written == pending.len() {
                pending.clear();
                written = 0;
                append_outgoing(&rx, &mut noise, &mut pending, rt)?;
                if ping.elapsed() >= Duration::from_secs(1) {
                    pending.extend(transport::encode(&mut noise, &Message::Ping)?);
                    ping = Instant::now();
                }
            }
            if written < pending.len() {
                match socket.write(&pending[written..]) {
                    Ok(0) => {
                        return Err(
                            std::io::Error::new(ErrorKind::UnexpectedEof, "对方已断开").into()
                        );
                    }
                    Ok(n) => written += n,
                    Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                    Err(e) => return Err(e.into()),
                }
            }
            match socket.read(&mut buffer) {
                Ok(0) => {
                    return Err(std::io::Error::new(ErrorKind::UnexpectedEof, "对方已断开").into());
                }
                Ok(n) => {
                    incoming.extend_from_slice(&buffer[..n]);
                    for message in transport::decode_available(&mut noise, &mut incoming)? {
                        received = Instant::now();
                        rt.received.fetch_add(1, Ordering::Relaxed);
                        if !hello {
                            let Message::Hello { version, device } = message else {
                                bail!("缺少设备握手");
                            };
                            ensure!(
                                version == VERSION,
                                "两端版本不同，需要所有设备均更新到 v0.4.0"
                            );
                            device.validate()?;
                            ensure!(
                                device.id != rt.local.id,
                                "设备标识与本机重复，请勿复制 settings.json 到其他电脑"
                            );
                            events.send(Event::Connected {
                                connection: id,
                                device,
                                link: link.clone(),
                                address: socket.peer_addr()?,
                            })?;
                            hello = true;
                        } else if !matches!(message, Message::Ping) {
                            ensure!(!matches!(message, Message::Hello { .. }), "重复设备握手");
                            events
                                .try_send(Event::Message {
                                    connection: id,
                                    message,
                                })
                                .map_err(|_| anyhow::anyhow!("接收队列拥塞"))?;
                        }
                    }
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                Err(e) => return Err(e.into()),
            }
            if received.elapsed() >= Duration::from_secs(5) {
                return Err(std::io::Error::new(ErrorKind::TimedOut, "连接超时，释放按键").into());
            }
            thread::sleep(Duration::from_millis(2));
        }
        ensure!(
            !link.protocol_error.load(Ordering::SeqCst),
            "连接因协议错误终止"
        );
        Ok(())
    })();
    link.closed.store(true, Ordering::SeqCst);
    let _ = socket.shutdown(Shutdown::Both);
    result
}
fn append_outgoing(
    rx: &Receiver<Message>,
    noise: &mut snow::TransportState,
    pending: &mut Vec<u8>,
    rt: &Runtime,
) -> Result<()> {
    for msg in rx.try_iter().take(64) {
        pending.extend(transport::encode(noise, &msg)?);
        rt.sent.fetch_add(1, Ordering::Relaxed);
    }
    Ok(())
}

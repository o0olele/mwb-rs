//! Bulk traffic has its own sockets, queues and threads; it never enters the input queue.
use super::wire::{self, Event as WireEvent, Packet};
use crate::{engine::Runtime, transport};
use anyhow::{Context, Result, bail, ensure};
use std::{
    collections::HashMap,
    io::{ErrorKind, Read, Write},
    net::{Shutdown, SocketAddr, TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Clone)]
pub struct Ticket {
    pub device: String,
    pub parent: Arc<AtomicBool>,
}
#[derive(Clone)]
pub struct Join {
    pub address: SocketAddr,
    pub ticket: String,
    pub parent: Arc<AtomicBool>,
}
pub type Tickets = Arc<Mutex<HashMap<String, Ticket>>>;
pub type JoinSlot = Arc<Mutex<Option<Join>>>;

#[derive(Clone)]
pub struct Link {
    tx: SyncSender<Packet>,
    pub closed: Arc<AtomicBool>,
}
impl Link {
    pub fn send(&self, packet: &Packet) -> bool {
        !self.closed.load(Ordering::Relaxed) && self.tx.try_send(packet.clone()).is_ok()
    }
}
pub enum Event {
    Connected {
        device: String,
        serial: u64,
        link: Link,
    },
    Packet {
        device: String,
        serial: u64,
        packet: Packet,
    },
    Disconnected {
        device: String,
        serial: u64,
    },
}

pub struct Limiter {
    pub rate: Arc<AtomicU64>,
    bucket: Mutex<(Instant, f64)>,
}
impl Limiter {
    pub fn new(rate: Arc<AtomicU64>) -> Self {
        Self {
            rate,
            bucket: Mutex::new((Instant::now(), 0.0)),
        }
    }
    fn allow(&self, bytes: usize) -> bool {
        let rate = self.rate.load(Ordering::Relaxed);
        let mut bucket = self.bucket.lock().unwrap();
        let now = Instant::now();
        bucket.1 =
            (bucket.1 + now.duration_since(bucket.0).as_secs_f64() * rate as f64).min(65536.0);
        bucket.0 = now;
        if rate == 0 {
            bucket.1 = 0.0;
            return true;
        }
        if bucket.1 < bytes as f64 {
            return false;
        }
        bucket.1 -= bytes as f64;
        true
    }
}

pub fn launch(
    rt: Arc<Runtime>,
    key: [u8; 32],
    listener: Option<TcpListener>,
    events: SyncSender<Event>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let limiter = Arc::new(Limiter::new(rt.exchange.rate.clone()));
        let download = Arc::new(Limiter::new(rt.exchange.rate.clone()));
        if let Some(listener) = listener {
            let mut workers: Vec<thread::JoinHandle<()>> = Vec::new();
            while !rt.stop.load(Ordering::SeqCst) {
                workers.retain(|t| !t.is_finished());
                match listener.accept() {
                    Ok((socket, _)) if workers.len() < 32 => {
                        let rt = rt.clone();
                        let events = events.clone();
                        let limiter = limiter.clone();
                        let download = download.clone();
                        workers.push(thread::spawn(move || {
                            let _ = session(rt, socket, key, None, events, limiter, download);
                        }));
                    }
                    Ok((socket, _)) => {
                        let _ = socket.shutdown(Shutdown::Both);
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10))
                    }
                    Err(e) => {
                        rt.log(format!("文件通道监听失败：{e}"));
                        break;
                    }
                }
            }
            for worker in workers {
                let _ = worker.join();
            }
        } else {
            while !rt.stop.load(Ordering::SeqCst) {
                let join = rt.exchange.join.lock().unwrap().clone();
                if let Some(join) = join.filter(|j| !j.parent.load(Ordering::SeqCst)) {
                    let result = TcpStream::connect_timeout(&join.address, Duration::from_secs(2))
                        .map_err(anyhow::Error::from)
                        .and_then(|socket| {
                            session(
                                rt.clone(),
                                socket,
                                key,
                                Some(join),
                                events.clone(),
                                limiter.clone(),
                                download.clone(),
                            )
                        });
                    if let Err(e) = result
                        && !rt.stop.load(Ordering::SeqCst)
                    {
                        rt.log(format!("文件/剪贴板通道暂不可用，键鼠连接保持：{e:#}"));
                    }
                    for _ in 0..50 {
                        if rt.stop.load(Ordering::SeqCst) {
                            break;
                        }
                        thread::sleep(Duration::from_millis(20));
                    }
                } else {
                    thread::sleep(Duration::from_millis(20));
                }
            }
        }
    })
}

fn publish(
    events: &SyncSender<Event>,
    mut event: Event,
    rt: &Runtime,
    parent: &AtomicBool,
) -> Result<()> {
    loop {
        ensure!(
            !rt.stop.load(Ordering::SeqCst) && !parent.load(Ordering::SeqCst),
            "通道已取消"
        );
        match events.try_send(event) {
            Ok(()) => return Ok(()),
            Err(mpsc::TrySendError::Full(value)) => {
                event = value;
                thread::sleep(Duration::from_millis(2));
            }
            Err(mpsc::TrySendError::Disconnected(_)) => bail!("文件工作线程已结束"),
        }
    }
}
fn session(
    rt: Arc<Runtime>,
    mut socket: TcpStream,
    key: [u8; 32],
    join: Option<Join>,
    events: SyncSender<Event>,
    limiter: Arc<Limiter>,
    download: Arc<Limiter>,
) -> Result<()> {
    let mut noise =
        transport::handshake_with_cancel(&mut socket, &key, join.is_some(), Some(&rt.stop))?;
    let mut incoming = Vec::new();
    let mut buffer = [0; 64 * 1024];
    let (device, parent) = if let Some(join) = join {
        let frame = wire::encode(
            &mut noise,
            &Packet::new(
                "",
                "",
                WireEvent::Join {
                    ticket: join.ticket,
                },
            ),
        )?;
        socket.set_write_timeout(Some(Duration::from_secs(2)))?;
        socket.write_all(&frame)?;
        (String::new(), join.parent)
    } else {
        let deadline = Instant::now() + Duration::from_secs(3);
        let packet = loop {
            ensure!(
                !rt.stop.load(Ordering::SeqCst) && Instant::now() < deadline,
                "文件通道认证超时"
            );
            if let Some(p) = wire::decode(&mut noise, &mut incoming)? {
                break p;
            }
            match socket.read(&mut buffer) {
                Ok(0) => bail!("文件通道已关闭"),
                Ok(n) => incoming.extend_from_slice(&buffer[..n]),
                Err(e)
                    if matches!(
                        e.kind(),
                        ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                    ) => {}
                Err(e) => return Err(e.into()),
            }
        };
        let WireEvent::Join { ticket } = packet.header.event else {
            bail!("文件通道缺少认证票据")
        };
        let ticket = rt
            .exchange
            .tickets
            .lock()
            .unwrap()
            .get(&ticket)
            .cloned()
            .context("文件通道票据无效")?;
        ensure!(!ticket.parent.load(Ordering::SeqCst), "配对连接已结束");
        (ticket.device, ticket.parent)
    };
    socket.set_nonblocking(true)?;
    static SERIAL: AtomicU64 = AtomicU64::new(1);
    let serial = SERIAL.fetch_add(1, Ordering::SeqCst);
    let (tx, rx): (_, Receiver<Packet>) = mpsc::sync_channel(8);
    let link = Link {
        tx,
        closed: Arc::new(AtomicBool::new(false)),
    };
    publish(
        &events,
        Event::Connected {
            device: device.clone(),
            serial,
            link: link.clone(),
        },
        &rt,
        &parent,
    )?;
    let result = (|| -> Result<()> {
        let mut pending = Vec::new();
        let mut offset = 0;
        let mut permitted = false;
        let mut received = Instant::now();
        let mut ping = Instant::now();
        let mut deferred: Option<Packet> = None;
        while !rt.stop.load(Ordering::SeqCst)
            && !parent.load(Ordering::SeqCst)
            && !link.closed.load(Ordering::SeqCst)
        {
            if offset == pending.len() {
                pending.clear();
                offset = 0;
                permitted = false;
                if let Ok(packet) = rx.try_recv() {
                    pending = wire::encode(&mut noise, &packet)?;
                } else if ping.elapsed() > Duration::from_secs(1) {
                    pending = wire::encode(&mut noise, &Packet::new("", "", WireEvent::Ping))?;
                    ping = Instant::now();
                    permitted = true;
                }
            }
            if !pending.is_empty() && (permitted || limiter.allow(pending.len())) {
                permitted = true;
                match socket.write(&pending[offset..]) {
                    Ok(0) => bail!("文件通道已关闭"),
                    Ok(n) => offset += n,
                    Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                    Err(e) => return Err(e.into()),
                }
            }
            if deferred.is_none() {
                match socket.read(&mut buffer) {
                    Ok(0) => bail!("文件通道已关闭"),
                    Ok(n) => incoming.extend_from_slice(&buffer[..n]),
                    Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                    Err(e) => return Err(e.into()),
                }
            }
            loop {
                let packet = if deferred.is_some() {
                    deferred.take()
                } else {
                    wire::decode(&mut noise, &mut incoming)?
                };
                let Some(packet) = packet else {
                    break;
                };
                if !packet.data.is_empty() && !download.allow(packet.data.len()) {
                    deferred = Some(packet);
                    break;
                }
                received = Instant::now();
                if !matches!(packet.header.event, WireEvent::Ping) {
                    ensure!(
                        !matches!(packet.header.event, WireEvent::Join { .. }),
                        "重复文件通道认证"
                    );
                    publish(
                        &events,
                        Event::Packet {
                            device: device.clone(),
                            serial,
                            packet,
                        },
                        &rt,
                        &parent,
                    )?;
                }
            }
            ensure!(received.elapsed() < Duration::from_secs(30), "文件通道超时");
            thread::sleep(Duration::from_millis(1));
        }
        Ok(())
    })();
    link.closed.store(true, Ordering::SeqCst);
    let _ = socket.shutdown(Shutdown::Both);
    // The dispatcher also notices closed flags if shutdown prevents this notification.
    let _ = events.try_send(Event::Disconnected { device, serial });
    result
}

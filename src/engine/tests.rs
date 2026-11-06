use super::*;
use crate::layout::{Rect, test_device};
use std::{
    collections::HashSet,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
};
#[derive(Default)]
struct Probe {
    enables: AtomicU64,
    releases: AtomicU64,
    events: Mutex<Vec<InputEvent>>,
    point: Mutex<Point>,
    held: Mutex<HashSet<u16>>,
    clipboard: Mutex<Vec<String>>,
    fail_enable: AtomicBool,
    fail_enter: AtomicBool,
    fail_apply: AtomicBool,
    fail_clipboard: AtomicBool,
}
struct FakeInput {
    probe: Arc<Probe>,
    bounds: Rect,
    mode: Option<MouseMode>,
}
impl SessionInput for FakeInput {
    fn enable(&mut self, _: Arc<Runtime>) -> Result<()> {
        self.probe.enables.fetch_add(1, Ordering::SeqCst);
        ensure!(
            !self.probe.fail_enable.load(Ordering::SeqCst),
            "模拟钩子安装失败"
        );
        Ok(())
    }
    fn enter(&mut self, p: Point, mode: MouseMode) -> Result<()> {
        if self.probe.fail_enter.load(Ordering::SeqCst) {
            return Err(InputInjectionError::new("鼠标", 0, 87).into());
        }
        self.mode = Some(mode);
        *self.probe.point.lock().unwrap() = p;
        Ok(())
    }
    fn apply(&mut self, e: InputEvent, keyboard: bool) -> Result<()> {
        if self.probe.fail_apply.load(Ordering::SeqCst) {
            return Err(InputInjectionError::new("键盘", 0, 5).into());
        }
        match &e {
            InputEvent::Absolute { x, y } => {
                ensure!(self.mode == Some(MouseMode::ScreenRelative), "模式不符");
                let p = Point {
                    x: *x as f64,
                    y: *y as f64,
                };
                ensure!(self.bounds.contains(p), "坐标无效");
                *self.probe.point.lock().unwrap() = p;
            }
            InputEvent::Relative { dx, dy } => {
                ensure!(self.mode == Some(MouseMode::Relative), "模式不符");
                ensure!(
                    dx.unsigned_abs() <= 32768 && dy.unsigned_abs() <= 32768,
                    "位移无效"
                );
                let mut p = self.probe.point.lock().unwrap();
                *p = self.bounds.clamp(Point {
                    x: p.x + *dx as f64,
                    y: p.y + *dy as f64,
                });
            }
            InputEvent::Key { vk, down, .. } if keyboard => {
                let mut held = self.probe.held.lock().unwrap();
                if *down {
                    held.insert(*vk);
                } else {
                    held.remove(vk);
                }
            }
            _ => {}
        }
        self.probe.events.lock().unwrap().push(e);
        Ok(())
    }
    fn position(&mut self) -> Result<Point> {
        Ok(*self.probe.point.lock().unwrap())
    }
    fn release_all(&mut self) {
        self.probe.releases.fetch_add(1, Ordering::SeqCst);
        self.probe.held.lock().unwrap().clear();
        self.mode = None;
    }
    fn park(&mut self) -> Result<()> {
        *self.probe.point.lock().unwrap() = self.bounds.center();
        Ok(())
    }
    fn clipboard(&mut self, text: String) -> Result<()> {
        ensure!(
            !self.probe.fail_clipboard.load(Ordering::SeqCst),
            "模拟剪贴板占用"
        );
        self.probe.clipboard.lock().unwrap().push(text);
        Ok(())
    }
}
fn wait(mut predicate: impl FnMut() -> bool) {
    let start = Instant::now();
    while !predicate() {
        assert!(start.elapsed() < Duration::from_secs(6), "状态未及时更新");
        thread::sleep(Duration::from_millis(5));
    }
}
struct Cluster {
    nodes: Vec<Arc<Runtime>>,
    probes: Vec<Arc<Probe>>,
    configs: Vec<Config>,
    address: String,
}
impl Cluster {
    fn new(count: usize) -> Self {
        let mut cluster = Self {
            nodes: Vec::new(),
            probes: Vec::new(),
            configs: Vec::new(),
            address: String::new(),
        };
        for i in 0..count {
            let info = test_device(
                (i + 1) as u8,
                if i == 1 { 3840 } else { 1920 },
                if i == 1 { 2160 } else { 1080 },
            );
            let config = Config {
                port: 0,
                device_id: info.id.clone(),
                name: info.name.clone(),
                ..Config::default()
            };
            let rt = Runtime::with_info(&config, info.clone());
            let probe = Arc::new(Probe::default());
            start_with_input(
                rt.clone(),
                config.clone(),
                [23; 32],
                (i > 0).then(|| cluster.address.clone()),
                FakeInput {
                    probe: probe.clone(),
                    bounds: info.bounds,
                    mode: None,
                },
            )
            .unwrap();
            if i == 0 {
                let addr: std::net::SocketAddr = rt.snapshot().address.parse().unwrap();
                cluster.address = format!("127.0.0.1:{}", addr.port());
            }
            cluster.nodes.push(rt);
            cluster.probes.push(probe);
            cluster.configs.push(config);
        }
        if count > 1 {
            wait(|| {
                cluster.nodes.iter().all(|n| {
                    n.snapshot()
                        .layout
                        .devices
                        .iter()
                        .filter(|d| d.online)
                        .count()
                        == count
                        && n.sharing_enabled()
                })
            });
        }
        cluster
    }
    fn control(&self, from: usize, to: usize) -> u64 {
        wait(|| self.nodes[from].can_switch());
        self.nodes[from].control(&self.nodes[to].local.id).unwrap();
        wait(|| {
            self.nodes.iter().all(|n| {
                n.snapshot().control.as_ref().is_some_and(|c| {
                    c.controller == self.nodes[from].local.id && c.target == self.nodes[to].local.id
                })
            })
        });
        self.nodes[from].snapshot().control.unwrap().epoch
    }
    fn key(&self, from: usize, vk: u16) {
        assert!(self.nodes[from].send_input(InputEvent::Key {
            vk,
            scan: 30,
            extended: false,
            down: true
        }));
    }
    fn arrange(&self, from: usize, positions: Vec<Placement>) {
        let rev = self.nodes[from].snapshot().revision;
        self.nodes[from].arrange(rev, positions.clone()).unwrap();
        wait(|| {
            self.nodes
                .iter()
                .all(|n| n.snapshot().layout.placements() == positions)
        });
    }
}
impl Drop for Cluster {
    fn drop(&mut self) {
        for n in &self.nodes {
            n.stop();
        }
        let start = Instant::now();
        while self.nodes.iter().any(|n| n.running.load(Ordering::SeqCst))
            && start.elapsed() < Duration::from_secs(6)
        {
            thread::sleep(Duration::from_millis(10));
        }
    }
}
#[test]
fn three_devices_route_input_between_clients_without_injecting_on_hub() {
    let c = Cluster::new(3);
    let epoch = c.control(1, 2);
    c.key(1, 65);
    wait(|| c.probes[2].held.lock().unwrap().contains(&65));
    assert!(c.probes[0].events.lock().unwrap().is_empty());
    assert!(c.probes[1].events.lock().unwrap().is_empty());
    let next = c.control(1, 0);
    assert!(next > epoch);
    wait(|| c.probes[2].held.lock().unwrap().is_empty());
    c.key(1, 66);
    wait(|| c.probes[0].held.lock().unwrap().contains(&66));
    assert_eq!(c.probes[2].events.lock().unwrap().len(), 1);
}

#[test]
fn queued_input_from_an_old_control_epoch_cannot_land_on_the_new_target() {
    let c = Cluster::new(3);
    let old = c.control(1, 2);
    c.control(1, 0);
    assert!(c.nodes[1].enqueue(Command::Input(
        old,
        InputEvent::Button {
            button: 1,
            down: true
        }
    )));
    assert!(c.nodes[1].enqueue(Command::Motion(old, 100, 100)));
    c.key(1, 66);
    wait(|| c.probes[0].held.lock().unwrap().contains(&66));
    assert_eq!(c.probes[0].events.lock().unwrap().len(), 1);
    assert!(c.probes[2].events.lock().unwrap().is_empty());
}
#[test]
fn layout_edits_from_any_device_are_shared_and_survive_reconnect() {
    let c = Cluster::new(3);
    let mut positions = c.nodes[1].snapshot().layout.placements();
    for p in &mut positions {
        if p.id == c.nodes[1].local.id {
            p.x = 2200;
            p.y = 600;
        }
        if p.id == c.nodes[2].local.id {
            p.x = -2400;
            p.y = -1800;
        }
    }
    c.arrange(1, positions.clone());
    c.nodes[2].enqueue(Command::DropLinks);
    wait(|| !c.nodes[2].snapshot().connected);
    wait(|| c.nodes[2].snapshot().connected && c.nodes[2].sharing_enabled());
    assert_eq!(c.nodes[2].snapshot().layout.placements(), positions);
    assert_eq!(c.nodes[0].snapshot().layout.devices.len(), 3);
}
#[test]
fn stale_layout_revision_is_rejected_and_does_not_overwrite_newer_positions() {
    let c = Cluster::new(3);
    let old = c.nodes[1].snapshot();
    let mut positions = old.layout.placements();
    positions[2].y = 3000;
    c.arrange(0, positions.clone());
    // Send a valid but stale draft, as could happen during a concurrent drag.
    c.nodes[1]
        .arrange(old.revision, old.layout.placements())
        .unwrap();
    wait(|| c.nodes[1].snapshot().detail.contains("布局未保存"));
    assert_eq!(c.nodes[0].snapshot().layout.placements(), positions);
}
#[test]
fn arbitrary_top_and_left_crossings_can_chain_three_devices_in_both_modes() {
    for relative in [false, true] {
        let c = Cluster::new(3);
        let mut positions = c.nodes[0].snapshot().layout.placements();
        for p in &mut positions {
            if p.id == c.nodes[0].local.id {
                p.x = 0;
                p.y = 0;
            }
            if p.id == c.nodes[1].local.id {
                p.x = 2100;
                p.y = 300;
            }
            if p.id == c.nodes[2].local.id {
                p.x = 2200;
                p.y = -1000;
            }
        }
        c.arrange(0, positions);
        c.nodes[0].relative_mouse.store(relative, Ordering::SeqCst);
        wait(|| c.nodes[0].can_switch());
        assert!(
            c.nodes[0].request_control(c.nodes[1].local.id.clone(), Point { x: 500.0, y: 500.0 })
        );
        wait(|| {
            c.nodes[0].remote.load(Ordering::SeqCst) && c.nodes[1].incoming.load(Ordering::SeqCst)
        });
        wait(|| c.nodes[0].can_switch());
        assert!(c.nodes[0].capture(0, -1000));
        wait(|| {
            c.nodes[0]
                .snapshot()
                .control
                .as_ref()
                .is_some_and(|s| s.target == c.nodes[2].local.id)
        });
        wait(|| c.nodes[0].can_switch());
        assert!(c.nodes[0].capture(-1000, 0));
        wait(|| {
            c.nodes[0]
                .snapshot()
                .control
                .as_ref()
                .is_some_and(|s| s.target == c.nodes[0].local.id)
        });
        assert!(!c.nodes[0].remote.load(Ordering::SeqCst));
        assert!(!c.nodes[0].incoming.load(Ordering::SeqCst));
        assert!(c.probes[0].point.lock().unwrap().x > 1900.0);
    }
}
#[test]
fn simultaneous_controllers_converge_to_one_owner_and_one_receiver() {
    let c = Cluster::new(3);
    wait(|| c.nodes[1].can_switch() && c.nodes[2].can_switch());
    c.nodes[1].control(&c.nodes[2].local.id).unwrap();
    c.nodes[2].control(&c.nodes[1].local.id).unwrap();
    wait(|| {
        let states: Vec<_> = c.nodes.iter().map(|n| n.snapshot().control).collect();
        states[0].is_some()
            && states[0] == states[1]
            && states[1] == states[2]
            && c.nodes
                .iter()
                .filter(|n| n.remote.load(Ordering::SeqCst))
                .count()
                == 1
            && c.nodes
                .iter()
                .filter(|n| n.incoming.load(Ordering::SeqCst))
                .count()
                == 1
    });
}
#[test]
fn dropping_unrelated_peer_does_not_interrupt_active_control() {
    let c = Cluster::new(3);
    c.control(1, 0);
    c.nodes[2].stop();
    wait(|| {
        !c.nodes[0]
            .snapshot()
            .layout
            .get(&c.nodes[2].local.id)
            .unwrap()
            .online
    });
    assert!(c.nodes[1].remote.load(Ordering::SeqCst));
    c.key(1, 65);
    wait(|| c.probes[0].held.lock().unwrap().contains(&65));
}
#[test]
fn target_disconnect_releases_control_and_other_peers_keep_working() {
    let c = Cluster::new(3);
    c.control(1, 2);
    c.key(1, 65);
    wait(|| c.probes[2].held.lock().unwrap().contains(&65));
    c.nodes[2].stop();
    wait(|| !c.nodes[1].remote.load(Ordering::SeqCst));
    wait(|| c.probes[2].held.lock().unwrap().is_empty());
    assert!(c.nodes[0].snapshot().connected && c.nodes[1].snapshot().connected);
    wait(|| c.nodes[1].can_switch());
    c.nodes[1].control(&c.nodes[0].local.id).unwrap();
    wait(|| c.nodes[0].incoming.load(Ordering::SeqCst));
}
#[test]
fn reconnect_keeps_paused_intent_and_resumes_normal_sessions() {
    for paused in [false, true] {
        let c = Cluster::new(2);
        if paused {
            c.nodes[1].reclaim();
        }
        c.nodes[1].enqueue(Command::DropLinks);
        wait(|| !c.nodes[1].snapshot().connected);
        wait(|| c.nodes[1].snapshot().connected);
        wait(|| {
            c.nodes[1]
                .snapshot()
                .layout
                .devices
                .iter()
                .all(|d| d.online)
        });
        assert_eq!(c.nodes[1].sharing_enabled(), !paused);
        if paused {
            c.nodes[1].resume().unwrap();
        }
        c.control(0, 1);
        c.key(0, 65);
        wait(|| c.probes[1].held.lock().unwrap().contains(&65));
    }
}
#[test]
fn local_takeover_keeps_sharing_ready_but_pause_rejects_new_control() {
    let c = Cluster::new(3);
    c.control(1, 2);
    c.nodes[2].return_local();
    wait(|| c.nodes[0].snapshot().control.is_none());
    assert!(c.nodes[2].sharing_enabled());
    c.control(1, 2);
    c.nodes[2].reclaim();
    wait(|| c.nodes[0].snapshot().control.is_none());
    wait(|| c.nodes[1].can_switch());
    c.nodes[1].control(&c.nodes[2].local.id).unwrap();
    thread::sleep(Duration::from_millis(200));
    assert!(!c.nodes[2].incoming.load(Ordering::SeqCst));
    assert!(!c.nodes[2].sharing_enabled());
    c.nodes[2].resume().unwrap();
    c.control(1, 2);
}
#[test]
fn injection_failure_pauses_only_target_preserves_links_and_can_resume() {
    let c = Cluster::new(3);
    c.probes[2].fail_enter.store(true, Ordering::SeqCst);
    wait(|| c.nodes[1].can_switch());
    c.nodes[1].control(&c.nodes[2].local.id).unwrap();
    wait(|| c.nodes[2].snapshot().detail.contains("错误码 87"));
    wait(|| !c.nodes[1].remote.load(Ordering::SeqCst));
    assert!(c.nodes.iter().all(|n| n.snapshot().connected));
    assert!(!c.nodes[2].sharing_enabled());
    c.probes[2].fail_enter.store(false, Ordering::SeqCst);
    c.nodes[2].resume().unwrap();
    c.control(1, 2);
    c.probes[2].fail_apply.store(true, Ordering::SeqCst);
    c.key(1, 65);
    wait(|| c.nodes[2].snapshot().detail.contains("错误码 5"));
    assert!(c.nodes[2].snapshot().connected);
}
#[test]
fn targeted_clipboard_failure_keeps_keyboard_links_alive() {
    let c = Cluster::new(3);
    c.probes[2].fail_clipboard.store(true, Ordering::SeqCst);
    c.nodes[1]
        .clipboard(c.nodes[2].local.id.clone(), "中文".into())
        .unwrap();
    wait(|| c.nodes[2].snapshot().detail.contains("剪贴板写入失败"));
    c.control(1, 2);
    c.key(1, 65);
    wait(|| c.probes[2].held.lock().unwrap().contains(&65));
    c.probes[2].fail_clipboard.store(false, Ordering::SeqCst);
    c.nodes[1]
        .clipboard(c.nodes[2].local.id.clone(), "retry".into())
        .unwrap();
    wait(|| !c.probes[2].clipboard.lock().unwrap().is_empty());
    assert!(c.probes[0].clipboard.lock().unwrap().is_empty());
}
#[test]
fn invalid_input_releases_only_the_affected_connection() {
    let c = Cluster::new(3);
    c.control(1, 2);
    c.nodes[1].send_input(InputEvent::Absolute { x: i32::MAX, y: 0 });
    wait(|| !c.nodes[2].snapshot().connected);
    assert!(c.nodes[0].snapshot().connected && c.nodes[1].snapshot().connected);
    assert!(c.probes[2].held.lock().unwrap().is_empty());
}
#[test]
fn disabled_auto_reconnect_waits_until_preference_is_enabled() {
    let c = Cluster::new(2);
    c.nodes[1].auto_reconnect.store(false, Ordering::SeqCst);
    c.nodes[1].enqueue(Command::DropLinks);
    wait(|| !c.nodes[1].snapshot().connected);
    thread::sleep(Duration::from_millis(1200));
    assert!(!c.nodes[1].snapshot().connected);
    c.nodes[1].auto_reconnect.store(true, Ordering::SeqCst);
    wait(|| c.nodes[1].snapshot().connected);
}
#[test]
fn offline_changes_and_full_queue_fail_open() {
    let config = Config::default();
    let rt = Runtime::with_info(&config, test_device(1, 1920, 1080));
    rt.return_local();
    assert!(!rt.paused.load(Ordering::SeqCst));
    let state = rt.snapshot();
    rt.arrange(state.revision, state.layout.placements())
        .unwrap();
    let (tx, _rx) = mpsc::sync_channel(0);
    *rt.commands.lock().unwrap() = Some(tx);
    rt.enabled.store(true, Ordering::SeqCst);
    rt.remote.store(true, Ordering::SeqCst);
    assert!(!rt.send_input(InputEvent::Button {
        button: 1,
        down: true
    }));
    assert!(!rt.remote.load(Ordering::SeqCst));
    assert!(!rt.sharing_enabled());
}
#[test]
fn occupied_port_is_reported_before_host_claims_started() {
    let occupied = TcpListener::bind("0.0.0.0:0").unwrap();
    let config = Config {
        port: occupied.local_addr().unwrap().port(),
        ..Config::default()
    };
    let info = test_device(1, 1920, 1080);
    let rt = Runtime::with_info(&config, info.clone());
    let p = Arc::new(Probe::default());
    assert!(
        start_with_input(
            rt.clone(),
            config,
            [23; 32],
            None,
            FakeInput {
                probe: p,
                bounds: info.bounds,
                mode: None
            }
        )
        .is_err()
    );
    assert!(!rt.running.load(Ordering::SeqCst));
    assert!(!rt.snapshot().listening);
}
#[test]
fn old_protocol_is_rejected_without_affecting_existing_group() {
    let c = Cluster::new(2);
    let mut socket = TcpStream::connect(&c.address).unwrap();
    let mut noise = crate::transport::handshake(&mut socket, &[23; 32], true).unwrap();
    let mut encrypted = [0u8; 1024];
    let length = noise
        .write_message(
            br#"{"Hello":{"version":2,"name":"old-peer"}}"#,
            &mut encrypted,
        )
        .unwrap();
    socket.write_all(&(length as u16).to_be_bytes()).unwrap();
    socket.write_all(&encrypted[..length]).unwrap();
    let mut buf = [0u8; 4096];
    socket
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    loop {
        match socket.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
    }
    wait(|| {
        c.nodes[0]
            .snapshot()
            .events
            .iter()
            .any(|e| e.contains("版本不同"))
    });
    assert_eq!(c.nodes[0].snapshot().layout.devices.len(), 2);
    assert!(c.nodes[1].sharing_enabled());
}
#[test]
fn stopping_client_cancels_pending_handshake() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let info = test_device(1, 1920, 1080);
    let config = Config::default();
    let rt = Runtime::with_info(&config, info.clone());
    let probe = Arc::new(Probe::default());
    start_with_input(
        rt.clone(),
        config,
        [23; 32],
        Some(listener.local_addr().unwrap().to_string()),
        FakeInput {
            probe: probe.clone(),
            bounds: info.bounds,
            mode: None,
        },
    )
    .unwrap();
    let mut socket = None;
    wait(|| match listener.accept() {
        Ok((s, _)) => {
            socket = Some(s);
            true
        }
        Err(_) => false,
    });
    let now = Instant::now();
    rt.stop();
    wait(|| !rt.running.load(Ordering::SeqCst));
    assert!(now.elapsed() < Duration::from_secs(2));
    assert_eq!(probe.enables.load(Ordering::SeqCst), 0);
}

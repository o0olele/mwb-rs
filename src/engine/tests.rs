use super::*;
use crate::exchange::clipboard::Content;
use crate::exchange::paste::RemoteFiles;
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
    clipboard_content: Mutex<Option<Content>>,
    clipboard_sequence: AtomicU64,
    clipboard_writes: AtomicU64,
    clipboard_files: Mutex<Vec<std::path::PathBuf>>,
    remote_files: Mutex<Option<RemoteFiles>>,
    fail_enable: AtomicBool,
    fail_enter: AtomicBool,
    fail_apply: AtomicBool,
    fail_clipboard: AtomicBool,
    fail_service: AtomicBool,
    service_takeover: AtomicBool,
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
        if self.probe.fail_service.load(Ordering::SeqCst) {
            return Err(InputServiceError("模拟锁屏输入服务断开".into()).into());
        }
        if self.probe.service_takeover.swap(false, Ordering::SeqCst) {
            return Err(LocalInputTakeover.into());
        }
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
    fn clipboard_backend(&self) -> Box<dyn Backend> {
        Box::new(FakeClipboard(self.probe.clone()))
    }
}
struct FakeClipboard(Arc<Probe>);
impl Backend for FakeClipboard {
    fn has_files(&mut self) -> bool {
        !self.0.clipboard_files.lock().unwrap().is_empty()
    }
    fn owns_files(&mut self) -> bool {
        self.0.remote_files.lock().unwrap().is_some()
    }
    fn write_files(&mut self, files: RemoteFiles) -> Result<()> {
        *self.0.remote_files.lock().unwrap() = Some(files);
        *self.0.clipboard_content.lock().unwrap() = None;
        self.0.clipboard_files.lock().unwrap().clear();
        self.0.clipboard_sequence.fetch_add(1, Ordering::SeqCst);
        self.0.clipboard_writes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn clear_files(&mut self) -> Result<()> {
        if self.0.remote_files.lock().unwrap().take().is_some() {
            self.0.clipboard_sequence.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }
    fn maintain(&mut self, active: bool) -> Result<()> {
        if !active {
            self.clear_files()?;
        }
        Ok(())
    }
    fn finish(&mut self) {
        let _ = self.clear_files();
    }
    fn files(&mut self) -> Result<Vec<std::path::PathBuf>> {
        Ok(self.0.clipboard_files.lock().unwrap().clone())
    }
    fn sequence(&mut self) -> u64 {
        self.0.clipboard_sequence.load(Ordering::SeqCst)
    }
    fn read(&mut self) -> Result<Option<Content>> {
        Ok(self.0.clipboard_content.lock().unwrap().clone())
    }
    fn write(&mut self, content: &Content) -> Result<()> {
        ensure!(
            !self.0.fail_clipboard.load(Ordering::SeqCst),
            "模拟剪贴板占用"
        );
        if let Ok(text) = String::from_utf8(content.data.as_ref().clone()) {
            self.0.clipboard.lock().unwrap().push(text);
        }
        *self.0.clipboard_content.lock().unwrap() = Some(content.clone());
        self.0.remote_files.lock().unwrap().take();
        self.0.clipboard_files.lock().unwrap().clear();
        self.0.clipboard_sequence.fetch_add(1, Ordering::SeqCst);
        self.0.clipboard_writes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}
#[track_caller]
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
                        && n.exchange.ready()
                })
            });
            wait(|| cluster.nodes[0].exchange.connection_count() == count - 1);
        }
        cluster
    }
    fn control(&self, from: usize, to: usize) -> u64 {
        wait(|| self.nodes[from].can_switch());
        self.nodes[from].control(&self.nodes[to].local.id).unwrap();
        wait(|| {
            self.nodes
                .iter()
                .filter(|n| n.snapshot().connected)
                .all(|n| {
                    n.snapshot().control.as_ref().is_some_and(|c| {
                        c.controller == self.nodes[from].local.id
                            && c.target == self.nodes[to].local.id
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
    fn arrange_row(&self) {
        // Network connection order need not match node creation order.
        let mut positions = self.nodes[0].snapshot().layout.placements();
        for p in &mut positions {
            let index = self.nodes.iter().position(|n| n.local.id == p.id).unwrap();
            p.x = self.nodes[..index]
                .iter()
                .map(|n| n.local.bounds.width + 200)
                .sum();
            p.y = 0;
        }
        self.arrange(0, positions);
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
    assert!(c.nodes[1].enqueue(Command::Motion {
        epoch: old,
        dx: 100,
        dy: 100,
        permit: c.nodes[1].edge_permit(),
    }));
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
fn edge_conditions_gate_local_and_chained_switching_in_both_mouse_modes() {
    use crate::switching::EdgeSwitchCondition;
    for relative in [false, true] {
        for (condition, key, wrong) in [
            (EdgeSwitchCondition::Ctrl, 0xA3, 0xA1),
            (EdgeSwitchCondition::Shift, 0xA0, 0xA2),
        ] {
            let c = Cluster::new(3);
            c.arrange_row();
            let rt = &c.nodes[0];
            rt.relative_mouse.store(relative, Ordering::SeqCst);
            rt.switch_gate.set_condition(condition);
            // Keyboard sharing is independent of the physical switching key.
            rt.keyboard.store(false, Ordering::SeqCst);
            let edge = Point {
                x: 1919.0,
                y: 500.0,
            };
            let right = Point { x: 1.0, y: 0.0 };
            wait(|| rt.can_switch());
            assert!(!rt.edge_at(edge, right));
            rt.switch_gate.observe_key(wrong, true);
            assert!(!rt.edge_at(edge, right));
            rt.switch_gate.observe_key(wrong, false);
            rt.switch_gate.observe_key(key, true);
            rt.edge.store(false, Ordering::SeqCst);
            assert!(!rt.edge_at(edge, right));
            rt.edge.store(true, Ordering::SeqCst);
            assert!(rt.edge_at(edge, right));
            wait(|| c.nodes[1].incoming.load(Ordering::SeqCst));
            rt.switch_gate.observe_key(key, false);
            wait(|| rt.can_switch());
            let before = c.probes[1].events.lock().unwrap().len();
            assert!(rt.capture(32768, 0));
            wait(|| c.probes[1].events.lock().unwrap().len() > before);
            // Relative feedback continues arriving, but cannot authorize a switch.
            thread::sleep(Duration::from_millis(40));
            assert_eq!(rt.snapshot().control.unwrap().target, c.nodes[1].local.id);
            assert!(rt.remote.load(Ordering::SeqCst));
            rt.switch_gate.observe_key(key, true);
            // A press alone must not reuse the preceding denied movement.
            thread::sleep(Duration::from_millis(40));
            assert_eq!(rt.snapshot().control.unwrap().target, c.nodes[1].local.id);
            assert!(rt.capture(1, 0));
            wait(|| c.nodes[2].incoming.load(Ordering::SeqCst));
            wait(|| rt.can_switch());
            assert!(rt.capture(-32768, 0));
            wait(|| c.nodes[1].incoming.load(Ordering::SeqCst));
            wait(|| rt.can_switch());
            assert!(rt.capture(-32768, 0));
            wait(|| !rt.remote.load(Ordering::SeqCst));
            assert_eq!(rt.snapshot().control.unwrap().target, rt.local.id);
            rt.switch_gate.observe_key(key, false);
            // Explicit control and emergency return are not gated by the key.
            c.control(0, 2);
            rt.reclaim();
            wait(|| c.nodes.iter().all(|n| n.snapshot().control.is_none()));
            assert!(!rt.sharing_enabled());
        }
    }
}

#[test]
fn queued_motion_cannot_gain_permission_from_a_later_modifier_press() {
    use crate::switching::EdgeSwitchCondition;
    for relative in [false, true] {
        let c = Cluster::new(3);
        c.arrange_row();
        let rt = &c.nodes[0];
        rt.relative_mouse.store(relative, Ordering::SeqCst);
        rt.switch_gate.set_condition(EdgeSwitchCondition::Ctrl);
        wait(|| rt.can_switch());
        assert!(rt.request_control(c.nodes[1].local.id.clone(), Point { x: 500.0, y: 500.0 }));
        wait(|| c.nodes[1].incoming.load(Ordering::SeqCst));
        wait(|| rt.can_switch());
        let epoch = rt.snapshot().control.unwrap().epoch;
        let denied = rt.edge_permit();
        rt.switch_gate.observe_key(0xA2, true);
        let stale = rt.edge_permit();
        rt.switch_gate.observe_key(0xA2, false);
        rt.switch_gate.observe_key(0xA2, true);
        for (permit, key) in [(denied, 65), (stale, 66)] {
            assert!(rt.enqueue(Command::Motion {
                epoch,
                dx: 32768,
                dy: 0,
                permit
            }));
            c.key(0, key);
            wait(|| c.probes[1].held.lock().unwrap().contains(&key));
            thread::sleep(Duration::from_millis(40));
            assert_eq!(rt.snapshot().control.unwrap().target, c.nodes[1].local.id);
        }
        assert!(rt.capture(1, 0));
        wait(|| c.nodes[2].incoming.load(Ordering::SeqCst));
        wait(|| c.probes[1].held.lock().unwrap().is_empty());
    }
}

#[test]
fn removing_offline_devices_syncs_without_interrupting_control_and_allows_rejoin() {
    let c = Cluster::new(3);
    c.nodes[2].stop();
    wait(|| !c.nodes[2].running.load(Ordering::SeqCst));
    wait(|| {
        c.nodes[..2].iter().all(|n| {
            !n.snapshot()
                .layout
                .get(&c.nodes[2].local.id)
                .unwrap()
                .online
        })
    });
    let epoch = c.control(0, 1);
    c.key(0, 65);
    wait(|| c.probes[1].held.lock().unwrap().contains(&65));
    let state = c.nodes[0].snapshot();
    c.nodes[0]
        .remove_offline(state.revision, &c.nodes[2].local.id)
        .unwrap();
    wait(|| {
        c.nodes[..2]
            .iter()
            .all(|n| n.snapshot().layout.get(&c.nodes[2].local.id).is_none())
    });
    assert_eq!(c.nodes[1].snapshot().control.unwrap().epoch, epoch);
    assert!(c.probes[1].held.lock().unwrap().contains(&65));
    start_with_input(
        c.nodes[2].clone(),
        c.configs[2].clone(),
        [23; 32],
        Some(c.address.clone()),
        FakeInput {
            probe: c.probes[2].clone(),
            bounds: c.nodes[2].local.bounds,
            mode: None,
        },
    )
    .unwrap();
    wait(|| {
        c.nodes.iter().all(|n| {
            n.snapshot()
                .layout
                .get(&c.nodes[2].local.id)
                .is_some_and(|d| d.online)
        })
    });
    for node in &c.nodes {
        assert_eq!(node.snapshot().layout.devices.len(), 3);
        assert!(node.snapshot().connected);
    }
}

#[test]
fn offline_removal_rechecks_revision_and_online_state_on_the_host() {
    let c = Cluster::new(3);
    c.nodes[2].auto_reconnect.store(false, Ordering::SeqCst);
    c.nodes[2].enqueue(Command::DropLinks);
    let id = &c.nodes[2].local.id;
    wait(|| {
        c.nodes[..2]
            .iter()
            .all(|n| !n.snapshot().layout.get(id).unwrap().online)
    });
    let old = c.nodes[0].snapshot();
    assert!(
        c.nodes[0]
            .remove_offline(old.revision, &c.nodes[0].local.id)
            .is_err()
    );
    assert!(
        c.nodes[0]
            .remove_offline(old.revision, &c.nodes[1].local.id)
            .is_err()
    );
    assert!(
        c.nodes[1]
            .remove_offline(c.nodes[1].snapshot().revision, id)
            .is_err()
    );
    c.nodes[0]
        .arrange(old.revision, old.layout.placements())
        .unwrap();
    wait(|| c.nodes[0].snapshot().revision > old.revision);
    // A queued UI command must be checked again after intervening topology edits.
    c.nodes[0].enqueue(Command::RemoveOffline(old.revision, id.clone()));
    wait(|| c.nodes[0].snapshot().detail.contains("设备列表已变化"));
    assert!(c.nodes[0].snapshot().layout.get(id).is_some());
    c.nodes[2].auto_reconnect.store(true, Ordering::SeqCst);
    wait(|| {
        c.nodes
            .iter()
            .all(|n| n.snapshot().layout.get(id).is_some_and(|d| d.online))
    });
    c.nodes[0].enqueue(Command::RemoveOffline(
        c.nodes[0].snapshot().revision,
        id.clone(),
    ));
    wait(|| c.nodes[0].snapshot().detail.contains("设备已在线"));
    assert!(c.nodes.iter().all(|n| n.snapshot().connected));
    assert!(c.nodes[0].snapshot().layout.get(id).unwrap().online);
}

#[test]
fn stopped_profile_removal_survives_settings_roundtrip() {
    let local = test_device(1, 1920, 1080);
    let other = test_device(2, 1920, 1080);
    let mut config = Config {
        device_id: local.id.clone(),
        ..Config::default()
    };
    config.layout.upsert(local.clone(), true).unwrap();
    config.layout.upsert(other.clone(), true).unwrap();
    let rt = Runtime::with_info(&config, local.clone());
    let revision = rt.snapshot().revision;
    assert!(rt.remove_offline(revision + 1, &other.id).is_err());
    assert!(rt.remove_offline(revision, &local.id).is_err());
    assert!(rt.remove_offline(revision, "unknown").is_err());
    rt.remove_offline(revision, &other.id).unwrap();
    config.layout = rt.snapshot().layout;
    let restored = serde_json::from_slice(&serde_json::to_vec(&config).unwrap()).unwrap();
    let reopened = Runtime::with_info(&restored, local);
    assert_eq!(reopened.snapshot().layout.devices.len(), 1);
    assert!(reopened.snapshot().layout.get(&other.id).is_none());
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
fn service_takeover_and_loss_release_input_in_both_mouse_modes_without_dropping_links() {
    for relative in [false, true] {
        let c = Cluster::new(3);
        c.nodes[1].relative_mouse.store(relative, Ordering::SeqCst);
        c.control(1, 2);
        c.key(1, 65);
        wait(|| c.probes[2].held.lock().unwrap().contains(&65));
        c.probes[2].service_takeover.store(true, Ordering::SeqCst);
        wait(|| c.nodes[0].snapshot().control.is_none());
        assert!(c.probes[2].held.lock().unwrap().is_empty());
        assert!(c.nodes.iter().all(|node| node.sharing_enabled()));
        c.control(1, 2);
        c.key(1, 66);
        wait(|| c.probes[2].held.lock().unwrap().contains(&66));
        c.probes[2].fail_service.store(true, Ordering::SeqCst);
        wait(|| c.nodes[0].snapshot().control.is_none());
        assert!(c.nodes.iter().all(|node| node.snapshot().connected));
        assert!(c.probes[2].held.lock().unwrap().is_empty());
        assert!(!c.nodes[2].sharing_enabled());
        assert!(c.nodes[1].sharing_enabled());
        c.probes[2].fail_service.store(false, Ordering::SeqCst);
        c.nodes[2].resume().unwrap();
        c.control(1, 2);
        c.key(1, 67);
        wait(|| c.probes[2].held.lock().unwrap().contains(&67));
    }
}
#[test]
fn targeted_clipboard_failure_keeps_keyboard_links_alive() {
    let c = Cluster::new(3);
    c.probes[2].fail_clipboard.store(true, Ordering::SeqCst);
    c.nodes[1]
        .clipboard(c.nodes[2].local.id.clone(), "中文".into())
        .unwrap();
    wait(|| c.nodes[2].snapshot().detail.contains("模拟剪贴板占用"));
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

fn local_copy(probe: &Probe, content: Content) {
    probe.remote_files.lock().unwrap().take();
    probe.clipboard_files.lock().unwrap().clear();
    *probe.clipboard_content.lock().unwrap() = Some(content);
    probe.clipboard_sequence.fetch_add(1, Ordering::SeqCst);
}

fn local_copy_files(probe: &Probe, paths: Vec<std::path::PathBuf>) {
    probe.remote_files.lock().unwrap().take();
    *probe.clipboard_content.lock().unwrap() = None;
    *probe.clipboard_files.lock().unwrap() = paths;
    probe.clipboard_sequence.fetch_add(1, Ordering::SeqCst);
}
fn remote_files(probe: &Probe) -> RemoteFiles {
    wait(|| probe.remote_files.lock().unwrap().is_some());
    probe.remote_files.lock().unwrap().clone().unwrap()
}

#[test]
fn native_paste_advertises_metadata_then_pulls_verified_files_only_on_demand() {
    use crate::exchange::Phase;
    let c = Cluster::new(3);
    let source = tempfile::tempdir().unwrap();
    let paths = vec![
        source.path().join("中文🙂.bin"),
        source.path().join("empty.txt"),
    ];
    let bytes: Vec<_> = (0..240_000).map(|i| (i % 251) as u8).collect();
    std::fs::write(&paths[0], &bytes).unwrap();
    std::fs::write(&paths[1], []).unwrap();
    local_copy_files(&c.probes[1], paths.clone());
    let remote = remote_files(&c.probes[2]);
    let hub = remote_files(&c.probes[0]);
    assert_eq!(remote.entries()[0].name, "中文🙂.bin");
    assert_eq!(remote.entries()[0].size, bytes.len() as u64);
    thread::sleep(Duration::from_millis(300));
    assert!(
        c.nodes.iter().all(|rt| rt.exchange.snapshot().is_empty()),
        "copy eagerly downloaded file bodies"
    );
    let fetch = remote.fetch(0).unwrap();
    let path = fetch.wait(|| {}).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    let empty = remote.fetch(1).unwrap();
    let empty_path = empty.wait(|| {}).unwrap();
    assert_eq!(std::fs::metadata(empty_path).unwrap().len(), 0);
    assert!(
        Arc::ptr_eq(&fetch, &remote.fetch(0).unwrap()),
        "repeat paste did not reuse verified cache"
    );
    wait(|| {
        c.nodes[1]
            .exchange
            .snapshot()
            .iter()
            .all(|t| t.phase == Phase::Complete)
    });
    assert_eq!(c.nodes[1].exchange.snapshot().len(), 2);
    assert!(
        c.nodes[0].exchange.snapshot().is_empty(),
        "hub materialized relayed file"
    );
    assert!(
        c.nodes[2]
            .exchange
            .snapshot()
            .iter()
            .all(|t| t.phase == Phase::Complete && t.path.is_none())
    );
    assert_eq!(
        c.probes[1].clipboard_writes.load(Ordering::SeqCst),
        0,
        "file clipboard echoed"
    );
    // Releasing clipboard and every stream releases only their generated cache directories.
    local_copy(&c.probes[2], Content::text("new copy".into()));
    drop(hub);
    drop(remote);
    drop(empty);
    drop(fetch);
    wait(|| !path.exists());
    assert_eq!(
        std::fs::read(&paths[0]).unwrap(),
        bytes,
        "paste altered source file"
    );
}

#[test]
fn paste_cancellation_cleans_partial_cache_and_allows_retry_while_input_keeps_working() {
    let c = Cluster::new(3);
    let source = tempfile::tempdir().unwrap();
    let path = source.path().join("cancel-paste.bin");
    let bytes = vec![29; 2 * 1024 * 1024];
    std::fs::write(&path, &bytes).unwrap();
    c.nodes[2].exchange.set_rate_mib(1);
    local_copy_files(&c.probes[1], vec![path]);
    let remote = remote_files(&c.probes[2]);
    let cancelled = remote.fetch(0).unwrap();
    wait(|| {
        c.nodes[2]
            .exchange
            .snapshot()
            .iter()
            .any(|t| t.transferred > 0)
    });
    c.control(1, 2);
    c.key(1, 65);
    wait(|| c.probes[2].held.lock().unwrap().contains(&65));
    remote.cancel();
    assert!(cancelled.wait(|| {}).is_err());
    wait(|| {
        c.nodes[2]
            .exchange
            .snapshot()
            .iter()
            .all(|t| t.phase.terminal())
    });
    assert_eq!(std::fs::read_dir(&cancelled.directory).unwrap().count(), 0);
    let directory = cancelled.directory.clone();
    let retried = remote.fetch(0).unwrap();
    assert_ne!(cancelled.id, retried.id);
    drop(cancelled);
    assert!(!directory.exists());
    assert_eq!(std::fs::read(retried.wait(|| {}).unwrap()).unwrap(), bytes);
    assert!(c.nodes.iter().all(|rt| rt.sharing_enabled()));
}

#[test]
fn changed_sources_and_replaced_clipboards_reject_stale_paste_without_breaking_input() {
    let c = Cluster::new(3);
    let source = tempfile::tempdir().unwrap();
    let path = source.path().join("changed.txt");
    std::fs::write(&path, b"before").unwrap();
    local_copy_files(&c.probes[1], vec![path.clone()]);
    let remote = remote_files(&c.probes[2]);
    std::fs::write(&path, b"changed after copy").unwrap();
    let fetch = remote.fetch(0).unwrap();
    wait(|| fetch.failed());
    assert!(format!("{:#}", fetch.wait(|| {}).unwrap_err()).contains("重新复制"));
    local_copy(&c.probes[1], Content::text("replaced".into()));
    wait(|| clipboard_bytes(&c.probes[2]) == b"replaced");
    assert!(remote.fetch(0).is_err());
    c.control(1, 2);
    c.key(1, 66);
    wait(|| c.probes[2].held.lock().unwrap().contains(&66));
}

#[test]
fn source_disconnect_unblocks_native_paste_and_cleans_the_partial_cache() {
    let c = Cluster::new(3);
    let source = tempfile::tempdir().unwrap();
    let path = source.path().join("disconnect.bin");
    std::fs::write(&path, vec![11; 4 * 1024 * 1024]).unwrap();
    c.nodes[2].exchange.set_rate_mib(1);
    local_copy_files(&c.probes[1], vec![path]);
    let remote = remote_files(&c.probes[2]);
    let fetch = remote.fetch(0).unwrap();
    wait(|| {
        c.nodes[2]
            .exchange
            .snapshot()
            .iter()
            .any(|t| t.transferred > 0)
    });
    c.nodes[1].stop();
    wait(|| fetch.failed());
    assert!(fetch.wait(|| {}).is_err());
    wait(|| std::fs::read_dir(&fetch.directory).unwrap().count() == 0);
    assert!(c.nodes[2].snapshot().connected && c.nodes[2].exchange.ready());
    c.control(2, 0);
    c.key(2, 67);
    wait(|| c.probes[0].held.lock().unwrap().contains(&67));
}

#[test]
fn unsupported_file_copy_revokes_remote_files_and_pause_blocks_new_paste() {
    let c = Cluster::new(2);
    let source = tempfile::tempdir().unwrap();
    let path = source.path().join("source.txt");
    std::fs::write(&path, b"content").unwrap();
    local_copy_files(&c.probes[0], vec![path.clone()]);
    let remote = remote_files(&c.probes[1]);
    c.nodes[1]
        .exchange
        .auto_clipboard
        .store(false, Ordering::SeqCst);
    wait(|| c.probes[1].remote_files.lock().unwrap().is_none());
    assert!(remote.fetch(0).is_err());
    c.nodes[1]
        .exchange
        .auto_clipboard
        .store(true, Ordering::SeqCst);
    local_copy_files(&c.probes[0], vec![path]);
    let remote = remote_files(&c.probes[1]);
    local_copy_files(&c.probes[0], vec![source.path().into()]);
    wait(|| c.probes[1].remote_files.lock().unwrap().is_none());
    assert!(remote.fetch(0).is_err());
    assert!(c.nodes.iter().all(|rt| rt.sharing_enabled()));
}

#[cfg(windows)]
#[test]
fn native_paste_streams_work_through_com_marshalling_and_support_seek_clone_and_eof() {
    use crate::exchange::windows_clipboard::{data_object, pump};
    use windows::{
        Win32::{
            Foundation::{S_FALSE, S_OK},
            System::{
                Com::*,
                DataExchange::RegisterClipboardFormatW,
                Ole::{OleInitialize, OleUninitialize, ReleaseStgMedium},
            },
            UI::Shell::{CFSTR_FILECONTENTS, IDataObjectAsyncCapability},
        },
        core::{AgileReference, Interface},
    };
    // Own COM apartments only; never touch the machine's real clipboard or keyboard hooks.
    struct Apartment;
    impl Apartment {
        fn new() -> Self {
            unsafe {
                OleInitialize(None).unwrap();
            }
            Self
        }
    }
    impl Drop for Apartment {
        fn drop(&mut self) {
            unsafe {
                OleUninitialize();
            }
        }
    }
    let _apartment = Apartment::new();
    let c = Cluster::new(3);
    let source = tempfile::tempdir().unwrap();
    let path = source.path().join("stream.bin");
    let bytes: Vec<_> = (0..130_123).map(|n| (n % 251) as u8).collect();
    std::fs::write(&path, &bytes).unwrap();
    local_copy_files(&c.probes[1], vec![path]);
    let object = data_object(remote_files(&c.probes[2])).unwrap();
    let agile = AgileReference::new(&object).unwrap();
    let consumer = thread::spawn(move || {
        let _apartment = Apartment::new();
        let object = agile.resolve().unwrap();
        let operations: IDataObjectAsyncCapability = object.cast().unwrap();
        unsafe {
            operations.StartOperation(None).unwrap();
            assert!(operations.InOperation().unwrap().as_bool());
            let request = FORMATETC {
                cfFormat: RegisterClipboardFormatW(CFSTR_FILECONTENTS) as u16,
                dwAspect: DVASPECT_CONTENT.0,
                lindex: 0,
                tymed: TYMED_ISTREAM.0 as u32,
                ..Default::default()
            };
            let mut medium = object.GetData(&request).unwrap();
            let stream = medium.u.pstm.as_ref().unwrap().clone();
            ReleaseStgMedium(&mut medium);
            let mut stat = STATSTG::default();
            stream.Stat(&mut stat, STATFLAG_NONAME).unwrap();
            assert_eq!(stat.cbSize, bytes.len() as u64);
            let mut first = [0u8; 17];
            let mut got = 0;
            assert_eq!(
                stream.Read(first.as_mut_ptr().cast(), 17, Some(&mut got)),
                S_OK
            );
            assert_eq!(got, 17);
            assert_eq!(first, bytes[..17]);
            let clone = stream.Clone().unwrap();
            stream.Seek(-5, STREAM_SEEK_END, None).unwrap();
            let mut tail = [0u8; 10];
            assert_eq!(
                stream.Read(tail.as_mut_ptr().cast(), 10, Some(&mut got)),
                S_FALSE
            );
            assert_eq!(got, 5);
            assert_eq!(tail[..5], bytes[bytes.len() - 5..]);
            assert_eq!(
                stream.Read(tail.as_mut_ptr().cast(), 10, Some(&mut got)),
                S_FALSE
            );
            assert_eq!(got, 0);
            let mut remaining = vec![0; bytes.len() - 17];
            assert_eq!(
                clone.Read(
                    remaining.as_mut_ptr().cast(),
                    remaining.len() as u32,
                    Some(&mut got)
                ),
                S_OK
            );
            assert_eq!(remaining, bytes[17..]);
            assert!(clone.Seek(-1, STREAM_SEEK_SET, None).is_err());
            assert!(clone.Write(first.as_ptr().cast(), 17, None).is_err());
            operations.EndOperation(S_OK, None, 1).unwrap();
            assert!(!operations.InOperation().unwrap().as_bool());
        }
    });
    let started = Instant::now();
    while !consumer.is_finished() {
        pump();
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "COM paste consumer timed out"
        );
        thread::sleep(Duration::from_millis(2));
    }
    consumer.join().unwrap();
}

#[cfg(windows)]
#[test]
#[ignore = "requires Windows Shell; copies only test data without touching the system clipboard"]
fn windows_shell_pastes_virtual_files_into_a_folder() {
    use crate::exchange::windows_clipboard::{data_object, pump};
    use std::os::windows::ffi::OsStrExt;
    use windows::{
        Win32::{
            Foundation::POINTL,
            System::{
                Ole::{DROPEFFECT_COPY, IDropTarget, OleInitialize, OleUninitialize},
                SystemServices::{MK_CONTROL, MK_LBUTTON},
            },
            UI::Shell::{
                BHID_SFUIObject, IDataObjectAsyncCapability, IShellItem,
                SHCreateItemFromParsingName,
            },
        },
        core::{Interface, PCWSTR},
    };
    struct Apartment;
    impl Drop for Apartment {
        fn drop(&mut self) {
            unsafe {
                OleUninitialize();
            }
        }
    }
    unsafe {
        OleInitialize(None).unwrap();
    }
    let _apartment = Apartment;
    let c = Cluster::new(3);
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    let paths = vec![
        source.path().join("粘贴🙂.bin"),
        source.path().join("empty.txt"),
    ];
    let bytes: Vec<_> = (0..130_123).map(|n| (n % 251) as u8).collect();
    std::fs::write(&paths[0], &bytes).unwrap();
    std::fs::write(&paths[1], []).unwrap();
    local_copy_files(&c.probes[1], paths.clone());
    let object = data_object(remote_files(&c.probes[2])).unwrap();
    unsafe {
        let operations: IDataObjectAsyncCapability = object.cast().unwrap();
        assert!(operations.GetAsyncMode().unwrap().as_bool());
        let directory: Vec<_> = destination
            .path()
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect();
        let folder: IShellItem =
            SHCreateItemFromParsingName(PCWSTR(directory.as_ptr()), None).unwrap();
        let target: IDropTarget = folder.BindToHandler(None, &BHID_SFUIObject).unwrap();
        let mut effect = DROPEFFECT_COPY;
        target
            .DragEnter(
                &object,
                MK_CONTROL | MK_LBUTTON,
                POINTL::default(),
                &mut effect,
            )
            .unwrap();
        assert_eq!(effect, DROPEFFECT_COPY);
        target
            .Drop(&object, MK_CONTROL, POINTL::default(), &mut effect)
            .unwrap();
        assert_eq!(effect, DROPEFFECT_COPY);
        let started = Instant::now();
        while operations.InOperation().unwrap().as_bool()
            || !std::fs::metadata(destination.path().join("粘贴🙂.bin"))
                .is_ok_and(|m| m.len() == bytes.len() as u64)
            || !destination.path().join("empty.txt").exists()
        {
            pump();
            assert!(
                started.elapsed() < Duration::from_secs(15),
                "asynchronous Shell paste timed out"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }
    let files: Vec<_> = std::fs::read_dir(destination.path())
        .unwrap()
        .map(|f| f.unwrap().path())
        .collect();
    assert_eq!(
        files.len(),
        2,
        "Shell did not materialize both virtual files"
    );
    assert_eq!(
        std::fs::read(destination.path().join("粘贴🙂.bin")).unwrap(),
        bytes
    );
    assert_eq!(
        std::fs::metadata(destination.path().join("empty.txt"))
            .unwrap()
            .len(),
        0
    );
    assert!(files.iter().any(|p| std::fs::read(p).unwrap() == bytes));
    assert_eq!(std::fs::read(&paths[0]).unwrap(), bytes);
    eprintln!("Windows Shell copied two virtual files through a three-device relay");
}

#[cfg(windows)]
#[test]
#[ignore = "writes and relays 128 MiB through native IStream; run explicitly for release validation"]
fn native_clipboard_paste_larger_than_original_100mb_limit_streams_end_to_end() {
    use crate::exchange::windows_clipboard::data_object;
    use sha2::{Digest, Sha256};
    use windows::Win32::{
        System::{Com::*, DataExchange::RegisterClipboardFormatW, Ole::ReleaseStgMedium},
        UI::Shell::CFSTR_FILECONTENTS,
    };
    let c = Cluster::new(3);
    let source = tempfile::tempdir().unwrap();
    let path = source.path().join("128MiB-paste.bin");
    let block: Vec<_> = (0..1024 * 1024).map(|i| (i % 251) as u8).collect();
    let mut file = std::fs::File::create(&path).unwrap();
    let mut expected = Sha256::new();
    for _ in 0..128 {
        file.write_all(&block).unwrap();
        expected.update(&block);
    }
    drop(file);
    local_copy_files(&c.probes[1], vec![path]);
    let remote = remote_files(&c.probes[2]);
    assert_eq!(remote.entries()[0].size, 128 * 1024 * 1024);
    assert!(c.nodes[1].exchange.snapshot().is_empty());
    let object = data_object(remote).unwrap();
    let mut actual = Sha256::new();
    let mut total = 0;
    let start = Instant::now();
    unsafe {
        let request = FORMATETC {
            cfFormat: RegisterClipboardFormatW(CFSTR_FILECONTENTS) as u16,
            dwAspect: DVASPECT_CONTENT.0,
            lindex: 0,
            tymed: TYMED_ISTREAM.0 as u32,
            ..Default::default()
        };
        let mut medium = object.GetData(&request).unwrap();
        let stream = medium.u.pstm.as_ref().unwrap().clone();
        ReleaseStgMedium(&mut medium);
        let mut buffer = [0u8; 32768];
        loop {
            let mut got = 0;
            stream
                .Read(
                    buffer.as_mut_ptr().cast(),
                    buffer.len() as u32,
                    Some(&mut got),
                )
                .ok()
                .unwrap();
            if got == 0 {
                break;
            }
            total += u64::from(got);
            actual.update(&buffer[..got as usize]);
        }
    }
    assert_eq!(total, 128 * 1024 * 1024);
    assert_eq!(actual.finalize(), expected.finalize());
    eprintln!(
        "128 MiB native clipboard IStream, three-device relay and SHA-256 verified: {:?}",
        start.elapsed()
    );
}
fn clipboard_bytes(probe: &Probe) -> Vec<u8> {
    probe
        .clipboard_content
        .lock()
        .unwrap()
        .as_ref()
        .map(|c| c.data.as_ref().clone())
        .unwrap_or_default()
}

#[test]
fn automatic_text_and_images_reach_all_devices_without_echo() {
    let c = Cluster::new(3);
    let text = "中文自动剪贴板\n".repeat(10_000);
    local_copy(&c.probes[1], Content::text(text.clone()));
    wait(|| {
        clipboard_bytes(&c.probes[0]) == text.as_bytes()
            && clipboard_bytes(&c.probes[2]) == text.as_bytes()
    });
    thread::sleep(Duration::from_millis(450));
    assert_eq!(c.probes[1].clipboard_writes.load(Ordering::SeqCst), 0);
    assert_eq!(c.probes[0].clipboard_writes.load(Ordering::SeqCst), 1);
    assert_eq!(c.probes[2].clipboard_writes.load(Ordering::SeqCst), 1);
    let rgba: Vec<_> = (0..64 * 48 * 4).map(|i| (i % 251) as u8).collect();
    local_copy(&c.probes[2], Content::image(64, 48, rgba.clone()));
    wait(|| clipboard_bytes(&c.probes[0]) == rgba && clipboard_bytes(&c.probes[1]) == rgba);
    thread::sleep(Duration::from_millis(350));
    assert_eq!(c.probes[2].clipboard_writes.load(Ordering::SeqCst), 1);
    assert_eq!(c.probes[0].clipboard_writes.load(Ordering::SeqCst), 2);
}

#[test]
fn clipboard_switch_and_pause_stop_automatic_sync_but_manual_send_is_available() {
    let c = Cluster::new(3);
    c.nodes[2]
        .exchange
        .auto_clipboard
        .store(false, Ordering::SeqCst);
    local_copy(&c.probes[1], Content::text("automatic".into()));
    wait(|| clipboard_bytes(&c.probes[0]) == b"automatic");
    thread::sleep(Duration::from_millis(250));
    assert!(clipboard_bytes(&c.probes[2]).is_empty());
    c.nodes[1]
        .clipboard(c.nodes[2].local.id.clone(), "manual".into())
        .unwrap();
    wait(|| clipboard_bytes(&c.probes[2]) == b"manual");
    c.nodes[1].reclaim();
    thread::sleep(Duration::from_millis(200));
    local_copy(&c.probes[1], Content::text("paused".into()));
    thread::sleep(Duration::from_millis(350));
    assert_eq!(clipboard_bytes(&c.probes[0]), b"automatic");
    c.nodes[1].resume().unwrap();
    local_copy(&c.probes[1], Content::text("resumed".into()));
    wait(|| clipboard_bytes(&c.probes[0]) == b"resumed");
}

#[test]
fn a_new_local_copy_wins_over_an_older_clipboard_still_in_flight() {
    let c = Cluster::new(3);
    c.nodes[1].exchange.set_rate_mib(1);
    local_copy(&c.probes[1], Content::text("old".repeat(500_000)));
    wait(|| c.nodes[2].exchange.clipboard_clock.load(Ordering::SeqCst) > 0);
    local_copy(&c.probes[2], Content::text("new local copy".into()));
    wait(|| {
        clipboard_bytes(&c.probes[0]) == b"new local copy"
            && clipboard_bytes(&c.probes[1]) == b"new local copy"
    });
    thread::sleep(Duration::from_secs(2));
    assert_eq!(clipboard_bytes(&c.probes[2]), b"new local copy");
}

#[test]
fn rejoining_device_receives_the_clipboard_clock_before_publishing() {
    let c = Cluster::new(3);
    c.nodes[0]
        .exchange
        .clipboard_clock
        .store(500, Ordering::SeqCst);
    local_copy(&c.probes[0], Content::text("earlier".into()));
    wait(|| clipboard_bytes(&c.probes[2]) == b"earlier");
    c.nodes[2].stop();
    wait(|| !c.nodes[2].running.load(Ordering::SeqCst));
    c.nodes[2]
        .exchange
        .clipboard_clock
        .store(0, Ordering::SeqCst);
    *c.nodes[2].exchange.clipboard_version.lock().unwrap() = Default::default();
    start_with_input(
        c.nodes[2].clone(),
        c.configs[2].clone(),
        [23; 32],
        Some(c.address.clone()),
        FakeInput {
            probe: c.probes[2].clone(),
            bounds: c.nodes[2].local.bounds,
            mode: None,
        },
    )
    .unwrap();
    wait(|| c.nodes[2].exchange.ready() && c.nodes[2].sharing_enabled());
    assert!(c.nodes[2].exchange.clipboard_clock.load(Ordering::SeqCst) >= 501);
    local_copy(&c.probes[2], Content::text("after restart".into()));
    wait(|| {
        clipboard_bytes(&c.probes[0]) == b"after restart"
            && clipboard_bytes(&c.probes[1]) == b"after restart"
    });
}

#[test]
fn files_from_clipboard_require_acceptance_and_preserve_duplicate_names() {
    use crate::exchange::Phase;
    let c = Cluster::new(3);
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    let path = source.path().join("文档.bin");
    let content: Vec<_> = (0..150_000).map(|i| (i % 251) as u8).collect();
    std::fs::write(&path, &content).unwrap();
    std::fs::write(destination.path().join("文档.bin"), b"existing").unwrap();
    *c.probes[1].clipboard_files.lock().unwrap() = vec![path];
    c.nodes[1]
        .exchange
        .send_clipboard_files(c.nodes[2].local.id.clone())
        .unwrap();
    wait(|| !c.nodes[2].exchange.snapshot().is_empty());
    let offer = c.nodes[2].exchange.snapshot()[0].clone();
    assert_eq!(offer.phase, Phase::Offered);
    assert_eq!(std::fs::read_dir(destination.path()).unwrap().count(), 1);
    c.nodes[2]
        .exchange
        .accept(offer.id.clone(), destination.path().into())
        .unwrap();
    wait(|| {
        c.nodes[1]
            .exchange
            .snapshot()
            .iter()
            .any(|t| t.id == offer.id && t.phase == Phase::Complete)
    });
    let received = c.nodes[2].exchange.snapshot()[0].path.clone().unwrap();
    assert_eq!(std::fs::read(received).unwrap(), content);
    assert_eq!(
        std::fs::read(destination.path().join("文档.bin")).unwrap(),
        b"existing"
    );
    assert!(
        c.nodes[0].exchange.snapshot().is_empty(),
        "hub only relays the file"
    );
}

#[test]
fn throttled_file_stream_keeps_mouse_and_keyboard_dispatch_responsive() {
    use crate::exchange::Phase;
    let c = Cluster::new(3);
    // The receiver's setting also limits incoming traffic, even if senders are faster.
    c.nodes[2].exchange.set_rate_mib(1);
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    let path = source.path().join("stream.bin");
    std::fs::write(&path, vec![31; 2 * 1024 * 1024]).unwrap();
    c.nodes[1]
        .exchange
        .send_files(c.nodes[2].local.id.clone(), vec![path])
        .unwrap();
    wait(|| !c.nodes[2].exchange.snapshot().is_empty());
    let offer = c.nodes[2].exchange.snapshot()[0].clone();
    let start = Instant::now();
    c.nodes[2]
        .exchange
        .accept(offer.id.clone(), destination.path().into())
        .unwrap();
    wait(|| c.nodes[2].exchange.snapshot()[0].transferred > 0);
    c.control(1, 2);
    let mut worst = Duration::ZERO;
    for i in 0..30 {
        let mark = Instant::now();
        let old_events = c.probes[2].events.lock().unwrap().len();
        assert!(c.nodes[1].capture(1, 1));
        assert!(c.nodes[1].send_input(InputEvent::Key {
            vk: 65,
            scan: 30,
            extended: false,
            down: i % 2 == 0
        }));
        wait(|| c.probes[2].events.lock().unwrap().len() >= old_events + 2);
        worst = worst.max(mark.elapsed());
    }
    assert!(worst < Duration::from_secs(1), "input stalled: {worst:?}");
    wait(|| {
        let transfer = c.nodes[1].exchange.snapshot()[0].clone();
        assert!(
            !transfer.phase.terminal() || transfer.phase == Phase::Complete,
            "{transfer:?}"
        );
        transfer.phase == Phase::Complete
    });
    assert!(
        start.elapsed() >= Duration::from_millis(1700),
        "configured aggregate rate was not enforced"
    );
    assert!(
        c.nodes
            .iter()
            .all(|n| n.sharing_enabled() && n.snapshot().connected)
    );
    eprintln!(
        "2 MiB limited relay finished in {:?}; worst input dispatch {:?}",
        start.elapsed(),
        worst
    );
}

#[test]
fn rejected_and_interrupted_files_clean_up_without_breaking_other_peers() {
    use crate::exchange::Phase;
    let c = Cluster::new(3);
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    let path = source.path().join("cancel.bin");
    std::fs::write(&path, vec![1; 4 * 1024 * 1024]).unwrap();
    c.nodes[1].exchange.set_rate_mib(1);
    c.nodes[1]
        .exchange
        .send_files(c.nodes[2].local.id.clone(), vec![path.clone()])
        .unwrap();
    wait(|| !c.nodes[2].exchange.snapshot().is_empty());
    let id = c.nodes[2].exchange.snapshot()[0].id.clone();
    c.nodes[2].exchange.cancel(id.clone()).unwrap();
    wait(|| c.nodes[1].exchange.snapshot()[0].phase == Phase::Cancelled);
    assert_eq!(std::fs::read_dir(destination.path()).unwrap().count(), 0);
    c.nodes[1]
        .exchange
        .send_files(c.nodes[2].local.id.clone(), vec![path])
        .unwrap();
    wait(|| c.nodes[2].exchange.snapshot()[0].phase == Phase::Offered);
    let id = c.nodes[2].exchange.snapshot()[0].id.clone();
    c.nodes[2]
        .exchange
        .accept(id, destination.path().into())
        .unwrap();
    wait(|| c.nodes[2].exchange.snapshot()[0].transferred > 0);
    c.nodes[2].stop();
    wait(|| !c.nodes[2].running.load(Ordering::SeqCst));
    assert_eq!(std::fs::read_dir(destination.path()).unwrap().count(), 0);
    c.control(1, 0);
    c.key(1, 66);
    wait(|| c.probes[0].held.lock().unwrap().contains(&66));
}

#[test]
fn empty_and_multiple_files_complete_independently() {
    use crate::exchange::Phase;
    let c = Cluster::new(2);
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    let paths: Vec<_> = ["empty.txt", "second.txt"]
        .into_iter()
        .map(|name| source.path().join(name))
        .collect();
    std::fs::write(&paths[0], []).unwrap();
    std::fs::write(&paths[1], b"second").unwrap();
    c.nodes[0]
        .exchange
        .send_files(c.nodes[1].local.id.clone(), paths)
        .unwrap();
    wait(|| c.nodes[1].exchange.snapshot().len() == 2);
    for offer in c.nodes[1].exchange.snapshot() {
        c.nodes[1]
            .exchange
            .accept(offer.id, destination.path().into())
            .unwrap();
    }
    wait(|| {
        c.nodes[0]
            .exchange
            .snapshot()
            .iter()
            .all(|t| t.phase == Phase::Complete)
    });
    assert_eq!(
        std::fs::read(destination.path().join("empty.txt")).unwrap(),
        b""
    );
    assert_eq!(
        std::fs::read(destination.path().join("second.txt")).unwrap(),
        b"second"
    );
}

#[test]
#[ignore = "writes and relays 128 MiB; run explicitly for release validation"]
fn file_larger_than_original_100mb_limit_streams_end_to_end() {
    use crate::exchange::Phase;
    use sha2::{Digest, Sha256};
    let c = Cluster::new(3);
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    let path = source.path().join("128MiB.bin");
    let block: Vec<_> = (0..1024 * 1024).map(|i| (i % 251) as u8).collect();
    let mut file = std::fs::File::create(&path).unwrap();
    let mut expected = Sha256::new();
    for _ in 0..128 {
        file.write_all(&block).unwrap();
        expected.update(&block);
    }
    drop(file);
    c.nodes[1]
        .exchange
        .send_files(c.nodes[2].local.id.clone(), vec![path])
        .unwrap();
    wait(|| !c.nodes[2].exchange.snapshot().is_empty());
    let offer = c.nodes[2].exchange.snapshot()[0].clone();
    assert_eq!(offer.size, 128 * 1024 * 1024);
    c.nodes[2]
        .exchange
        .accept(offer.id.clone(), destination.path().into())
        .unwrap();
    let start = Instant::now();
    loop {
        let state = c.nodes[1].exchange.snapshot()[0].clone();
        if state.phase == Phase::Complete {
            break;
        }
        assert!(!state.phase.terminal(), "transfer failed: {state:?}");
        assert!(
            start.elapsed() < Duration::from_secs(180),
            "large file timed out"
        );
        thread::sleep(Duration::from_millis(50));
    }
    let mut file = std::fs::File::open(destination.path().join("128MiB.bin")).unwrap();
    let mut actual = Sha256::new();
    let mut buffer = [0u8; 32768];
    loop {
        let n = file.read(&mut buffer).unwrap();
        if n == 0 {
            break;
        }
        actual.update(&buffer[..n]);
    }
    assert_eq!(actual.finalize(), expected.finalize());
    eprintln!(
        "128 MiB SHA-256-verified client-to-client relay: {:?}",
        start.elapsed()
    );
}

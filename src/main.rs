#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use gpui_kit::component::{
    button::*,
    input::{Input, InputState},
    *,
};
use gpui_kit::{prelude::FluentBuilder, *};
use mwb::{
    config::{
        Config, new_pairing_key, new_pairing_key_with_length, parse_pairing_key,
        validate_pairing_key,
    },
    engine::{self, Runtime},
    exchange::{Phase, format_size},
    instance,
    layout::{Layout, Point as LayoutPoint},
    pairing::SavedConnection,
    platform, startup,
    switching::EdgeSwitchCondition,
};
use std::{
    sync::{Arc, Mutex, atomic::Ordering},
    time::Duration,
};

mod ui;
use ui::{Panel, SetupStep};

const INK: u32 = 0x202535;
const MUTED: u32 = 0x737C90;
const GREEN: u32 = 0x218563;
const BORDER: u32 = 0xE8EAF1;
const BACKGROUND: u32 = 0xF7F8FB;
const ACCENT: u32 = 0x5867E8;
const ACCENT_SOFT: u32 = 0xEFF0FD;
const WARNING: u32 = 0x9A6418;

struct CanvasDrag {
    id: String,
    start: Point<Pixels>,
    x: i32,
    y: i32,
    scale: f64,
    moved: bool,
}

struct Workspace {
    config: Config,
    runtime: Arc<Runtime>,
    secret: Entity<InputState>,
    key_length: Entity<InputState>,
    address: Entity<InputState>,
    port: Entity<InputState>,
    configured: bool,
    setup_step: SetupStep,
    panel: Option<Panel>,
    show_advanced: bool,
    show_advanced_preferences: bool,
    awaiting_connection: bool,
    local_address: String,
    focus: FocusHandle,
    layout_draft: Layout,
    layout_revision: u64,
    layout_dirty: bool,
    selected: String,
    drag: Option<CanvasDrag>,
    pan_drag: Option<(Point<Pixels>, LayoutPoint)>,
    canvas_bounds: Arc<Mutex<Bounds<Pixels>>>,
    view_scale: f64,
    view_offset: LayoutPoint,
    fit_pending: bool,
    fitted_size: gpui_kit::Size<Pixels>,
    notice: String,
    input_service_notice: String,
    startup_status: Option<startup::Status>,
    startup_notice: String,
    show_key: bool,
    preparing: bool,
    start_generation: u64,
    pending_connection: Option<SavedConnection>,
    _refresh: Task<()>,
}

impl Workspace {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (mut config, mut notice) = match Config::load() {
            Ok(config) => (config, String::new()),
            Err(err) => (Config::default(), format!("{err:#}")),
        };
        if let Err(err) = config.save() {
            notice = format!("保存设备标识失败：{err:#}");
        }
        let configured = config.saved_connection.is_some();
        let setup_step = match &config.saved_connection {
            Some(saved) if saved.target.is_some() => SetupStep::Join,
            Some(_) => SetupStep::Host,
            None => SetupStep::Choose,
        };
        let (key, auto_start) = match config.restore_connection() {
            Ok(Some(saved)) => {
                config.port = saved.port;
                if let Some(target) = &saved.target {
                    config.peer_address = target.clone();
                }
                (
                    saved.secret,
                    config.auto_connect.then_some(saved.target.is_some()),
                )
            }
            Ok(None) => (
                new_pairing_key().expect("Windows secure random source unavailable"),
                None,
            ),
            Err(err) => {
                notice = format!("无法恢复已保存配对，请重新输入密钥并连接：{err:#}");
                // Keep the saved profile for recovery; never replace it with a random key.
                (String::new(), None)
            }
        };
        let runtime = Runtime::new(&config);
        let layout_draft = runtime.snapshot().layout;
        let selected = String::new();
        let secret = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("输入或粘贴配对密钥")
                .default_value(key)
                .masked(true)
        });
        let key_length = cx.new(|cx| InputState::new(window, cx).default_value("64"));
        let address = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("例如 192.168.1.20")
                .default_value(config.peer_address.clone())
        });
        let port = cx.new(|cx| InputState::new(window, cx).default_value(config.port.to_string()));
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        let refresh = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(250))
                    .await;
                if this
                    .update(cx, |this, cx| {
                        this.refresh_layout();
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        let mut workspace = Self {
            config,
            runtime,
            secret,
            key_length,
            address,
            port,
            configured,
            setup_step,
            panel: None,
            show_advanced: false,
            show_advanced_preferences: false,
            awaiting_connection: false,
            local_address: suggested_host_address(),
            focus,
            layout_draft,
            layout_revision: 0,
            layout_dirty: false,
            selected,
            drag: None,
            pan_drag: None,
            canvas_bounds: Arc::new(Mutex::new(Bounds::default())),
            view_scale: 0.12,
            view_offset: LayoutPoint { x: 32.0, y: 60.0 },
            fit_pending: true,
            fitted_size: size(px(0.), px(0.)),
            notice,
            input_service_notice: mwb::input_service::installation_notice().unwrap_or_default(),
            startup_status: None,
            startup_notice: String::new(),
            show_key: false,
            preparing: false,
            start_generation: 0,
            pending_connection: None,
            _refresh: refresh,
        };
        workspace.refresh_startup();
        if let Some(connect) = auto_start {
            workspace.start(connect, cx);
        }
        workspace
    }

    fn persist(&mut self) {
        if let Err(err) = self.config.save() {
            self.notice = format!("保存设置失败：{err:#}");
        }
    }

    fn refresh_startup(&mut self) {
        self.apply_startup_result(startup::status());
    }

    fn apply_startup_result(&mut self, result: anyhow::Result<startup::Status>) {
        match result {
            Ok(status) => {
                self.startup_status = Some(status);
                self.startup_notice = if status == startup::Status::OtherExecutable {
                    "自启动当前指向其他位置；可改用正在运行的版本，或关闭自启动。".into()
                } else {
                    String::new()
                };
            }
            Err(error) => {
                self.startup_status = None;
                self.startup_notice = format!("无法确认开机自启动设置：{error:#}");
            }
        }
    }

    fn start(&mut self, connect: bool, cx: &mut Context<Self>) {
        if self.preparing || self.runtime.running.load(Ordering::SeqCst) {
            return;
        }
        self.notice.clear();
        let secret = self.secret.read(cx).value().to_string();
        let result = (|| -> anyhow::Result<()> {
            validate_pairing_key(&secret)?;
            let port = self
                .port
                .read(cx)
                .value()
                .trim()
                .parse::<u16>()
                .map_err(|_| anyhow::anyhow!("连接端口须为 1–65535"))?;
            anyhow::ensure!(port != 0, "连接端口须为 1–65535");
            self.config.port = port;
            self.config.peer_address = self.address.read(cx).value().trim().to_string();
            if connect {
                anyhow::ensure!(
                    !self.config.peer_address.is_empty(),
                    "请输入另一台电脑的 IP 或主机名"
                );
            }
            Ok(())
        })();
        if let Err(err) = result {
            self.notice = format!("{err:#}");
            cx.notify();
            return;
        }
        self.preparing = true;
        self.awaiting_connection = false;
        self.start_generation = self.start_generation.wrapping_add(1);
        let generation = self.start_generation;
        self.pending_connection = None;
        self.notice = "正在准备配对…".into();
        let target = connect.then(|| self.config.peer_address.clone());
        let saved_target = target.clone();
        let port = self.config.port;
        let device_id = self.config.device_id.clone();
        // Custom passphrases use a memory-hard KDF; keep it off the UI thread.
        let key_task = cx.background_executor().spawn(async move {
            let key = parse_pairing_key(&secret)?;
            let saved = SavedConnection::new(&secret, saved_target, port, &device_id);
            Ok::<_, anyhow::Error>((key, saved))
        });
        cx.spawn(async move |this, cx| {
            let key = key_task.await;
            let _ = this.update(cx, |this, cx| {
                if this.start_generation != generation {
                    return;
                }
                this.preparing = false;
                this.notice.clear();
                match key.and_then(|(key, saved)| {
                    engine::start(this.runtime.clone(), this.config.clone(), key, target)?;
                    Ok(saved)
                }) {
                    Ok(saved) => {
                        this.awaiting_connection = true;
                        match saved {
                            Ok(saved) => this.pending_connection = Some(saved),
                            Err(err) => {
                                this.notice = format!("连接已启动，但无法保存配对信息：{err:#}");
                            }
                        }
                        this.refresh_layout();
                    }
                    Err(err) => {
                        this.awaiting_connection = false;
                        this.notice = format!("{err:#}");
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn stop(&mut self, cx: &mut Context<Self>) {
        self.refresh_layout();
        self.start_generation = self.start_generation.wrapping_add(1);
        self.preparing = false;
        self.pending_connection = None;
        self.awaiting_connection = false;
        self.runtime.stop();
        self.notice = "已停止连接；下次打开仍按启动连接偏好恢复。".into();
        cx.notify();
    }

    fn generate_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let result = self
            .key_length
            .read(cx)
            .value()
            .trim()
            .parse::<usize>()
            .map_err(|_| anyhow::anyhow!("随机密钥位数须为 1–64"))
            .and_then(new_pairing_key_with_length);
        match result {
            Ok(key) => {
                self.secret
                    .update(cx, |state, cx| state.set_value(key, window, cx));
                self.notice.clear();
            }
            Err(err) => self.notice = format!("{err:#}"),
        }
        cx.notify();
    }

    fn toggle_sharing(&mut self, cx: &mut Context<Self>) {
        if self.runtime.sharing_enabled() {
            self.runtime.reclaim();
            self.notice = "已暂停共享，键鼠由本机控制".into();
        } else {
            match platform::install_hooks(self.runtime.clone()).and_then(|()| self.runtime.resume())
            {
                Ok(()) => {
                    self.notice = "共享已恢复，可从屏幕边缘切换到另一台电脑".into();
                }
                Err(err) => self.notice = format!("输入捕获启动失败：{err:#}"),
            }
        }
        cx.notify();
    }

    fn copy_clipboard(&mut self, cx: &mut Context<Self>) {
        let result = self.runtime.exchange.send_clipboard(self.selected.clone());
        self.notice = match result {
            Ok(()) => "剪贴板文本或图片已加入发送队列".into(),
            Err(e) => format!("{e:#}"),
        };
        cx.notify();
    }

    fn choose_files(&mut self, directory: bool, cx: &mut Context<Self>) {
        let target = self.selected.clone();
        let prompt = cx.prompt_for_paths(PathPromptOptions {
            files: !directory,
            directories: directory,
            multiple: !directory,
            prompt: Some(
                if directory {
                    "选择接收文件夹"
                } else {
                    "选择要发送的文件"
                }
                .into(),
            ),
        });
        cx.spawn(async move |this, cx| {
            let result: anyhow::Result<_> = async { prompt.await? }.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(Some(paths)) if directory => {
                        if let Some(path) = paths.into_iter().next() {
                            this.config.receive_directory = path;
                            this.persist();
                            this.notice = "接收文件夹已更新".into();
                        }
                    }
                    Ok(Some(paths)) => {
                        this.notice = match this.runtime.exchange.send_files(target, paths) {
                            Ok(()) => "已发送文件邀请，等待对方接收".into(),
                            Err(e) => format!("{e:#}"),
                        };
                    }
                    Ok(None) => {}
                    Err(e) => this.notice = format!("打开文件选择器失败：{e:#}"),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn transfers(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.runtime.snapshot();
        let ready = self.runtime.exchange.ready();
        let target_online = state
            .layout
            .get(&self.selected)
            .is_some_and(|d| d.online && d.info.id != self.runtime.local.id);
        let transfers = self.runtime.exchange.snapshot();
        let ordered = transfers
            .iter()
            .filter(|t| !t.phase.terminal())
            .chain(transfers.iter().filter(|t| t.phase.terminal()).take(4));
        div()
            .v_flex()
            .w_full()
            .gap_3()
            .child(
                div()
                    .v_flex()
                    .gap_2()
                    .child(
                        div()
                            .v_flex()
                            .gap_1()
                            .child(
                                div()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("剪贴板与文件"),
                            )
                            .child(subtle(if self.config.auto_clipboard {
                                "在对端文件夹按 Ctrl+V，即可粘贴已复制的文件。"
                            } else {
                                "自动剪贴板已关闭 · 仍可手动发送文本、图片和文件"
                            })),
                    )
                    .child(div().text_xs().text_color(rgb(MUTED)).child(if ready {
                        "数据通道已连接"
                    } else {
                        "等待数据通道"
                    })),
            )
            .child(
                div()
                    .h_flex()
                    .gap_2()
                    .flex_wrap()
                    .child(
                        Button::new("send-files")
                            .primary()
                            .label("选择文件发送")
                            .disabled(!ready || !target_online)
                            .on_click(cx.listener(|this, _, _, cx| this.choose_files(false, cx))),
                    )
                    .child(
                        Button::new("send-clipboard-files")
                            .label("发送剪贴板中的文件")
                            .disabled(!ready || !target_online)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.notice = match this
                                    .runtime
                                    .exchange
                                    .send_clipboard_files(this.selected.clone())
                                {
                                    Ok(()) => "正在读取已复制的文件，等待对方接收".into(),
                                    Err(e) => format!("{e:#}"),
                                };
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("choose-receive-folder")
                            .ghost()
                            .label("接收文件夹")
                            .on_click(cx.listener(|this, _, _, cx| this.choose_files(true, cx))),
                    ),
            )
            .child(subtle(format!(
                "保存到 {} · {} · 无文件大小上限",
                self.config.receive_directory.display(),
                if self.config.transfer_rate_mib == 0 {
                    "不限速".into()
                } else {
                    format!("限速 {} MiB/s", self.config.transfer_rate_mib)
                }
            )))
            .when(transfers.is_empty(), |el| {
                el.child(subtle(
                    "选择设备后发送文件；收到文件邀请时点击接收。同名文件自动改名。",
                ))
            })
            .child(
                div()
                    .id("transfer-list")
                    .v_flex()
                    .gap_2()
                    .children(ordered.map(|t| {
                        let accept_id = t.id.clone();
                        let cancel_id = t.id.clone();
                        let peer = state
                            .layout
                            .get(&t.peer)
                            .map(|d| d.info.name.as_str())
                            .unwrap_or("其他设备");
                        let fraction = if t.size == 0 {
                            if t.phase == Phase::Complete { 1.0 } else { 0.0 }
                        } else {
                            (t.transferred as f64 / t.size as f64).min(1.0)
                        };
                        div()
                            .v_flex()
                            .gap_2()
                            .p_3()
                            .bg(rgb(BACKGROUND))
                            .rounded_md()
                            .child(
                                div()
                                    .h_flex()
                                    .items_center()
                                    .justify_between()
                                    .gap_2()
                                    .child(
                                        div()
                                            .v_flex()
                                            .flex_1()
                                            .min_w_0()
                                            .gap_1()
                                            .child(div().text_sm().truncate().child(format!(
                                                "{} · {} {}",
                                                t.name,
                                                if t.incoming { "来自" } else { "发往" },
                                                peer
                                            )))
                                            .child(subtle(format!(
                                                "{} · {} / {}",
                                                t.phase.label(),
                                                format_size(t.transferred),
                                                format_size(t.size)
                                            ))),
                                    )
                                    .child(
                                        div()
                                            .h_flex()
                                            .flex_shrink_0()
                                            .gap_2()
                                            .when(t.phase == Phase::Offered, |el| {
                                                el.child(
                                                    Button::new(format!("accept-{accept_id}"))
                                                        .small()
                                                        .primary()
                                                        .label("接收")
                                                        .disabled(!ready)
                                                        .on_click(cx.listener(
                                                            move |this, _, _, cx| {
                                                                if let Err(e) =
                                                                    this.runtime.exchange.accept(
                                                                        accept_id.clone(),
                                                                        this.config
                                                                            .receive_directory
                                                                            .clone(),
                                                                    )
                                                                {
                                                                    this.notice = format!("{e:#}");
                                                                }
                                                                cx.notify();
                                                            },
                                                        )),
                                                )
                                            })
                                            .when(!t.phase.terminal(), |el| {
                                                el.child(
                                                    Button::new(format!("cancel-{cancel_id}"))
                                                        .small()
                                                        .ghost()
                                                        .label(if t.phase == Phase::Offered {
                                                            "拒绝"
                                                        } else {
                                                            "取消"
                                                        })
                                                        .on_click(cx.listener(
                                                            move |this, _, _, cx| {
                                                                if let Err(e) = this
                                                                    .runtime
                                                                    .exchange
                                                                    .cancel(cancel_id.clone())
                                                                {
                                                                    this.notice = format!("{e:#}");
                                                                }
                                                                cx.notify();
                                                            },
                                                        )),
                                                )
                                            }),
                                    ),
                            )
                            .child(
                                div().h(px(3.)).w_full().bg(rgb(BORDER)).rounded_sm().child(
                                    div()
                                        .h_full()
                                        .w(relative(fraction as f32))
                                        .bg(rgb(ACCENT))
                                        .rounded_sm(),
                                ),
                            )
                            .when(!t.detail.is_empty(), |el| {
                                el.child(subtle(t.detail.clone()))
                            })
                            .when_some(t.path.clone(), |el, path| {
                                el.child(subtle(path.display().to_string()))
                            })
                    })),
            )
    }

    fn refresh_layout(&mut self) {
        let state = self.runtime.snapshot();
        if self.awaiting_connection && state.established {
            self.configured = true;
            self.awaiting_connection = false;
            if self.panel == Some(Panel::Connection) {
                self.panel = None;
            }
            self.fit_pending = true;
        } else if self.awaiting_connection
            && !self.preparing
            && !self.runtime.running.load(Ordering::SeqCst)
        {
            self.awaiting_connection = false;
            if self.notice.is_empty() {
                self.notice = state.detail.clone();
            }
        }
        let remember = state.established && self.pending_connection.is_some();
        if remember {
            self.config.saved_connection = self.pending_connection.take();
        }
        if remember || self.config.layout != state.layout {
            self.config.layout = state.layout.clone();
            self.persist();
        }
        if !self.layout_dirty
            && self.drag.is_none()
            && (self.layout_draft != state.layout || self.layout_revision != state.revision)
        {
            if self.layout_draft.devices.len() != state.layout.devices.len() {
                self.fit_pending = true;
            }
            self.layout_draft = state.layout;
            self.layout_revision = state.revision;
        }
        if self.layout_draft.get(&self.selected).is_none() {
            self.selected.clear();
        }
        if self.canvas_bounds.lock().unwrap().size != self.fitted_size {
            self.fit_pending = true;
        }
        if self.fit_pending && self.drag.is_none() && self.pan_drag.is_none() {
            self.fit_canvas();
        }
    }
    fn fit_canvas(&mut self) {
        let bounds = *self.canvas_bounds.lock().unwrap();
        let width = f32::from(bounds.size.width) as f64;
        let height = f32::from(bounds.size.height) as f64;
        if width < 100.0 || height < 100.0 {
            return;
        }
        let extent = self.layout_draft.extent();
        self.view_scale = ((width - 120.0) / extent.width as f64)
            .min((height - 180.0) / extent.height as f64)
            .clamp(0.002, 0.12);
        self.view_offset = LayoutPoint {
            x: (width - extent.width as f64 * self.view_scale) / 2.0
                - extent.x as f64 * self.view_scale,
            y: (height - extent.height as f64 * self.view_scale) / 2.0
                - 12.0
                - extent.y as f64 * self.view_scale,
        };
        self.fit_pending = false;
        self.fitted_size = bounds.size;
    }
    fn zoom_canvas(&mut self, factor: f64) {
        let b = *self.canvas_bounds.lock().unwrap();
        let center = LayoutPoint {
            x: f32::from(b.size.width) as f64 / 2.0,
            y: f32::from(b.size.height) as f64 / 2.0,
        };
        let next = (self.view_scale * factor).clamp(0.002, 2.0);
        let ratio = next / self.view_scale;
        self.view_offset = LayoutPoint {
            x: center.x + (self.view_offset.x - center.x) * ratio,
            y: center.y + (self.view_offset.y - center.y) * ratio,
        };
        self.view_scale = next;
    }
    fn canvas_move(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        if let Some(drag) = &mut self.drag {
            if event.pressed_button != Some(MouseButton::Left) {
                self.drag = None;
                return;
            }
            let distance = f32::from(event.position.x - drag.start.x).abs()
                + f32::from(event.position.y - drag.start.y).abs();
            if !drag.moved && distance < 4.0 {
                return;
            }
            drag.moved = true;
            if let Some(d) = self
                .layout_draft
                .devices
                .iter_mut()
                .find(|d| d.info.id == drag.id)
            {
                d.x = (drag.x as f64
                    + f32::from(event.position.x - drag.start.x) as f64 / drag.scale)
                    .round()
                    .clamp(-1_000_000.0, 1_000_000.0) as i32;
                d.y = (drag.y as f64
                    + f32::from(event.position.y - drag.start.y) as f64 / drag.scale)
                    .round()
                    .clamp(-1_000_000.0, 1_000_000.0) as i32;
                self.layout_dirty = true;
                cx.notify();
            }
        } else if let Some((start, offset)) = self.pan_drag {
            if event.pressed_button != Some(MouseButton::Middle) {
                self.pan_drag = None;
                return;
            }
            self.view_offset = LayoutPoint {
                x: offset.x + f32::from(event.position.x - start.x) as f64,
                y: offset.y + f32::from(event.position.y - start.y) as f64,
            };
            cx.notify();
        }
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        self.refresh_layout();
        self.runtime.stop();
    }
}

fn subtle(text: impl Into<SharedString>) -> Div {
    div().text_xs().text_color(rgb(MUTED)).child(text.into())
}

fn option_row(title: &'static str, desc: &'static str, control: impl IntoElement) -> Div {
    div()
        .h_flex()
        .items_center()
        .justify_between()
        .gap_4()
        .py_2()
        .border_b_1()
        .border_color(rgb(BORDER))
        .child(
            div()
                .v_flex()
                .flex_1()
                .min_w_0()
                .gap_1()
                .text_xs()
                .child(title)
                .child(subtle(desc)),
        )
        .child(control)
}

fn suggested_host_address() -> String {
    // UDP connect only selects a local route; it sends no packet. Prefer an
    // address other computers can enter, rather than the listener's 0.0.0.0.
    std::net::UdpSocket::bind((std::net::Ipv4Addr::UNSPECIFIED, 0))
        .and_then(|socket| {
            socket.connect((std::net::Ipv4Addr::new(192, 0, 2, 1), 9))?;
            socket.local_addr()
        })
        .ok()
        .map(|address| address.ip())
        .filter(|address| !address.is_unspecified() && !address.is_loopback())
        .map(|address| address.to_string())
        .unwrap_or_else(|| std::env::var("COMPUTERNAME").unwrap_or_else(|_| "localhost".into()))
}

fn main() {
    if std::env::args().nth(1).as_deref() == Some("--remove-autostart") {
        // MSI invokes this before deleting files, without opening the UI or
        // acquiring the profile lock. Only this account's matching entry is removed.
        std::process::exit(match startup::remove_if_current() {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("{error:#}");
                1
            }
        });
    }
    if std::env::args().nth(1).as_deref() == Some("--check-input-service") {
        // A deployment smoke check that never opens a window, installs a hook,
        // injects input or touches the clipboard. Read the exit code from the
        // launching process because the release binary has no console window.
        std::process::exit(match platform::input_service_ready() {
            Ok(true) => 0,
            Ok(false) => 2,
            Err(error) => {
                eprintln!("{error:#}");
                3
            }
        });
    }
    let _instance = match instance::acquire(&Config::path()) {
        Ok(instance::Acquisition::Acquired(guard)) => guard,
        Ok(instance::Acquisition::Running(pid)) => {
            if let Some(pid) = pid {
                instance::activate_existing(pid);
            }
            return;
        }
        Err(error) => {
            // A release binary has no console; make startup failures visible.
            use windows::{Win32::UI::WindowsAndMessaging::*, core::PCWSTR};
            let text: Vec<u16> = format!("无法启动 MWB：{error:#}")
                .encode_utf16()
                .chain(Some(0))
                .collect();
            unsafe {
                MessageBoxW(
                    None,
                    PCWSTR(text.as_ptr()),
                    windows::core::w!("MWB Rust"),
                    MB_OK | MB_ICONERROR,
                );
            }
            return;
        }
    };
    gpui_kit::application()
        .with_assets(assets::AllAssets)
        .run(|cx| {
            gpui_kit::init(cx);
            Theme::change(ThemeMode::Light, None, cx);
            Theme::update(cx, |theme| {
                theme.colors.primary = rgb(ACCENT).into();
                theme.colors.button_primary = rgb(ACCENT).into();
                theme.colors.button_primary_hover = rgb(0x4B59D4).into();
                theme.colors.button_primary_active = rgb(0x3F4DBF).into();
            });
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1100.), px(800.)),
                    cx,
                ))),
                window_min_size: Some(size(px(900.), px(660.))),
                titlebar: Some(TitlebarOptions {
                    title: Some(instance::WINDOW_TITLE.into()),
                    ..Default::default()
                }),
                ..Default::default()
            };
            gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| Workspace::new(window, cx))
            })
            .expect("Unable to open MWB window");
            cx.activate(true);
        });
}

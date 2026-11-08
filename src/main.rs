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
    layout::{Layout, Point as LayoutPoint},
    platform,
    switching::EdgeSwitchCondition,
};
use std::{
    sync::{Arc, Mutex, atomic::Ordering},
    time::Duration,
};

const INK: u32 = 0x202D29;
const MUTED: u32 = 0x7B8781;
const GREEN: u32 = 0x227455;
const BORDER: u32 = 0xE4E9E5;

struct CanvasDrag {
    id: String,
    start: Point<Pixels>,
    x: i32,
    y: i32,
    scale: f64,
}

struct Workspace {
    config: Config,
    runtime: Arc<Runtime>,
    secret: Entity<InputState>,
    key_length: Entity<InputState>,
    address: Entity<InputState>,
    tab: usize,
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
    notice: String,
    input_service_notice: String,
    show_key: bool,
    preparing: bool,
    _refresh: Task<()>,
}

impl Workspace {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (config, mut notice) = match Config::load() {
            Ok(config) => (config, String::new()),
            Err(err) => (Config::default(), format!("{err:#}")),
        };
        if let Err(err) = config.save() {
            notice = format!("保存设备标识失败：{err:#}");
        }
        let runtime = Runtime::new(&config);
        let layout_draft = runtime.snapshot().layout;
        let selected = config.device_id.clone();
        let key = new_pairing_key().expect("Windows secure random source unavailable");
        let secret = cx.new(|cx| InputState::new(window, cx).default_value(key).masked(true));
        let key_length = cx.new(|cx| InputState::new(window, cx).default_value("64"));
        let address = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("例如 192.168.1.20")
                .default_value(config.peer_address.clone())
        });
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
        Self {
            config,
            runtime,
            secret,
            key_length,
            address,
            tab: 0,
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
            notice,
            input_service_notice: mwb::input_service::installation_notice().unwrap_or_default(),
            show_key: false,
            preparing: false,
            _refresh: refresh,
        }
    }

    fn persist(&mut self) {
        if let Err(err) = self.config.save() {
            self.notice = format!("保存设置失败：{err:#}");
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
        self.notice = "正在准备配对…".into();
        let target = connect.then(|| self.config.peer_address.clone());
        // Custom passphrases use a memory-hard KDF; keep it off the UI thread.
        let key_task = cx
            .background_executor()
            .spawn(async move { parse_pairing_key(&secret) });
        cx.spawn(async move |this, cx| {
            let key = key_task.await;
            let _ = this.update(cx, |this, cx| {
                this.preparing = false;
                this.notice.clear();
                match key.and_then(|key| {
                    engine::start(this.runtime.clone(), this.config.clone(), key, target)
                }) {
                    Ok(()) => this.persist(),
                    Err(err) => this.notice = format!("{err:#}"),
                }
                cx.notify();
            });
        })
        .detach();
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
        card()
            .gap_3()
            .child(
                div()
                    .h_flex()
                    .justify_between()
                    .items_center()
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
                                "自动同步文本、图片与文件列表 · 在对端文件夹按 Ctrl+V 粘贴文件"
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
                    .max_h(px(350.))
                    .overflow_y_scroll()
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
                            .bg(rgb(0xF4F6F3))
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
                                            .gap_1()
                                            .child(div().text_sm().child(format!(
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
                                        .bg(rgb(GREEN))
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
        if self.config.layout != state.layout {
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
            self.selected = self.runtime.local.id.clone();
        }
        if self.fit_pending {
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
        self.view_scale = ((width - 80.0) / extent.width as f64)
            .min((height - 100.0) / extent.height as f64)
            .clamp(0.002, 1.0);
        self.view_offset = LayoutPoint {
            x: (width - extent.width as f64 * self.view_scale) / 2.0
                - extent.x as f64 * self.view_scale,
            y: (height - extent.height as f64 * self.view_scale) / 2.0
                - extent.y as f64 * self.view_scale,
        };
        self.fit_pending = false;
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
        if let Some(drag) = &self.drag {
            if event.pressed_button != Some(MouseButton::Left) {
                self.drag = None;
                return;
            }
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

    fn sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .v_flex()
            .w(px(208.))
            .h_full()
            .flex_shrink_0()
            .bg(rgb(0xF1F4F0))
            .border_r_1()
            .border_color(rgb(BORDER))
            .p_5()
            .gap_2()
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap_3()
                    .mt_3()
                    .mb_8()
                    .child(
                        div()
                            .size(px(34.))
                            .rounded_lg()
                            .bg(rgb(GREEN))
                            .text_color(gpui_kit::white())
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(Icon::new(assets::IconName::MousePointer2).size_5()),
                    )
                    .child(
                        div()
                            .v_flex()
                            .child(div().text_lg().font_weight(FontWeight::BOLD).child("mwb."))
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(rgb(MUTED))
                                    .child("无界协作 · RUST EDITION"),
                            ),
                    ),
            )
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(rgb(MUTED))
                    .mb_2()
                    .child("工作空间"),
            )
            .children(
                [
                    (0, "设备与连接", assets::IconName::Monitor),
                    (1, "共享偏好", assets::IconName::Settings2),
                    (2, "关于此版本", assets::IconName::Info),
                ]
                .into_iter()
                .map(|(index, label, icon)| {
                    div()
                        .id(("nav", index))
                        .h_flex()
                        .gap_3()
                        .items_center()
                        .px_3()
                        .py_3()
                        .rounded_lg()
                        .cursor_pointer()
                        .bg(if self.tab == index {
                            rgb(0xDFE9DE)
                        } else {
                            rgb(0xF1F4F0)
                        })
                        .text_color(if self.tab == index {
                            rgb(GREEN)
                        } else {
                            rgb(0x6E7B73)
                        })
                        .child(Icon::new(icon).size_4())
                        .child(label)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.tab = index;
                            cx.notify();
                        }))
                }),
            )
            .child(div().flex_1())
            .child(
                div()
                    .border_t_1()
                    .border_color(rgb(BORDER))
                    .pt_4()
                    .v_flex()
                    .gap_2()
                    .child(
                        div()
                            .h_flex()
                            .items_center()
                            .gap_2()
                            .text_color(rgb(GREEN))
                            .child(div().size(px(6.)).rounded_full().bg(rgb(GREEN)))
                            .child("本地优先"),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child("直连你的另一台电脑。"),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child(format!("v{} · Windows 预览版", env!("CARGO_PKG_VERSION"))),
                    ),
            )
    }

    fn devices(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let saved = self.runtime.snapshot();
        let selected_offline = saved
            .layout
            .get(&self.selected)
            .is_some_and(|d| !d.online && d.info.id != self.runtime.local.id);
        let can_remove = !self.runtime.running.load(Ordering::SeqCst) || saved.host;
        let invalid = self.layout_draft.validate().err().map(|e| e.to_string());
        let bounds = self.canvas_bounds.clone();
        let mut area = div()
            .id("layout-canvas")
            .relative()
            .w_full()
            .h(px(400.))
            .overflow_hidden()
            .rounded_lg()
            .bg(rgb(0xF2F5F1))
            .border_1()
            .border_color(rgb(BORDER))
            .child(
                canvas(
                    move |b, _, _| {
                        *bounds.lock().unwrap() = b;
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .on_mouse_move(
                cx.listener(|this, event: &MouseMoveEvent, _, cx| this.canvas_move(event, cx)),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.drag = None;
                    cx.notify();
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.drag = None;
                    cx.notify();
                }),
            )
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(|this, event: &MouseDownEvent, _, _| {
                    this.pan_drag = Some((event.position, this.view_offset));
                }),
            )
            .on_mouse_up(
                MouseButton::Middle,
                cx.listener(|this, _, _, _| this.pan_drag = None),
            )
            .on_mouse_up_out(
                MouseButton::Middle,
                cx.listener(|this, _, _, _| this.pan_drag = None),
            );
        for row in 1..10 {
            area = area.child(
                div()
                    .absolute()
                    .top(px(row as f32 * 40.))
                    .left_0()
                    .w_full()
                    .h(px(1.))
                    .bg(rgb(0xE6ECE4)),
            );
        }
        for column in 1..28 {
            area = area.child(
                div()
                    .absolute()
                    .left(px(column as f32 * 40.))
                    .top_0()
                    .h_full()
                    .w(px(1.))
                    .bg(rgb(0xE6ECE4)),
            );
        }
        for device in &self.layout_draft.devices {
            let x = self.view_offset.x + device.x as f64 * self.view_scale;
            let y = self.view_offset.y + device.y as f64 * self.view_scale;
            let selected = self.selected == device.info.id;
            let online = saved.layout.get(&device.info.id).is_some_and(|d| d.online);
            let local = device.info.id == self.runtime.local.id;
            for (index, screen) in device.info.screens.iter().enumerate() {
                let screen_x = x + (screen.rect.x - device.info.bounds.x) as f64 * self.view_scale;
                let screen_y = y + (screen.rect.y - device.info.bounds.y) as f64 * self.view_scale;
                let w = screen.rect.width as f64 * self.view_scale;
                let h = screen.rect.height as f64 * self.view_scale;
                let id = device.info.id.clone();
                let origin = (device.x, device.y);
                let screen_element = div()
                    .id(format!("screen-{}-{index}", id))
                    .absolute()
                    .left(px(screen_x as f32))
                    .top(px(screen_y as f32))
                    .w(px(w as f32))
                    .h(px(h as f32))
                    .border_2()
                    .border_color(rgb(if invalid.is_some() {
                        0xBF5F4C
                    } else if selected {
                        GREEN
                    } else {
                        0x9BAE9D
                    }))
                    .rounded_md()
                    .bg(rgb(if !online {
                        0xE3E7E1
                    } else if local {
                        0xE1EED7
                    } else {
                        0xE8F0E9
                    }))
                    .overflow_hidden()
                    .v_flex()
                    .justify_center()
                    .items_center()
                    .gap_1()
                    .p_2()
                    .when(w > 75.0 && h > 35.0, |el| {
                        el.child(
                            div()
                                .text_size(px(11.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(device.info.name.clone()),
                        )
                    })
                    .when(w > 95.0 && h > 65.0, |el| {
                        el.child(
                            div()
                                .text_size(px(10.))
                                .text_color(rgb(MUTED))
                                .child(format!("{} × {}", screen.rect.width, screen.rect.height)),
                        )
                    })
                    .when(w > 95.0 && h > 90.0, |el| {
                        el.child(
                            div()
                                .text_size(px(10.))
                                .text_color(rgb(GREEN))
                                .child(if local {
                                    "本机"
                                } else if online {
                                    "在线"
                                } else {
                                    "离线"
                                }),
                        )
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                            this.runtime.return_local();
                            this.selected = id.clone();
                            this.drag = Some(CanvasDrag {
                                id: id.clone(),
                                start: event.position,
                                x: origin.0,
                                y: origin.1,
                                scale: this.view_scale,
                            });
                            cx.stop_propagation();
                            cx.notify();
                        }),
                    );
                area = area.child(screen_element);
            }
        }
        card()
            .gap_3()
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .v_flex()
                            .gap_1()
                            .child(
                                div()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("你的设备，自由摆放"),
                            )
                            .child(subtle(
                                "按实际分辨率等比例显示 · 拖动屏幕调整位置 · 中键拖动画布",
                            )),
                    )
                    .child(
                        div()
                            .h_flex()
                            .gap_2()
                            .child(Button::new("zoom-out").small().label("−").on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.zoom_canvas(0.8);
                                    cx.notify();
                                }),
                            ))
                            .child(
                                Button::new("zoom-in")
                                    .small()
                                    .label("+")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.zoom_canvas(1.25);
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("fit-layout")
                                    .small()
                                    .label("适应画布")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.fit_canvas();
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
            .child(area)
            .when(selected_offline, |el| {
                el.child(
                    div()
                        .h_flex()
                        .items_center()
                        .justify_between()
                        .gap_3()
                        .child(subtle(if !can_remove {
                            "在连接中心可移除离线设备；停止连接后可清理本机记录。"
                        } else if self.layout_dirty {
                            "请先保存或还原布局，再移除离线设备。"
                        } else {
                            "移除后释放设备名额；该设备仍可使用相同密钥重新加入。"
                        }))
                        .child(
                            Button::new("remove-offline-device")
                                .small()
                                .label("移除选中离线设备")
                                .disabled(
                                    !can_remove
                                        || self.layout_dirty
                                        || self.preparing
                                        || self.drag.is_some(),
                                )
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.notice = match this
                                        .runtime
                                        .remove_offline(this.layout_revision, &this.selected)
                                    {
                                        Ok(()) => "移除请求已提交，设备列表更新后自动保存。".into(),
                                        Err(e) => format!("{e:#}"),
                                    };
                                    this.refresh_layout();
                                    cx.notify();
                                })),
                        ),
                )
            })
            .child(div().h_flex().flex_wrap().gap_2().children(
                self.layout_draft.devices.iter().map(|d| {
                    let id = d.info.id.clone();
                    let label = format!(
                        "{} · {} × {}",
                        d.info.name, d.info.bounds.width, d.info.bounds.height
                    );
                    Button::new(format!("select-{}", id))
                        .small()
                        .label(label)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.selected = id.clone();
                            cx.notify();
                        }))
                }),
            ))
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .child(subtle(invalid.unwrap_or_else(|| {
                        if self.layout_dirty {
                            "布局尚未保存；支持任意偏移和间隙，无需对齐网格。".into()
                        } else {
                            format!(
                                "{} 台设备 · 保存后同步整个工作组 · {:.0}%",
                                self.layout_draft.devices.len(),
                                self.view_scale * 100.0
                            )
                        }
                    })))
                    .child(
                        div()
                            .h_flex()
                            .gap_2()
                            .child(
                                Button::new("reset-layout")
                                    .small()
                                    .ghost()
                                    .label("还原")
                                    .disabled(!self.layout_dirty)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.layout_dirty = false;
                                        this.drag = None;
                                        this.refresh_layout();
                                        this.fit_canvas();
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("save-layout")
                                    .small()
                                    .primary()
                                    .label("保存布局")
                                    .disabled(!self.layout_dirty)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        match this.runtime.arrange(
                                            this.layout_revision,
                                            this.layout_draft.placements(),
                                        ) {
                                            Ok(()) => {
                                                this.layout_dirty = false;
                                                this.notice =
                                                    "布局已提交，应用后会同步到工作组。".into();
                                            }
                                            Err(e) => this.notice = format!("{e:#}"),
                                        }
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
    }

    fn connections(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let running = self.runtime.running.load(Ordering::SeqCst) || self.preparing;
        card()
            .gap_4()
            .child(
                div()
                    .h_flex()
                    .justify_between()
                    .items_center()
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("连接工作组"),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child(format!("局域网直连 · TCP {}", self.config.port)),
                    ),
            )
            .child(
                div()
                    .h_flex()
                    .gap_6()
                    .child(
                        div()
                            .v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_3()
                            .child(
                                div()
                                    .text_sm()
                                    .child("1. 两端使用相同密钥 · 可自定义 1–64 位"),
                            )
                            .child(Input::new(&self.secret).disabled(running))
                            .child(
                                div()
                                    .h_flex()
                                    .gap_2()
                                    .child(
                                        Button::new("copy-key")
                                            .small()
                                            .icon(assets::IconName::Copy)
                                            .label("复制密钥")
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                cx.write_to_clipboard(ClipboardItem::new_string(
                                                    this.secret.read(cx).value().to_string(),
                                                ));
                                                this.notice =
                                                    "已复制配对密钥。在另一台电脑的密钥框中粘贴"
                                                        .into();
                                                cx.notify();
                                            })),
                                    )
                                    .child(
                                        Button::new("show-key")
                                            .small()
                                            .ghost()
                                            .label(if self.show_key { "隐藏" } else { "显示" })
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.show_key = !this.show_key;
                                                this.secret.update(cx, |state, cx| {
                                                    state.set_masked(!this.show_key, window, cx)
                                                });
                                                cx.notify();
                                            })),
                                    ),
                            )
                            .child(
                                div()
                                    .h_flex()
                                    .items_center()
                                    .gap_2()
                                    .child(subtle("随机生成"))
                                    .child(
                                        Input::new(&self.key_length)
                                            .small()
                                            .w(px(52.))
                                            .disabled(running),
                                    )
                                    .child(subtle("位"))
                                    .child(
                                        Button::new("new-key")
                                            .small()
                                            .label("生成密钥")
                                            .disabled(running)
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.generate_key(window, cx)
                                            })),
                                    ),
                            )
                            .child(subtle("可直接输入短口令；短密钥更容易被猜中。"))
                            .child(subtle("密钥仅在本次运行保留，不写入配置。")),
                    )
                    .child(div().w(px(1.)).bg(rgb(BORDER)))
                    .child(
                        div()
                            .v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_3()
                            .child(div().text_sm().child("2. 加入时填写连接中心的 IP 或主机名"))
                            .child(Input::new(&self.address).disabled(running))
                            .child(
                                div()
                                    .h_flex()
                                    .gap_2()
                                    .child(
                                        Button::new("listen")
                                            .label("创建工作组")
                                            .disabled(running)
                                            .on_click(
                                                cx.listener(|this, _, _, cx| this.start(false, cx)),
                                            ),
                                    )
                                    .child(
                                        Button::new("connect")
                                            .primary()
                                            .icon(assets::IconName::ArrowRight)
                                            .label(if self.preparing {
                                                "准备配对…"
                                            } else {
                                                "加入工作组"
                                            })
                                            .disabled(running)
                                            .on_click(
                                                cx.listener(|this, _, _, cx| this.start(true, cx)),
                                            ),
                                    ),
                            )
                            .child(subtle("一台电脑创建工作组，其他电脑使用相同密钥加入。连接中心需保持运行。")),
                    ),
            )
    }

    fn sharing(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let connected = self.runtime.snapshot().connected;
        let enabled = self.runtime.sharing_enabled();
        let remote = self.runtime.remote.load(Ordering::SeqCst);
        let incoming = self.runtime.incoming.load(Ordering::SeqCst);
        let selected_online = self
            .runtime
            .snapshot()
            .layout
            .get(&self.selected)
            .is_some_and(|d| d.online && d.info.id != self.runtime.local.id);
        card()
            .gap_4()
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .v_flex()
                            .gap_1()
                            .child(div().font_weight(FontWeight::SEMIBOLD).child("键鼠共享"))
                            .child(subtle(if remote {
                                "正在控制另一台电脑"
                            } else if incoming {
                                "正在接收另一台电脑的输入"
                            } else if enabled {
                                "连接后自动共享 · 从屏幕边缘切换"
                            } else if connected {
                                "共享已暂停，点击恢复后继续使用"
                            } else {
                                "连接成功后，两端自动共享键鼠"
                            })),
                    )
                    .child(
                        Button::new("sharing")
                            .primary()
                            .label(if enabled {
                                "暂停共享"
                            } else if connected {
                                "恢复共享"
                            } else {
                                "连接后自动开启"
                            })
                            .disabled(!connected)
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_sharing(cx))),
                    ),
            )
            .when(!self.input_service_notice.is_empty(), |el| {
                el.child(
                    div()
                        .rounded_md()
                        .px_3()
                        .py_2()
                        .bg(rgb(0xFFF4DF))
                        .text_color(rgb(0x856018))
                        .text_xs()
                        .child(self.input_service_notice.clone()),
                )
            })
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .justify_between()
                    .border_t_1()
                    .border_color(rgb(BORDER))
                    .pt_3()
                    .child(
                        div()
                            .h_flex()
                            .items_center()
                            .gap_2()
                            .child(Icon::new(assets::IconName::CornerDownLeft).size_4())
                            .child(div().text_xs().text_color(rgb(MUTED)).child("随时回到本机"))
                            .child(
                                div()
                                    .px_2()
                                    .py_1()
                                    .bg(rgb(0xF4F6F3))
                                    .rounded_md()
                                    .text_xs()
                                    .child("Ctrl + Alt + Home"),
                            ),
                    )
                    .child(
                        div()
                            .h_flex()
                            .gap_2()
                            .child(
                                Button::new("send-clipboard")
                                    .small()
                                    .ghost()
                                    .label("发送剪贴板")
                                    .disabled(
                                        !connected
                                            || !enabled
                                            || !selected_online
                                            || !self.runtime.exchange.ready(),
                                    )
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.copy_clipboard(cx)),
                                    ),
                            )
                            .child(
                                Button::new("control")
                                    .small()
                                    .label("控制选中设备")
                                    .disabled(
                                        !connected
                                            || !enabled
                                            || !selected_online
                                            || remote
                                            || self.runtime.incoming.load(Ordering::SeqCst),
                                    )
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        if let Err(err) = this.runtime.control(&this.selected) {
                                            this.notice = format!("无法切换控制：{err:#}");
                                        }
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
    }

    fn preferences(&self, cx: &mut Context<Self>) -> impl IntoElement {
        card()
            .gap_5()
            .child(div().font_weight(FontWeight::SEMIBOLD).child("共享偏好"))
            .child(option_row(
                "相对移动鼠标",
                "默认按屏幕比例移动；开启后使用原生相对移动。更改后从边缘重新切换。",
                Button::new("relative-mouse-toggle")
                    .label(if self.config.relative_mouse {
                        "已开启"
                    } else {
                        "已关闭"
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        if this.runtime.snapshot().connected {
                            this.runtime.return_local();
                        }
                        this.config.relative_mouse = !this.config.relative_mouse;
                        this.runtime
                            .relative_mouse
                            .store(this.config.relative_mouse, Ordering::SeqCst);
                        this.persist();
                        cx.notify();
                    })),
            ))
            .child(option_row(
                "从屏幕边缘切换",
                "按画布中的屏幕位置和移动方向切换；允许错位与空隙。",
                Button::new("edge-toggle")
                    .label(if self.config.edge_switch {
                        "已开启"
                    } else {
                        "已关闭"
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.config.edge_switch = !this.config.edge_switch;
                        this.runtime
                            .edge
                            .store(this.config.edge_switch, Ordering::SeqCst);
                        this.persist();
                        cx.notify();
                    })),
            ))
            .child(option_row(
                "边缘切换条件",
                "仅影响自动跨屏；左右 Ctrl / Shift 均可，手动控制和紧急返回始终可用。",
                div().h_flex().gap_2().children(
                    [
                        (EdgeSwitchCondition::Always, "直接切换"),
                        (EdgeSwitchCondition::Ctrl, "按住 Ctrl"),
                        (EdgeSwitchCondition::Shift, "按住 Shift"),
                    ]
                    .into_iter()
                    .enumerate()
                    .map(|(i, (condition, label))| {
                        Button::new(("edge-condition", i))
                            .small()
                            .label(label)
                            .when(self.config.edge_switch_condition == condition, |b| {
                                b.primary()
                            })
                            .disabled(!self.config.edge_switch)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.config.edge_switch_condition = condition;
                                this.runtime.switch_gate.set_condition(condition);
                                this.persist();
                                cx.notify();
                            }))
                    }),
                ),
            ))
            .child(option_row(
                "避开屏幕角落",
                "开启后，每块屏幕四角的 100 像素区域不触发切换。手动控制不受影响。",
                Button::new("block-corners-toggle")
                    .label(if self.config.block_corners {
                        "已开启"
                    } else {
                        "已关闭"
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.config.block_corners = !this.config.block_corners;
                        this.runtime
                            .block_corners
                            .store(this.config.block_corners, Ordering::SeqCst);
                        this.persist();
                        cx.notify();
                    })),
            ))
            .child(option_row(
                "自动重连",
                "连接失败或中断后，主动连接端逐步延长间隔重试；暂停状态会保留。等待端持续监听。",
                Button::new("auto-reconnect-toggle")
                    .label(if self.config.auto_reconnect {
                        "已开启"
                    } else {
                        "已关闭"
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.config.auto_reconnect = !this.config.auto_reconnect;
                        this.runtime
                            .auto_reconnect
                            .store(this.config.auto_reconnect, Ordering::SeqCst);
                        this.persist();
                        cx.notify();
                    })),
            ))
            .child(option_row(
                "共享键盘",
                "键盘跟随鼠标；关闭后仅共享鼠标。",
                Button::new("keyboard-toggle")
                    .label(if self.config.keyboard {
                        "已开启"
                    } else {
                        "已关闭"
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.runtime.return_local();
                        this.config.keyboard = !this.config.keyboard;
                        this.runtime
                            .keyboard
                            .store(this.config.keyboard, Ordering::SeqCst);
                        this.persist();
                        cx.notify();
                    })),
            ))
            .child(option_row(
                "自动剪贴板",
                "同步文本、图片与文件列表；文件在对端 Ctrl+V 时传输，暂停时停止自动同步。",
                Button::new("auto-clipboard-toggle")
                    .label(if self.config.auto_clipboard {
                        "已开启"
                    } else {
                        "已关闭"
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.config.auto_clipboard = !this.config.auto_clipboard;
                        this.runtime
                            .exchange
                            .auto_clipboard
                            .store(this.config.auto_clipboard, Ordering::SeqCst);
                        this.persist();
                        cx.notify();
                    })),
            ))
            .child(option_row(
                "文件传输速率",
                "默认 8 MiB/s。网络较慢或鼠标不流畅时可调低；0 表示不限速。",
                div()
                    .h_flex()
                    .gap_1()
                    .flex_wrap()
                    .children([1, 4, 8, 16, 32, 0].into_iter().map(|rate| {
                        Button::new(format!("transfer-rate-{rate}"))
                            .small()
                            .label(if rate == 0 {
                                "不限速".into()
                            } else {
                                format!("{rate}")
                            })
                            .disabled(rate == self.config.transfer_rate_mib)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.config.transfer_rate_mib = rate;
                                this.runtime.exchange.set_rate_mib(rate);
                                this.persist();
                                cx.notify();
                            }))
                    })),
            ))
            .child(option_row(
                "紧急返回",
                "立即停止本机共享，并释放远端按下的按键。",
                div().text_sm().child("Ctrl + Alt + Home"),
            ))
            .child(subtle(
                "设备标识、已保存布局、共享偏好和连接中心地址会保存在本机。",
            ))
    }

    fn about(&self) -> impl IntoElement {
        card().gap_5()
            .child(div().text_2xl().font_weight(FontWeight::SEMIBOLD).child("一套键鼠，自由连接。"))
            .child(subtle(format!("MWB Rust · {} 预览版", env!("CARGO_PKG_VERSION"))))
            .child(div().text_sm().child("以 Rust + GPUI Kit 重写 Mouse without Borders 的核心使用流程。"))
            .child(div().text_sm().child("当前支持：多设备工作组、自由屏幕布局、键鼠共享、自动文本/图片剪贴板、跨设备 Ctrl+V 粘贴文件与不限大小的文件传输。"))
            .child(div().text_sm().child("接收端可选安装锁屏输入服务，允许从其他电脑操作锁屏界面；安装方法见 README。控制端需保持解锁。"))
            .child(div().text_sm().child("后续计划：文件夹传输、断点续传、自动发现与托盘。"))
            .child(div().text_sm().child("两端均需运行本重写版。首次登录后先打开程序并连接工作组；暂不提供远程 Ctrl+Alt+Delete。"))
            .child(div().text_xs().text_color(rgb(MUTED)).child("Native UI · GPUI Kit 0.8.1  /  Transport · Noise + ChaCha20-Poly1305"))
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        self.runtime.stop();
    }
}

impl Render for Workspace {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.runtime.snapshot();
        let running = self.runtime.running.load(Ordering::SeqCst);
        div()
            .h_flex()
            .size_full()
            .bg(rgb(0xFAFBF8))
            .text_color(rgb(INK))
            .font_family("Microsoft YaHei UI")
            .text_sm()
            .child(self.sidebar(cx))
            .child(
                div()
                    .v_flex()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .child(
                        div()
                            .h_flex()
                            .items_center()
                            .justify_between()
                            .px_8()
                            .py_5()
                            .border_b_1()
                            .border_color(rgb(BORDER))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(MUTED))
                                    .child("MWB  /  个人工作空间"),
                            )
                            .child(
                                div()
                                    .h_flex()
                                    .items_center()
                                    .gap_2()
                                    .text_xs()
                                    .text_color(rgb(if state.connected { GREEN } else { MUTED }))
                                    .child(
                                        div().size(px(6.)).rounded_full().bg(rgb(
                                            if state.connected { GREEN } else { 0xB7C0B9 },
                                        )),
                                    )
                                    .child(if state.connected {
                                        "设备已连接"
                                    } else if state.reconnecting {
                                        "等待自动重连"
                                    } else if state.listening {
                                        "等待设备连接"
                                    } else {
                                        "尚未连接设备"
                                    }),
                            ),
                    )
                    .when(!self.notice.is_empty(), |el| {
                        el.child(
                            div().px_8().pt_3().flex_shrink_0().child(
                                div()
                                    .h_flex()
                                    .items_center()
                                    .justify_between()
                                    .gap_3()
                                    .rounded_lg()
                                    .px_3()
                                    .py_2()
                                    .bg(rgb(0xEDF3E9))
                                    .text_color(rgb(GREEN))
                                    .text_xs()
                                    .child(div().flex_1().child(self.notice.clone()))
                                    .child(
                                        Button::new("dismiss-notice")
                                            .ghost()
                                            .small()
                                            .icon(assets::IconName::X)
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.notice.clear();
                                                cx.notify();
                                            })),
                                    ),
                            ),
                        )
                    })
                    .child(
                        div()
                            .id("main-scroll")
                            .v_flex()
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .px_8()
                            .py_6()
                            .gap_5()
                            .child(
                                div()
                                    .h_flex()
                                    .items_end()
                                    .justify_between()
                                    .child(
                                        div()
                                            .v_flex()
                                            .gap_2()
                                            .child(
                                                div()
                                                    .text_size(px(27.))
                                                    .font_weight(FontWeight::SEMIBOLD)
                                                    .child(
                                                        [
                                                            "让工作，自由越界。",
                                                            "按你的习惯协作。",
                                                            "无界，从这里开始。",
                                                        ][self.tab],
                                                    ),
                                            )
                                            .child(subtle(
                                                [
                                                    "在相邻的电脑间，自然地移动、输入与协作。",
                                                    "为你的多设备桌面，设置顺手的工作方式。",
                                                    "一个原生、轻量的桌面共享工具。",
                                                ][self.tab],
                                            )),
                                    )
                                    .when(running, |el| {
                                        el.child(
                                            Button::new("stop").ghost().label("停止连接").on_click(
                                                cx.listener(|this, _, _, cx| {
                                                    this.runtime.stop();
                                                    cx.notify();
                                                }),
                                            ),
                                        )
                                    }),
                            )
                            .when(self.tab == 0, |el| {
                                el.child(self.devices(cx))
                                    .child(self.connections(cx))
                                    .child(self.sharing(cx))
                                    .child(self.transfers(cx))
                            })
                            .when(self.tab == 1, |el| el.child(self.preferences(cx)))
                            .when(self.tab == 2, |el| el.child(self.about()))
                            .when(self.tab == 2, |el| {
                                el.child(card().gap_2().child("连接记录").children(
                                    state.events.into_iter().map(|event| {
                                        div().text_xs().text_color(rgb(MUTED)).child(event)
                                    }),
                                ))
                            }),
                    )
                    .child(
                        div()
                            .h_flex()
                            .flex_shrink_0()
                            .justify_between()
                            .gap_4()
                            .px_8()
                            .py_3()
                            .border_t_1()
                            .border_color(rgb(BORDER))
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child(state.detail)
                            .child(format!(
                                "发送 {}  /  接收 {}",
                                self.runtime.sent.load(Ordering::Relaxed),
                                self.runtime.received.load(Ordering::Relaxed)
                            )),
                    ),
            )
    }
}

fn subtle(text: impl Into<SharedString>) -> Div {
    div().text_xs().text_color(rgb(MUTED)).child(text.into())
}

fn card() -> Div {
    div()
        .v_flex()
        .w_full()
        .p_5()
        .rounded_xl()
        .border_1()
        .border_color(rgb(BORDER))
        .bg(gpui_kit::white())
}

fn option_row(title: &'static str, desc: &'static str, control: impl IntoElement) -> Div {
    div()
        .h_flex()
        .items_center()
        .justify_between()
        .gap_5()
        .pb_4()
        .border_b_1()
        .border_color(rgb(BORDER))
        .child(div().v_flex().gap_2().child(title).child(subtle(desc)))
        .child(control)
}

fn main() {
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
    gpui_kit::application()
        .with_assets(assets::AllAssets)
        .run(|cx| {
            gpui_kit::init(cx);
            Theme::change(ThemeMode::Light, None, cx);
            Theme::update(cx, |theme| {
                theme.colors.primary = rgb(GREEN).into();
                theme.colors.button_primary = rgb(GREEN).into();
                theme.colors.button_primary_hover = rgb(0x1B6248).into();
                theme.colors.button_primary_active = rgb(0x164F3A).into();
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
                    size(px(1100.), px(900.)),
                    cx,
                ))),
                window_min_size: Some(size(px(940.), px(720.))),
                titlebar: Some(TitlebarOptions {
                    title: Some("MWB · 无界协作".into()),
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

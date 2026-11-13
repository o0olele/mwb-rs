use super::*;
use gpui_kit::component::switch::Switch;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum SetupStep {
    Choose,
    Host,
    Join,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Panel {
    Settings,
    Invite,
    Transfers,
    Connection,
    Activity,
}

impl Workspace {
    fn open_panel(&mut self, panel: Panel, cx: &mut Context<Self>) {
        self.runtime.return_local();
        self.drag = None;
        self.pan_drag = None;
        if panel == Panel::Settings {
            self.refresh_startup();
        }
        self.panel = Some(panel);
        cx.notify();
    }

    fn close_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.panel = None;
        self.focus.focus(window, cx);
        cx.notify();
    }

    fn select_role(&mut self, step: SetupStep, window: &mut Window, cx: &mut Context<Self>) {
        if self.preparing || self.runtime.running.load(Ordering::SeqCst) {
            return;
        }
        if !self.configured && step == SetupStep::Join && self.setup_step == SetupStep::Choose {
            self.secret
                .update(cx, |state, cx| state.set_value("", window, cx));
        } else if step == SetupStep::Host && self.secret.read(cx).value().trim().is_empty() {
            self.generate_key(window, cx);
        }
        self.setup_step = step;
        self.show_advanced = false;
        self.notice.clear();
        cx.notify();
    }

    fn retry_connection(&mut self, cx: &mut Context<Self>) {
        if self.preparing {
            return;
        }
        let connect = self
            .config
            .saved_connection
            .as_ref()
            .map_or(self.setup_step == SetupStep::Join, |saved| {
                saved.target.is_some()
            });
        if self.secret.read(cx).value().trim().is_empty() {
            self.open_panel(Panel::Connection, cx);
            return;
        }
        if !self.runtime.running.load(Ordering::SeqCst) {
            self.start(connect, cx);
            return;
        }
        self.stop(cx);
        self.preparing = true;
        self.notice.clear();
        let generation = self.start_generation;
        let runtime = self.runtime.clone();
        cx.spawn(async move |this, cx| {
            while runtime.running.load(Ordering::SeqCst) {
                cx.background_executor()
                    .timer(Duration::from_millis(50))
                    .await;
                let cancelled = this.update(cx, |this, _| this.start_generation != generation);
                if !matches!(cancelled, Ok(false)) {
                    return;
                }
            }
            let _ = this.update(cx, |this, cx| {
                if this.start_generation == generation {
                    this.preparing = false;
                    this.start(connect, cx);
                }
            });
        })
        .detach();
        cx.notify();
    }

    fn forget_connection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.preparing || self.runtime.running.load(Ordering::SeqCst) {
            return;
        }
        let previous = self.config.saved_connection.take();
        if let Err(error) = self.config.save() {
            self.config.saved_connection = previous;
            self.notice = format!("无法忘记配对，原配置已保留：{error:#}");
            cx.notify();
            return;
        }
        self.pending_connection = None;
        self.awaiting_connection = false;
        self.configured = false;
        self.setup_step = SetupStep::Choose;
        self.panel = None;
        self.selected.clear();
        self.show_advanced = false;
        self.show_key = false;
        self.secret.update(cx, |state, cx| {
            state.set_value("", window, cx);
            state.set_masked(true, window, cx);
        });
        self.key_length
            .update(cx, |state, cx| state.set_value("64", window, cx));
        self.generate_key(window, cx);
        self.focus.focus(window, cx);
        cx.notify();
    }

    fn pairing_address(&self) -> String {
        if self.runtime.running.load(Ordering::SeqCst) {
            return if self.runtime.snapshot().host {
                self.local_address.clone()
            } else {
                self.config.peer_address.clone()
            };
        }
        self.config
            .saved_connection
            .as_ref()
            .and_then(|saved| saved.target.clone())
            .unwrap_or_else(|| self.local_address.clone())
    }

    fn notice_banner(&self, cx: &mut Context<Self>) -> Div {
        div()
            .h_flex()
            .items_center()
            .gap_3()
            .px_4()
            .py_2()
            .bg(rgb(ACCENT_SOFT))
            .text_color(rgb(INK))
            .text_xs()
            .child(
                Icon::new(assets::IconName::Info)
                    .size_4()
                    .text_color(rgb(ACCENT)),
            )
            .child(div().flex_1().min_w_0().child(self.notice.clone()))
            .child(
                Button::new("dismiss-notice")
                    .ghost()
                    .small()
                    .icon(assets::IconName::X)
                    .tooltip("关闭提示")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.notice.clear();
                        cx.notify();
                    })),
            )
    }

    fn brand_header(&self) -> Div {
        div()
            .h_flex()
            .items_center()
            .gap_3()
            .h(px(50.))
            .flex_shrink_0()
            .px_6()
            .border_b_1()
            .border_color(rgb(BORDER))
            .bg(gpui_kit::white())
            .child(
                div()
                    .size(px(27.))
                    .rounded_lg()
                    .bg(rgb(ACCENT))
                    .text_color(gpui_kit::white())
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(Icon::new(assets::IconName::MousePointer2).size_4()),
            )
            .child(
                div()
                    .text_size(px(23.))
                    .font_weight(FontWeight::BOLD)
                    .child("mwb."),
            )
            .child(subtle("无界桌面"))
            .child(div().flex_1())
            .child(subtle(format!("v{}", env!("CARGO_PKG_VERSION"))))
    }

    fn role_choices(&self, compact: bool, cx: &mut Context<Self>) -> Div {
        div()
            .flex()
            .gap_4()
            .w_full()
            .when(compact, |el| el.flex_col())
            .children(
                [
                    (
                        SetupStep::Host,
                        assets::IconName::MonitorUp,
                        "作为主机",
                        "创建连接，等待其他电脑加入",
                        "创建主机",
                    ),
                    (
                        SetupStep::Join,
                        assets::IconName::Plug,
                        "连接主机",
                        "输入主机地址与配对密钥",
                        "加入连接",
                    ),
                ]
                .into_iter()
                .enumerate()
                .map(|(index, (step, icon, title, desc, action))| {
                    Button::new(("setup-role", index))
                        .flex_1()
                        .w_full()
                        .h(px(if compact { 174. } else { 214. }))
                        .p_5()
                        .rounded_xl()
                        .bg(gpui_kit::white())
                        .border_color(rgb(BORDER))
                        .accessibility_label(format!("{title}，{desc}"))
                        .child(
                            div()
                                .v_flex()
                                .items_start()
                                .w_full()
                                .gap_2()
                                .text_color(rgb(INK))
                                .child(
                                    div()
                                        .size(px(42.))
                                        .rounded_xl()
                                        .bg(rgb(ACCENT_SOFT))
                                        .text_color(rgb(ACCENT))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .child(Icon::new(icon).size_6()),
                                )
                                .child(
                                    div()
                                        .mt_2()
                                        .text_lg()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child(title),
                                )
                                .child(subtle(desc))
                                .child(
                                    div()
                                        .h_flex()
                                        .items_center()
                                        .justify_between()
                                        .w_full()
                                        .mt_3()
                                        .text_xs()
                                        .text_color(rgb(ACCENT))
                                        .child(action)
                                        .child(Icon::new(assets::IconName::ArrowRight).size_4()),
                                ),
                        )
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.select_role(step, window, cx)
                        }))
                }),
            )
    }

    fn connection_form(&self, cx: &mut Context<Self>) -> Div {
        let joining = self.setup_step == SetupStep::Join;
        let running = self.preparing || self.runtime.running.load(Ordering::SeqCst);
        let state = self.runtime.snapshot();
        div()
            .v_flex()
            .gap_5()
            .w_full()
            .child(
                Button::new("setup-back")
                    .ghost()
                    .small()
                    .label("返回")
                    .icon(assets::IconName::ArrowLeft)
                    .disabled(running)
                    .self_start()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.setup_step = SetupStep::Choose;
                        this.notice.clear();
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .v_flex()
                    .gap_2()
                    .child(
                        div()
                            .text_size(px(27.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(if joining {
                                "连接主机"
                            } else {
                                "作为主机"
                            }),
                    )
                    .child(subtle(if joining {
                        "在主机的「添加设备」中获取连接信息。"
                    } else {
                        "其他电脑使用相同密钥，连接到这台设备。"
                    })),
            )
            .when(!joining, |el| {
                el.child(
                    div()
                        .h_flex()
                        .items_center()
                        .gap_2()
                        .child(
                            Icon::new(assets::IconName::Monitor)
                                .size_4()
                                .text_color(rgb(MUTED)),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .child(self.config.name.clone()),
                        )
                        .child(subtle("本机")),
                )
            })
            .when(joining, |el| {
                el.child(
                    div()
                        .v_flex()
                        .gap_2()
                        .child("主机地址")
                        .child(Input::new(&self.address).disabled(running)),
                )
            })
            .child(
                div()
                    .v_flex()
                    .gap_2()
                    .child(
                        div()
                            .h_flex()
                            .justify_between()
                            .items_center()
                            .child("配对密钥")
                            .when(!joining, |el| el.child(subtle("可自定义 · 1–64 个字符"))),
                    )
                    .child(
                        div()
                            .h_flex()
                            .items_center()
                            .gap_2()
                            .child(
                                Input::new(&self.secret)
                                    .flex_1()
                                    .min_w_0()
                                    .disabled(running),
                            )
                            .child(self.key_visibility_button(cx)),
                    ),
            )
            .child(
                Button::new("setup-advanced")
                    .ghost()
                    .small()
                    .self_start()
                    .icon(if self.show_advanced {
                        assets::IconName::ChevronUp
                    } else {
                        assets::IconName::ChevronDown
                    })
                    .label("高级设置")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.show_advanced = !this.show_advanced;
                        cx.notify();
                    })),
            )
            .when(self.show_advanced, |el| {
                el.child(
                    div()
                        .v_flex()
                        .gap_3()
                        .child(
                            div()
                                .v_flex()
                                .gap_2()
                                .child(subtle("连接端口"))
                                .child(Input::new(&self.port).disabled(running)),
                        )
                        .when(!joining, |el| {
                            el.child(
                                div()
                                    .h_flex()
                                    .items_center()
                                    .gap_2()
                                    .child(subtle("随机密钥长度"))
                                    .child(
                                        Input::new(&self.key_length)
                                            .small()
                                            .w(px(56.))
                                            .disabled(running),
                                    )
                                    .child(
                                        Button::new("generate-key")
                                            .small()
                                            .label("重新生成")
                                            .disabled(running)
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.generate_key(window, cx)
                                            })),
                                    ),
                            )
                        }),
                )
            })
            .when(running, |el| {
                el.child(
                    div()
                        .v_flex()
                        .gap_2()
                        .p_3()
                        .rounded_lg()
                        .bg(rgb(ACCENT_SOFT))
                        .child(subtle(if self.preparing {
                            "正在准备配对…"
                        } else {
                            &state.detail
                        }))
                        .child(
                            Button::new("cancel-setup")
                                .small()
                                .ghost()
                                .label("停止连接")
                                .self_start()
                                .on_click(cx.listener(|this, _, _, cx| this.stop(cx))),
                        ),
                )
            })
            .child(
                Button::new("start-setup")
                    .primary()
                    .w_full()
                    .h(px(44.))
                    .label(if joining {
                        "连接并记住"
                    } else {
                        "创建主机"
                    })
                    .icon(assets::IconName::ArrowRight)
                    .loading(running)
                    .disabled(running)
                    .on_click(cx.listener(move |this, _, _, cx| this.start(joining, cx))),
            )
            .child(div().text_center().child(subtle(if joining {
                "连接成功后，下次打开将直接进入设备画布。"
            } else {
                "创建后可查看主机地址，并邀请其他电脑。"
            })))
    }

    fn key_visibility_button(&self, cx: &mut Context<Self>) -> Button {
        Button::new("show-key")
            .ghost()
            .small()
            .icon(if self.show_key {
                assets::IconName::EyeOff
            } else {
                assets::IconName::Eye
            })
            .tooltip(if self.show_key {
                "隐藏配对密钥"
            } else {
                "显示配对密钥"
            })
            .on_click(cx.listener(|this, _, window, cx| {
                this.show_key = !this.show_key;
                this.secret
                    .update(cx, |state, cx| state.set_masked(!this.show_key, window, cx));
                cx.notify();
            }))
    }

    fn onboarding(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        div()
            .id("onboarding-scroll")
            .v_flex()
            .items_center()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px_6()
            .py_6()
            .bg(rgb(BACKGROUND))
            .child(div().flex_1().min_h(px(24.)))
            .child(
                div()
                    .w_full()
                    .max_w(px(if self.setup_step == SetupStep::Choose {
                        580.
                    } else {
                        400.
                    }))
                    .when(self.setup_step == SetupStep::Choose, |el| {
                        el.child(
                            div()
                                .v_flex()
                                .gap_3()
                                .items_center()
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(rgb(ACCENT))
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child("开始连接"),
                                )
                                .child(
                                    div()
                                        .text_size(px(32.))
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child("一套键鼠，多台电脑。"),
                                )
                                .child(subtle("这台电脑将以什么方式连接？"))
                                .child(div().mt_5().w_full().child(self.role_choices(false, cx)))
                                .child(
                                    div()
                                        .h_flex()
                                        .items_center()
                                        .gap_2()
                                        .mt_3()
                                        .text_color(rgb(MUTED))
                                        .child(Icon::new(assets::IconName::LockKeyhole).size_3())
                                        .child(subtle("加密连接 · 配置保存在本机")),
                                ),
                        )
                    })
                    .when(self.setup_step != SetupStep::Choose, |el| {
                        el.child(self.connection_form(cx))
                    }),
            )
            .child(div().flex_1().min_h(px(32.)))
            .child(subtle("首次配置一次，下次打开自动连接。"))
    }

    fn workspace_header(&self, cx: &mut Context<Self>) -> Div {
        let state = self.runtime.snapshot();
        let running = self.runtime.running.load(Ordering::SeqCst);
        let enabled = self.runtime.sharing_enabled();
        let count = state
            .layout
            .devices
            .iter()
            .filter(|device| device.online)
            .count();
        let (label, color, background) = if self.preparing {
            ("正在准备连接…".into(), ACCENT, ACCENT_SOFT)
        } else if state.connected {
            (format!("{count} 台在线"), GREEN, 0xEAF6F0)
        } else if state.reconnecting {
            ("主机离线 · 自动重连".into(), WARNING, 0xFFF7E8)
        } else if state.listening {
            ("主机已就绪 · 等待加入".into(), GREEN, 0xEAF6F0)
        } else if running {
            ("正在连接主机…".into(), ACCENT, ACCENT_SOFT)
        } else {
            ("连接已停止".into(), MUTED, BACKGROUND)
        };
        div()
            .h_flex()
            .items_center()
            .justify_between()
            .flex_shrink_0()
            .gap_4()
            .px_6()
            .py_4()
            .bg(gpui_kit::white())
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap_4()
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("设备画布"),
                    )
                    .child(
                        div()
                            .h_flex()
                            .items_center()
                            .gap_2()
                            .px_3()
                            .py_1()
                            .rounded_full()
                            .text_size(px(11.))
                            .text_color(rgb(color))
                            .bg(rgb(background))
                            .child(status_dot(color))
                            .child(label),
                    ),
            )
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new("toggle-sharing")
                            .ghost()
                            .small()
                            .icon(if enabled {
                                assets::IconName::Pause
                            } else {
                                assets::IconName::Play
                            })
                            .label(if enabled {
                                "暂停共享"
                            } else {
                                "恢复共享"
                            })
                            .disabled(!state.connected || self.panel.is_some())
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_sharing(cx))),
                    )
                    .child(
                        Button::new("invite-device")
                            .primary()
                            .small()
                            .icon(assets::IconName::Plus)
                            .label("添加设备")
                            .disabled(
                                !state.established
                                    || !running
                                    || self.preparing
                                    || self.panel.is_some(),
                            )
                            .on_click(
                                cx.listener(|this, _, _, cx| this.open_panel(Panel::Invite, cx)),
                            ),
                    )
                    .child(
                        Button::new("open-settings")
                            .ghost()
                            .small()
                            .icon(assets::IconName::Settings2)
                            .tooltip("设置")
                            .disabled(self.panel.is_some())
                            .on_click(
                                cx.listener(|this, _, _, cx| this.open_panel(Panel::Settings, cx)),
                            ),
                    ),
            )
    }

    fn connection_banner(&self, cx: &mut Context<Self>) -> Div {
        let state = self.runtime.snapshot();
        let running = self.runtime.running.load(Ordering::SeqCst) || self.preparing;
        let title = if self.preparing {
            "正在准备连接"
        } else if state.reconnecting {
            "正在重新连接主机"
        } else if running {
            "正在连接主机"
        } else {
            "连接已停止"
        };
        div()
            .h_flex()
            .items_center()
            .gap_3()
            .mx_6()
            .mb_3()
            .px_4()
            .py_3()
            .rounded_lg()
            .bg(rgb(0xFFF7E8))
            .border_1()
            .border_color(rgb(0xF4E7CC))
            .child(
                Icon::new(assets::IconName::RefreshCw)
                    .size_4()
                    .text_color(rgb(WARNING)),
            )
            .child(
                div()
                    .v_flex()
                    .gap_1()
                    .flex_1()
                    .min_w_0()
                    .child(div().text_xs().child(title))
                    .child(subtle(if running {
                        "保留当前布局，连接成功后自动恢复。"
                    } else {
                        "配对与布局已保留，可重新连接或在设置中更改。"
                    })),
            )
            .child(
                Button::new("retry-connection")
                    .small()
                    .label(if state.reconnecting {
                        "立即重试"
                    } else if running {
                        "连接设置"
                    } else {
                        "重新连接"
                    })
                    .disabled(self.preparing || self.panel.is_some())
                    .on_click(cx.listener(|this, _, _, cx| {
                        let state = this.runtime.snapshot();
                        if this.runtime.running.load(Ordering::SeqCst) && !state.reconnecting {
                            this.open_panel(Panel::Connection, cx);
                        } else {
                            this.retry_connection(cx);
                        }
                    })),
            )
    }

    fn device_canvas(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let state = self.runtime.snapshot();
        let invalid = self
            .layout_draft
            .validate()
            .err()
            .map(|error| error.to_string());
        let bounds = self.canvas_bounds.clone();
        let mut area = div()
            .id("layout-canvas")
            .relative()
            .flex_1()
            .min_h(px(260.))
            .w_full()
            .overflow_hidden()
            .bg(rgb(BACKGROUND))
            .border_y_1()
            .border_color(rgb(BORDER))
            .child(
                canvas(
                    move |area, _, _| {
                        *bounds.lock().unwrap() = area;
                    },
                    |bounds, _, window, _| {
                        let width = f32::from(bounds.size.width);
                        let height = f32::from(bounds.size.height);
                        for row in 0..(height / 24.).ceil() as i32 {
                            for column in 0..(width / 24.).ceil() as i32 {
                                let origin = bounds.origin
                                    + point(
                                        px(column as f32 * 24. + 12.),
                                        px(row as f32 * 24. + 12.),
                                    );
                                window.paint_quad(
                                    fill(
                                        Bounds::new(origin, size(px(1.5), px(1.5))),
                                        rgb(0xDCE1EB),
                                    )
                                    .corner_radii(px(1.)),
                                );
                            }
                        }
                    },
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
                    if this.panel.is_none() {
                        this.pan_drag = Some((event.position, this.view_offset));
                    }
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

        for device in &self.layout_draft.devices {
            let x = self.view_offset.x + device.x as f64 * self.view_scale;
            let y = self.view_offset.y + device.y as f64 * self.view_scale;
            let selected = self.selected == device.info.id;
            let online = state
                .layout
                .get(&device.info.id)
                .is_some_and(|device| device.online);
            let local = device.info.id == self.runtime.local.id;
            let host = if state.host { local } else { false };
            for (index, screen) in device.info.screens.iter().enumerate() {
                let screen_x = x + (screen.rect.x - device.info.bounds.x) as f64 * self.view_scale;
                let screen_y = y + (screen.rect.y - device.info.bounds.y) as f64 * self.view_scale;
                let width = screen.rect.width as f64 * self.view_scale;
                let height = screen.rect.height as f64 * self.view_scale;
                let id = device.info.id.clone();
                let origin = (device.x, device.y);
                let border = if invalid.is_some() {
                    0xC85858
                } else if selected || local {
                    ACCENT
                } else if online {
                    0xB4BECF
                } else {
                    0xD1D6E0
                };
                let screen_element = div()
                    .id(format!("screen-{id}-{index}"))
                    .absolute()
                    .left(px(screen_x as f32))
                    .top(px(screen_y as f32))
                    .w(px(width as f32))
                    .h(px(height as f32))
                    .rounded_lg()
                    .border_1()
                    .when(selected, |el| el.border_2())
                    .border_color(rgb(border))
                    .bg(rgb(if !online {
                        0xF0F2F6
                    } else if local {
                        0xF3F3FE
                    } else {
                        0xFCFCFE
                    }))
                    .text_color(rgb(if online { INK } else { MUTED }))
                    .overflow_hidden()
                    .cursor_move()
                    .v_flex()
                    .items_center()
                    .justify_center()
                    .gap_1()
                    .p_2()
                    .when(width > 160. && height > 95., |el| {
                        el.child(
                            div()
                                .absolute()
                                .top_2()
                                .left_2()
                                .right_2()
                                .h_flex()
                                .items_center()
                                .justify_between()
                                .child(div().text_size(px(11.)).text_color(rgb(ACCENT)).child(
                                    if host {
                                        "本机 · 主机"
                                    } else if local {
                                        "本机"
                                    } else {
                                        ""
                                    },
                                ))
                                .child(
                                    div()
                                        .h_flex()
                                        .items_center()
                                        .gap_1()
                                        .text_size(px(11.))
                                        .text_color(rgb(MUTED))
                                        .child(status_dot(if online { GREEN } else { MUTED }))
                                        .child(if online { "在线" } else { "离线" }),
                                ),
                        )
                    })
                    .when(width > 145. && height > 110., |el| {
                        el.child(
                            Icon::new(if online {
                                assets::IconName::Monitor
                            } else {
                                assets::IconName::MonitorOff
                            })
                            .size_6()
                            .text_color(rgb(if local {
                                ACCENT
                            } else {
                                MUTED
                            })),
                        )
                    })
                    .when(width > 75. && height > 35., |el| {
                        el.child(
                            div()
                                .w_full()
                                .text_center()
                                .truncate()
                                .text_size(px(if width > 240. { 14. } else { 11. }))
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(device.info.name.clone()),
                        )
                    })
                    .when(width > 95. && height > 65., |el| {
                        el.child(
                            div()
                                .text_size(px(11.))
                                .text_color(rgb(MUTED))
                                .child(format!("{} × {}", screen.rect.width, screen.rect.height)),
                        )
                    })
                    .when(height > 65., |el| {
                        el.child(
                            div()
                                .absolute()
                                .bottom_1()
                                .w(px(25.))
                                .h(px(3.))
                                .rounded_full()
                                .bg(rgb(BORDER)),
                        )
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                            if this.panel.is_some() {
                                return;
                            }
                            this.runtime.return_local();
                            this.selected = id.clone();
                            this.drag = Some(CanvasDrag {
                                id: id.clone(),
                                start: event.position,
                                x: origin.0,
                                y: origin.1,
                                scale: this.view_scale,
                                moved: false,
                            });
                            cx.stop_propagation();
                            cx.notify();
                        }),
                    );
                area = area.child(screen_element);
                if width > 110. {
                    area = area.child(
                        div()
                            .absolute()
                            .left(px(screen_x as f32))
                            .top(px((screen_y + height + 9.) as f32))
                            .w(px(width as f32))
                            .text_center()
                            .truncate()
                            .text_size(px(11.))
                            .text_color(rgb(MUTED))
                            .child(if !online {
                                "离线 · 保留位置".into()
                            } else if device.info.screens.len() > 1 {
                                format!(
                                    "显示器 {}{}",
                                    index + 1,
                                    if screen.primary { " · 主屏" } else { "" }
                                )
                            } else if local {
                                "本机显示器".into()
                            } else {
                                "在线设备".into()
                            }),
                    );
                }
            }
        }
        area.child(
            div()
                .absolute()
                .top_4()
                .left_6()
                .right_6()
                .h_flex()
                .items_center()
                .justify_between()
                .gap_3()
                .child(subtle(if state.listening && !state.connected {
                    "主机已就绪，点击「添加设备」邀请其他电脑"
                } else {
                    "拖动屏幕，按桌面上的实际位置排列 · 中键拖动画布"
                }))
                .child(
                    div()
                        .h_flex()
                        .items_center()
                        .gap_1()
                        .text_size(px(11.))
                        .text_color(rgb(if invalid.is_some() { 0xC85858 } else { MUTED }))
                        .when(!self.layout_dirty, |el| {
                            el.child(
                                Icon::new(assets::IconName::Check)
                                    .size_3()
                                    .text_color(rgb(GREEN)),
                            )
                        })
                        .child(if invalid.is_some() {
                            "屏幕重叠，请调整位置"
                        } else if self.layout_dirty {
                            "布局有未保存的更改"
                        } else {
                            "布局已同步"
                        }),
                ),
        )
        .when(!self.selected.is_empty() && self.drag.is_none(), |el| {
            el.child(
                div()
                    .absolute()
                    .bottom(px(76.))
                    .left_6()
                    .right_6()
                    .flex()
                    .justify_center()
                    .child(self.selection_bar(cx)),
            )
        })
        .child(
            div()
                .absolute()
                .bottom_4()
                .left_6()
                .right_6()
                .h_flex()
                .items_center()
                .justify_between()
                .gap_4()
                .child(self.canvas_tools(cx))
                .when(self.layout_dirty, |el| {
                    el.child(
                        div()
                            .h_flex()
                            .gap_2()
                            .p_1()
                            .rounded_lg()
                            .bg(gpui_kit::white())
                            .border_1()
                            .border_color(rgb(BORDER))
                            .child(
                                Button::new("restore-layout")
                                    .small()
                                    .ghost()
                                    .label("还原")
                                    .disabled(self.panel.is_some())
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
                                    .icon(assets::IconName::Check)
                                    .disabled(invalid.is_some() || self.panel.is_some())
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
                                            Err(error) => this.notice = format!("{error:#}"),
                                        }
                                        cx.notify();
                                    })),
                            ),
                    )
                })
                .when(!self.layout_dirty, |el| {
                    el.child(subtle("移过屏幕边缘，切换控制"))
                }),
        )
    }

    fn canvas_tools(&self, cx: &mut Context<Self>) -> Div {
        div()
            .h_flex()
            .items_center()
            .gap_1()
            .p_1()
            .rounded_lg()
            .bg(gpui_kit::white())
            .border_1()
            .border_color(rgb(BORDER))
            .child(
                Button::new("zoom-out")
                    .small()
                    .ghost()
                    .icon(assets::IconName::Minus)
                    .tooltip("缩小画布")
                    .disabled(self.panel.is_some())
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.zoom_canvas(0.8);
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .w(px(42.))
                    .text_center()
                    .text_size(px(11.))
                    .text_color(rgb(MUTED))
                    .child(format!("{:.0}%", self.view_scale * 100.)),
            )
            .child(
                Button::new("zoom-in")
                    .small()
                    .ghost()
                    .icon(assets::IconName::Plus)
                    .tooltip("放大画布")
                    .disabled(self.panel.is_some())
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.zoom_canvas(1.25);
                        cx.notify();
                    })),
            )
            .child(div().w(px(1.)).h(px(16.)).mx_1().bg(rgb(BORDER)))
            .child(
                Button::new("fit-canvas")
                    .small()
                    .ghost()
                    .icon(assets::IconName::Scan)
                    .tooltip("适应画布")
                    .disabled(self.panel.is_some())
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.fit_canvas();
                        cx.notify();
                    })),
            )
    }

    fn selection_bar(&self, cx: &mut Context<Self>) -> Div {
        let state = self.runtime.snapshot();
        let device = state.layout.get(&self.selected);
        let local = self.selected == self.runtime.local.id;
        let online = device.is_some_and(|device| device.online);
        let can_remove = !self.runtime.running.load(Ordering::SeqCst) || state.host;
        div()
            .h_flex()
            .items_center()
            .gap_5()
            .px_4()
            .py_2()
            .rounded_xl()
            .bg(gpui_kit::white())
            .border_1()
            .border_color(rgb(BORDER))
            .shadow_md()
            .child(
                Icon::new(assets::IconName::Monitor)
                    .size_4()
                    .text_color(rgb(MUTED)),
            )
            .child(
                div()
                    .v_flex()
                    .gap_1()
                    .max_w(px(260.))
                    .child(
                        div()
                            .truncate()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(
                                device
                                    .map(|device| device.info.name.clone())
                                    .unwrap_or_default(),
                            ),
                    )
                    .child(subtle(if local {
                        "本机"
                    } else if online {
                        "在线 · 可共享键鼠与文件"
                    } else if !can_remove {
                        "离线 · 可在主机上移除此设备"
                    } else {
                        "离线 · 已保留设备位置"
                    })),
            )
            .when(!local && online, |el| {
                el.child(
                    Button::new("control-device")
                        .small()
                        .label("控制设备")
                        .disabled(
                            !state.connected
                                || !self.runtime.sharing_enabled()
                                || self.runtime.remote.load(Ordering::SeqCst)
                                || self.runtime.incoming.load(Ordering::SeqCst)
                                || self.panel.is_some(),
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Err(error) = this.runtime.control(&this.selected) {
                                this.notice = format!("无法切换控制：{error:#}");
                            }
                            cx.notify();
                        })),
                )
                .child(
                    Button::new("send-clipboard")
                        .small()
                        .ghost()
                        .icon(assets::IconName::Clipboard)
                        .tooltip("发送剪贴板")
                        .disabled(
                            !self.runtime.exchange.ready()
                                || !self.runtime.sharing_enabled()
                                || self.panel.is_some(),
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.copy_clipboard(cx))),
                )
                .child(
                    Button::new("send-files-panel")
                        .small()
                        .ghost()
                        .icon(assets::IconName::Send)
                        .tooltip("发送文件与传输记录")
                        .disabled(self.panel.is_some())
                        .on_click(
                            cx.listener(|this, _, _, cx| this.open_panel(Panel::Transfers, cx)),
                        ),
                )
            })
            .when(!local && !online && can_remove, |el| {
                el.child(
                    Button::new("remove-device")
                        .small()
                        .ghost()
                        .icon(assets::IconName::Trash)
                        .tooltip("移除离线设备")
                        .disabled(
                            self.layout_dirty
                                || self.preparing
                                || self.drag.is_some()
                                || self.panel.is_some(),
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.notice = match this
                                .runtime
                                .remove_offline(this.layout_revision, &this.selected)
                            {
                                Ok(()) => "移除请求已提交，设备列表更新后自动保存。".into(),
                                Err(error) => format!("{error:#}"),
                            };
                            this.refresh_layout();
                            cx.notify();
                        })),
                )
            })
            .child(
                Button::new("deselect-device")
                    .small()
                    .ghost()
                    .icon(assets::IconName::X)
                    .tooltip("取消选择")
                    .disabled(self.panel.is_some())
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.selected.clear();
                        cx.notify();
                    })),
            )
    }

    fn status_bar(&self, cx: &mut Context<Self>) -> Div {
        let state = self.runtime.snapshot();
        let enabled = self.runtime.sharing_enabled();
        let sharing = if self.runtime.remote.load(Ordering::SeqCst) {
            "正在控制远端"
        } else if self.runtime.incoming.load(Ordering::SeqCst) {
            "正在接收控制"
        } else if state.connected && enabled {
            "键鼠共享中"
        } else if state.connected {
            "共享已暂停"
        } else if state.listening {
            "等待设备连接"
        } else if state.reconnecting {
            "等待主机恢复"
        } else {
            "共享未连接"
        };
        let clipboard = if !self.config.auto_clipboard {
            "剪贴板同步已关闭"
        } else if !state.connected || !enabled {
            "剪贴板同步已暂停"
        } else if !self.runtime.exchange.ready() {
            "等待剪贴板通道"
        } else {
            "剪贴板同步已开启"
        };
        let active = self
            .runtime
            .exchange
            .snapshot()
            .iter()
            .filter(|transfer| !transfer.phase.terminal())
            .count();
        div()
            .h_flex()
            .items_center()
            .flex_wrap()
            .gap_5()
            .px_6()
            .py_3()
            .flex_shrink_0()
            .bg(gpui_kit::white())
            .text_size(px(11.))
            .text_color(rgb(MUTED))
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap_2()
                    .text_color(rgb(if enabled { GREEN } else { MUTED }))
                    .child(status_dot(if enabled { GREEN } else { MUTED }))
                    .child(sharing),
            )
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap_2()
                    .child(Icon::new(assets::IconName::Clipboard).size_3())
                    .child(clipboard),
            )
            .child(
                Button::new("show-transfers")
                    .ghost()
                    .small()
                    .label(if active == 0 {
                        "传输记录".into()
                    } else {
                        format!("传输记录 ({active})")
                    })
                    .icon(assets::IconName::ArrowUpRight)
                    .disabled(self.panel.is_some())
                    .on_click(cx.listener(|this, _, _, cx| this.open_panel(Panel::Transfers, cx))),
            )
            .child(div().flex_1())
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(BORDER))
                            .bg(rgb(BACKGROUND))
                            .child("Ctrl + Alt + Home"),
                    )
                    .child("返回本机"),
            )
    }

    fn workspace(&self, cx: &mut Context<Self>) -> Div {
        let state = self.runtime.snapshot();
        div()
            .v_flex()
            .flex_1()
            .min_h_0()
            .w_full()
            .bg(gpui_kit::white())
            .child(self.workspace_header(cx))
            .when(!state.connected && !state.listening, |el| {
                el.child(self.connection_banner(cx))
            })
            .child(self.device_canvas(cx))
            .child(self.status_bar(cx))
    }

    fn invitation(&self, cx: &mut Context<Self>) -> Div {
        let address = self.pairing_address();
        div()
            .v_flex()
            .gap_5()
            .child(
                div()
                    .size(px(44.))
                    .rounded_xl()
                    .bg(rgb(ACCENT_SOFT))
                    .text_color(rgb(ACCENT))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(Icon::new(assets::IconName::MonitorUp).size_6()),
            )
            .child(
                div()
                    .v_flex()
                    .gap_2()
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("让另一台电脑加入"),
                    )
                    .child(subtle(
                        "在另一台电脑打开 MWB，选择「连接主机」，填写以下信息。",
                    )),
            )
            .child(
                div().v_flex().gap_2().child("主机地址").child(
                    div()
                        .px_3()
                        .py_3()
                        .rounded_lg()
                        .border_1()
                        .border_color(rgb(BORDER))
                        .child(address),
                ),
            )
            .child(
                div().v_flex().gap_2().child("配对密钥").child(
                    div()
                        .h_flex()
                        .gap_2()
                        .items_center()
                        .child(Input::new(&self.secret).flex_1().min_w_0().disabled(true))
                        .child(self.key_visibility_button(cx)),
                ),
            )
            .child(
                Button::new("copy-invitation")
                    .primary()
                    .w_full()
                    .icon(assets::IconName::Copy)
                    .label("复制连接信息")
                    .on_click(cx.listener(|this, _, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(format!(
                            "主机地址：{}\n端口：{}\n配对密钥：{}",
                            this.pairing_address(),
                            this.config.port,
                            this.secret.read(cx).value()
                        )));
                        this.notice =
                            "已复制连接信息，请在另一台电脑分别填写地址和配对密钥。".into();
                        cx.notify();
                    })),
            )
            .child(
                Button::new("copy-pairing-key")
                    .ghost()
                    .w_full()
                    .label("仅复制配对密钥")
                    .on_click(cx.listener(|this, _, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(
                            this.secret.read(cx).value().to_string(),
                        ));
                        this.notice = "已复制配对密钥".into();
                        cx.notify();
                    })),
            )
            .child(subtle(format!(
                "连接端口 {} · 同一局域网 · 最多 16 台设备",
                self.config.port
            )))
    }

    fn preferences(&self, cx: &mut Context<Self>) -> Div {
        let state = self.runtime.snapshot();
        let running = self.preparing || self.runtime.running.load(Ordering::SeqCst);
        div()
            .v_flex()
            .gap_3()
            .child(section_heading("启动与连接"))
            .child(option_row(
                "开机自启动",
                "登录 Windows 后启动",
                Switch::new("autostart")
                    .small()
                    .accessibility_label("开机自启动")
                    .checked(self.startup_status == Some(startup::Status::Enabled))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(status) = this.startup_status {
                            this.apply_startup_result(startup::set_enabled(
                                status != startup::Status::Enabled,
                            ));
                        } else {
                            this.refresh_startup();
                        }
                        cx.notify();
                    })),
            ))
            .when(
                self.startup_status == Some(startup::Status::OtherExecutable),
                |el| {
                    el.child(
                        Button::new("startup-disable")
                            .small()
                            .label("关闭其他位置的启动项")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.apply_startup_result(startup::set_enabled(false));
                                cx.notify();
                            })),
                    )
                },
            )
            .when(!self.startup_notice.is_empty(), |el| {
                el.child(subtle(self.startup_notice.clone()))
            })
            .child(option_row(
                "启动时自动连接",
                "恢复上次的主机与配对",
                Switch::new("auto-connect")
                    .small()
                    .accessibility_label("启动时自动连接")
                    .checked(self.config.auto_connect)
                    .on_click(cx.listener(|this, value, _, cx| {
                        this.config.auto_connect = *value;
                        this.persist();
                        cx.notify();
                    })),
            ))
            .child(option_row(
                "断线自动重连",
                "主机上线后自动恢复",
                Switch::new("auto-reconnect")
                    .small()
                    .accessibility_label("断线自动重连")
                    .checked(self.config.auto_reconnect)
                    .on_click(cx.listener(|this, value, _, cx| {
                        this.config.auto_reconnect = *value;
                        this.runtime.auto_reconnect.store(*value, Ordering::SeqCst);
                        this.persist();
                        cx.notify();
                    })),
            ))
            .child(div().mt_3().child(section_heading("共享偏好")))
            .child(option_row(
                "屏幕边缘切换",
                "按屏幕排列和鼠标方向切换",
                Switch::new("edge-switch")
                    .small()
                    .accessibility_label("屏幕边缘切换")
                    .checked(self.config.edge_switch)
                    .on_click(cx.listener(|this, value, _, cx| {
                        this.config.edge_switch = *value;
                        this.runtime.edge.store(*value, Ordering::SeqCst);
                        this.persist();
                        cx.notify();
                    })),
            ))
            .child(
                div().v_flex().gap_2().child(subtle("切换条件")).child(
                    div().h_flex().flex_wrap().gap_2().children(
                        [
                            (EdgeSwitchCondition::Always, "直接切换"),
                            (EdgeSwitchCondition::Ctrl, "按住 Ctrl"),
                            (EdgeSwitchCondition::Shift, "按住 Shift"),
                        ]
                        .into_iter()
                        .enumerate()
                        .map(|(index, (condition, label))| {
                            Button::new(("edge-condition", index))
                                .small()
                                .label(label)
                                .when(self.config.edge_switch_condition == condition, |button| {
                                    button.primary()
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
                ),
            )
            .child(option_row(
                "共享键盘",
                "键盘跟随当前鼠标目标",
                Switch::new("share-keyboard")
                    .small()
                    .accessibility_label("共享键盘")
                    .checked(self.config.keyboard)
                    .on_click(cx.listener(|this, value, _, cx| {
                        this.runtime.return_local();
                        this.config.keyboard = *value;
                        this.runtime.keyboard.store(*value, Ordering::SeqCst);
                        this.persist();
                        cx.notify();
                    })),
            ))
            .child(option_row(
                "同步剪贴板",
                "文本、图片和文件列表",
                Switch::new("share-clipboard")
                    .small()
                    .accessibility_label("同步剪贴板")
                    .checked(self.config.auto_clipboard)
                    .on_click(cx.listener(|this, value, _, cx| {
                        this.config.auto_clipboard = *value;
                        this.runtime
                            .exchange
                            .auto_clipboard
                            .store(*value, Ordering::SeqCst);
                        this.persist();
                        cx.notify();
                    })),
            ))
            .child(
                Button::new("advanced-preferences")
                    .small()
                    .ghost()
                    .self_start()
                    .icon(if self.show_advanced_preferences {
                        assets::IconName::ChevronUp
                    } else {
                        assets::IconName::ChevronDown
                    })
                    .label("高级偏好")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.show_advanced_preferences = !this.show_advanced_preferences;
                        cx.notify();
                    })),
            )
            .when(self.show_advanced_preferences, |el| {
                el.child(self.advanced_preferences(cx))
            })
            .child(div().mt_3().child(section_heading("当前连接")))
            .child(
                div()
                    .v_flex()
                    .gap_2()
                    .child(
                        if self
                            .config
                            .saved_connection
                            .as_ref()
                            .is_some_and(|saved| saved.target.is_some())
                        {
                            "连接主机"
                        } else {
                            "作为主机"
                        },
                    )
                    .child(subtle(format!(
                        "{} : {}",
                        self.pairing_address(),
                        self.config.port
                    )))
                    .child(subtle(state.detail))
                    .child(
                        div()
                            .h_flex()
                            .flex_wrap()
                            .gap_2()
                            .mt_2()
                            .child(
                                Button::new("stop-or-start")
                                    .small()
                                    .label(if running {
                                        "停止连接"
                                    } else {
                                        "重新连接"
                                    })
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        if this.preparing
                                            || this.runtime.running.load(Ordering::SeqCst)
                                        {
                                            this.stop(cx);
                                        } else {
                                            this.retry_connection(cx);
                                        }
                                    })),
                            )
                            .child(
                                Button::new("reconfigure")
                                    .small()
                                    .ghost()
                                    .label("重新配置")
                                    .disabled(running)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.setup_step = SetupStep::Choose;
                                        this.show_advanced = false;
                                        this.open_panel(Panel::Connection, cx);
                                    })),
                            )
                            .child(
                                Button::new("forget-pairing")
                                    .small()
                                    .ghost()
                                    .label("忘记配对")
                                    .disabled(running || self.config.saved_connection.is_none())
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.forget_connection(window, cx)
                                    })),
                            ),
                    ),
            )
            .child(
                Button::new("show-activity")
                    .small()
                    .ghost()
                    .label("关于 MWB 与连接记录")
                    .icon(assets::IconName::ArrowUpRight)
                    .on_click(cx.listener(|this, _, _, cx| this.open_panel(Panel::Activity, cx))),
            )
    }

    fn advanced_preferences(&self, cx: &mut Context<Self>) -> Div {
        div()
            .v_flex()
            .gap_3()
            .child(option_row(
                "相对移动鼠标",
                "使用原生相对位移，切换后生效",
                Switch::new("relative-mouse")
                    .small()
                    .accessibility_label("相对移动鼠标")
                    .checked(self.config.relative_mouse)
                    .on_click(cx.listener(|this, value, _, cx| {
                        this.runtime.return_local();
                        this.config.relative_mouse = *value;
                        this.runtime.relative_mouse.store(*value, Ordering::SeqCst);
                        this.persist();
                        cx.notify();
                    })),
            ))
            .child(option_row(
                "避开屏幕角落",
                "屏幕四角 100 像素内不自动切换",
                Switch::new("block-corners")
                    .small()
                    .accessibility_label("避开屏幕角落")
                    .checked(self.config.block_corners)
                    .on_click(cx.listener(|this, value, _, cx| {
                        this.config.block_corners = *value;
                        this.runtime.block_corners.store(*value, Ordering::SeqCst);
                        this.persist();
                        cx.notify();
                    })),
            ))
            .child(
                div()
                    .v_flex()
                    .gap_2()
                    .child(subtle("文件传输速率 · MiB/s"))
                    .child(div().h_flex().gap_1().flex_wrap().children(
                        [1, 4, 8, 16, 32, 0].into_iter().map(|rate| {
                            Button::new(format!("transfer-rate-{rate}"))
                                .small()
                                .label(if rate == 0 {
                                    "不限速".into()
                                } else {
                                    format!("{rate}")
                                })
                                .when(rate == self.config.transfer_rate_mib, |button| {
                                    button.primary()
                                })
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.config.transfer_rate_mib = rate;
                                    this.runtime.exchange.set_rate_mib(rate);
                                    this.persist();
                                    cx.notify();
                                }))
                        }),
                    )),
            )
            .child(
                Button::new("settings-receive-folder")
                    .small()
                    .label("设置接收文件夹")
                    .on_click(cx.listener(|this, _, _, cx| this.choose_files(true, cx))),
            )
            .child(subtle(self.config.receive_directory.display().to_string()))
            .when(!self.input_service_notice.is_empty(), |el| {
                el.child(subtle(self.input_service_notice.clone()))
            })
    }

    fn activity(&self) -> Div {
        let state = self.runtime.snapshot();
        div()
            .v_flex()
            .gap_4()
            .child(
                div()
                    .text_lg()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("一套键鼠，自由连接。"),
            )
            .child(subtle(format!("MWB Rust · v{}", env!("CARGO_PKG_VERSION"))))
            .child(subtle(
                "多设备键鼠共享、自由屏幕布局、自动剪贴板与跨设备文件粘贴。",
            ))
            .child(section_heading("连接记录"))
            .child(subtle(state.detail))
            .children(state.events.into_iter().map(|event| {
                div()
                    .py_2()
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .child(subtle(event))
            }))
            .child(subtle(format!(
                "输入事件 · 发送 {} / 接收 {}",
                self.runtime.sent.load(Ordering::Relaxed),
                self.runtime.received.load(Ordering::Relaxed)
            )))
    }

    fn side_panel(&self, panel: Panel, cx: &mut Context<Self>) -> Div {
        let title = match panel {
            Panel::Settings => "设置",
            Panel::Invite => "添加设备",
            Panel::Transfers => "传输记录",
            Panel::Connection => "连接设置",
            Panel::Activity => "关于与连接记录",
        };
        let body = match panel {
            Panel::Settings => self.preferences(cx).into_any_element(),
            Panel::Invite => self.invitation(cx).into_any_element(),
            Panel::Transfers => self.transfers(cx).into_any_element(),
            Panel::Activity => self.activity().into_any_element(),
            Panel::Connection => div()
                .v_flex()
                .gap_5()
                .when(self.setup_step == SetupStep::Choose, |el| {
                    el.child(subtle("选择这台电脑的连接角色"))
                        .child(self.role_choices(true, cx))
                })
                .when(self.setup_step != SetupStep::Choose, |el| {
                    el.child(self.connection_form(cx))
                })
                .into_any_element(),
        };
        div()
            .absolute()
            .top(px(50.))
            .bottom_0()
            .right_0()
            .w(px(400.))
            .v_flex()
            .bg(gpui_kit::white())
            .border_l_1()
            .border_color(rgb(BORDER))
            .shadow_lg()
            .child(
                div()
                    .h_flex()
                    .justify_between()
                    .items_center()
                    .px_6()
                    .py_4()
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(title),
                    )
                    .child(
                        Button::new("close-panel")
                            .ghost()
                            .small()
                            .icon(assets::IconName::X)
                            .tooltip("关闭面板 · Esc")
                            .on_click(
                                cx.listener(|this, _, window, cx| this.close_panel(window, cx)),
                            ),
                    ),
            )
            .when(!self.notice.is_empty(), |el| {
                el.child(self.notice_banner(cx))
            })
            .child(
                div()
                    .id(("panel-scroll", panel as usize))
                    .v_flex()
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p_6()
                    .child(body),
            )
    }
}

impl Render for Workspace {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .relative()
            .v_flex()
            .size_full()
            .overflow_hidden()
            .bg(rgb(BACKGROUND))
            .text_color(rgb(INK))
            .font_family("Microsoft YaHei UI")
            .text_sm()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" {
                    if this.panel.is_some() {
                        this.close_panel(window, cx);
                    } else {
                        this.selected.clear();
                        cx.notify();
                    }
                }
            }))
            .child(self.brand_header())
            .when(!self.notice.is_empty() && self.panel.is_none(), |el| {
                el.child(self.notice_banner(cx))
            })
            .when(self.configured, |el| el.child(self.workspace(cx)))
            .when(!self.configured, |el| el.child(self.onboarding(cx)))
            .when_some(self.panel, |el, panel| {
                el.child(
                    div()
                        .absolute()
                        .top(px(50.))
                        .bottom_0()
                        .left_0()
                        .right_0()
                        .bg(rgba(0x20253528))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, window, cx| {
                                this.close_panel(window, cx);
                                cx.stop_propagation();
                            }),
                        ),
                )
                .child(self.side_panel(panel, cx))
            })
    }
}

fn status_dot(color: u32) -> Div {
    div()
        .size(px(6.))
        .flex_shrink_0()
        .rounded_full()
        .bg(rgb(color))
}

fn section_heading(label: &'static str) -> Div {
    div().text_xs().text_color(rgb(MUTED)).child(label)
}

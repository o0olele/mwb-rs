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
    platform,
    protocol::{MAX_TEXT_BYTES, Message},
};
use std::{
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

const INK: u32 = 0x202D29;
const MUTED: u32 = 0x7B8781;
const GREEN: u32 = 0x227455;
const BORDER: u32 = 0xE4E9E5;

struct Workspace {
    config: Config,
    runtime: Arc<Runtime>,
    secret: Entity<InputState>,
    key_length: Entity<InputState>,
    address: Entity<InputState>,
    tab: usize,
    notice: String,
    show_key: bool,
    preparing: bool,
    _refresh: Task<()>,
}

impl Workspace {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (config, notice) = match Config::load() {
            Ok(config) => (config, String::new()),
            Err(err) => (Config::default(), format!("{err:#}")),
        };
        let runtime = Runtime::new(&config);
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
                if this.update(cx, |_, cx| cx.notify()).is_err() {
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
            notice,
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
        if self.runtime.enabled.load(Ordering::SeqCst) {
            self.runtime.reclaim();
            self.notice = "已暂停共享，键鼠由本机控制".into();
        } else {
            match platform::install_hooks(self.runtime.clone()) {
                Ok(()) => {
                    self.runtime.enabled.store(true, Ordering::SeqCst);
                    self.notice = "共享已恢复，可从屏幕边缘切换到另一台电脑".into();
                }
                Err(err) => self.notice = format!("输入捕获启动失败：{err:#}"),
            }
        }
        cx.notify();
    }

    fn copy_clipboard(&mut self, cx: &mut Context<Self>) {
        let result = (|| -> anyhow::Result<()> {
            let text = arboard::Clipboard::new()?.get_text()?;
            anyhow::ensure!(text.len() <= MAX_TEXT_BYTES, "首版支持最多 32 KB 文本");
            let message = Message::Clipboard(text);
            anyhow::ensure!(
                serde_json::to_vec(&message)?.len() + 16 <= mwb::transport::MAX_FRAME,
                "文本转义后超过单帧限制，请减少文本长度"
            );
            anyhow::ensure!(self.runtime.send(message), "发送队列不可用");
            Ok(())
        })();
        self.notice = match result {
            Ok(()) => "已发送本机剪贴板文本".into(),
            Err(err) => format!("{err:#}"),
        };
        cx.notify();
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
        let state = self.runtime.snapshot();
        let local = device_card(
            "01 / 本机",
            self.config.name.clone(),
            "当前 Windows 电脑".into(),
            true,
            true,
        );
        let peer = device_card(
            "02 / 另一台电脑",
            if state.connected {
                state.peer_name.clone()
            } else {
                "等待配对".into()
            },
            if state.connected {
                state.address.clone()
            } else {
                "连接后显示设备信息".into()
            },
            false,
            state.connected,
        );
        let (first, second) = if self.config.peer_on_right {
            (local, peer)
        } else {
            (peer, local)
        };
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
                                    .child("你的桌面，连在一起"),
                            )
                            .child(subtle("将另一台电脑放在屏幕左侧或右侧。")),
                    )
                    .child(
                        Button::new("flip")
                            .ghost()
                            .icon(assets::IconName::ArrowLeftRight)
                            .label("交换位置")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.runtime.return_local();
                                this.config.peer_on_right = !this.config.peer_on_right;
                                this.runtime
                                    .right
                                    .store(this.config.peer_on_right, Ordering::SeqCst);
                                this.persist();
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap_5()
                    .py_1()
                    .child(first)
                    .child(
                        div()
                            .v_flex()
                            .items_center()
                            .gap_2()
                            .text_color(rgb(if state.connected { GREEN } else { 0xBEC8C0 }))
                            .child(Icon::new(assets::IconName::ArrowLeftRight).size_5())
                            .child(div().text_size(px(10.)).child(if state.connected {
                                "已加密"
                            } else {
                                "待连接"
                            })),
                    )
                    .child(second),
            )
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .justify_between()
                    .border_t_1()
                    .border_color(rgb(BORDER))
                    .pt_3()
                    .child(subtle(if self.config.peer_on_right {
                        "向右越过屏幕边缘 → 控制另一台电脑"
                    } else {
                        "向左越过屏幕边缘 ← 控制另一台电脑"
                    }))
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child("左右布局 · 2 台设备"),
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
                            .child("连接一台电脑"),
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
                            .child(div().text_sm().child("2. 输入对方 IP 地址或主机名"))
                            .child(Input::new(&self.address).disabled(running))
                            .child(
                                div()
                                    .h_flex()
                                    .gap_2()
                                    .child(
                                        Button::new("listen")
                                            .label("等待连接")
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
                                                "连接电脑"
                                            })
                                            .disabled(running)
                                            .on_click(
                                                cx.listener(|this, _, _, cx| this.start(true, cx)),
                                            ),
                                    ),
                            )
                            .child(subtle("一端等待，另一端填写地址并连接。")),
                    ),
            )
    }

    fn sharing(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let connected = self.runtime.snapshot().connected;
        let enabled = self.runtime.enabled.load(Ordering::SeqCst);
        let remote = self.runtime.remote.load(Ordering::SeqCst);
        let incoming = self.runtime.incoming.load(Ordering::SeqCst);
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
                                    .label("发送剪贴板文本")
                                    .disabled(!connected || !enabled)
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.copy_clipboard(cx)),
                                    ),
                            )
                            .child(
                                Button::new("control")
                                    .small()
                                    .label("控制对方")
                                    .disabled(!connected || !enabled || remote)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        if this.runtime.send(Message::Enter {
                                            from_left: this.config.peer_on_right,
                                        }) {
                                            this.runtime.remote.store(true, Ordering::SeqCst);
                                            platform::park_cursor();
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
                "从屏幕边缘切换",
                "鼠标跨过设备相邻的屏幕边缘时，将控制权交给对方。",
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
                "剪贴板文本",
                "在设备与连接页主动发送，单次最多 32 KB。",
                div().text_xs().text_color(rgb(MUTED)).child("手动发送"),
            ))
            .child(option_row(
                "紧急返回",
                "立即停止本机共享，并释放远端按下的按键。",
                div().text_sm().child("Ctrl + Alt + Home"),
            ))
            .child(subtle("设备左右位置、共享偏好和对方地址会自动保存在本机。"))
    }

    fn about(&self) -> impl IntoElement {
        card().gap_5()
            .child(div().text_2xl().font_weight(FontWeight::SEMIBOLD).child("一套键鼠，两台电脑。"))
            .child(subtle(format!("MWB Rust · {} 预览版", env!("CARGO_PKG_VERSION"))))
            .child(div().text_sm().child("以 Rust + GPUI Kit 重写 Mouse without Borders 的核心使用流程。"))
            .child(div().text_sm().child("当前支持：两台 Windows 电脑直连、密钥配对、加密键鼠传输、左右越界切换、手动发送文本剪贴板。"))
            .child(div().text_sm().child("后续计划：自动发现、多设备布局、文件传输、托盘与后台服务。"))
            .child(div().text_sm().child("两端均需运行本重写版。普通用户权限下运行，当前不支持 UAC 安全桌面。"))
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
                                                    "为你的双机桌面，设置顺手的工作方式。",
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

fn device_card(
    label: &'static str,
    name: String,
    info: String,
    local: bool,
    connected: bool,
) -> Div {
    div()
        .v_flex()
        .flex_1()
        .min_w_0()
        .items_center()
        .gap_1()
        .child(
            div()
                .h_flex()
                .w_full()
                .justify_between()
                .mb_1()
                .text_size(px(10.))
                .text_color(rgb(MUTED))
                .child(label)
                .child(if local { "THIS DEVICE" } else { "PEER DEVICE" }),
        )
        .child(
            div()
                .v_flex()
                .items_center()
                .w(px(168.))
                .h(px(70.))
                .rounded_lg()
                .border_2()
                .border_color(rgb(if local || connected {
                    0x94AC96
                } else {
                    0xD5DDD6
                }))
                .bg(rgb(if local { 0xEFF4E8 } else { 0xF6F8F5 }))
                .justify_center()
                .gap_1()
                .child(
                    Icon::new(assets::IconName::Monitor)
                        .size_6()
                        .text_color(rgb(if local || connected { GREEN } else { 0xB8C4BA })),
                )
                .child(
                    div()
                        .text_size(px(10.))
                        .text_color(rgb(MUTED))
                        .child(if local {
                            "你的工作，从这里出发"
                        } else if connected {
                            "已建立加密连接"
                        } else {
                            "等待另一台电脑加入"
                        }),
                ),
        )
        .child(div().w(px(2.)).h(px(8.)).bg(rgb(0xBECBBF)))
        .child(
            div()
                .w(px(44.))
                .h(px(3.))
                .rounded_full()
                .bg(rgb(0xBECBBF))
                .mb_0(),
        )
        .child(div().font_weight(FontWeight::SEMIBOLD).child(name))
        .child(div().text_size(px(10.)).text_color(rgb(MUTED)).child(info))
}

fn main() {
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

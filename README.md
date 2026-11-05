# MWB Rust

使用 **Rust + GPUI Kit** 重写 Mouse without Borders 的 Windows 首版。
项目位于 `E:\Code\mwb`，原 Microsoft Garage 安装目录作为功能参考。

## 运行

Windows 10/11，Rust MSVC 工具链、Visual Studio C++ Build Tools、Windows SDK 和 CMake。
当前固定使用 GPUI Kit **0.8.1**，依赖版本由 `Cargo.lock` 锁定。

```powershell
cargo run --locked
```

开发构建：`target\debug\mwb.exe`。
发布构建：`cargo build --release --locked`，输出 `target\release\mwb.exe`。

## 两台电脑连接

1. 在两台 Windows 电脑上运行本重写版。
2. A 电脑点击「复制密钥」，将密钥粘贴到 B 电脑的配对密钥框。连接期间无法修改密钥。
3. A 点击「等待连接」。B 输入 A 的局域网 IP 或主机名，点击「连接电脑」。两台电脑均需允许本程序的 TCP **24810** 通信。
4. 显示「设备已连接」后，两端分别点击「开启共享」。通过「交换位置」配置对方相对本机的左右位置；两端布局应互为镜像。
5. 将鼠标移到相邻的屏幕边缘，即可控制对方。也可以点击「控制对方」。从对方的入口边缘移回，即可回到本机。
6. **Ctrl + Alt + Home** 随时停止共享并回到本机。接收端的本地物理键鼠操作也会停止共享。重新共享需要再次点击「开启共享」。

「发送剪贴板文本」主动发送本机剪贴板中的文本，接收端也须开启共享。单次最多 32 KB（JSON 编码后的加密帧还须小于 64 KB）。此操作会覆盖对方的文本剪贴板，不会自动读取或同步剪贴板。

## 已实现

- 原生中文界面：设备布局、连接管理、共享偏好、连接记录和实际收发计数。
- 双机 TCP 直连，手动 IP/主机名连接。
- 256 位随机配对密钥，Noise NNpsk0 / X25519 / ChaCha20-Poly1305 / BLAKE2s 加密连接，每次连接重新握手。
- Windows 低级键鼠钩子，鼠标移动、五键、水平/垂直滚轮、键盘扫描码注入。
- 左右屏幕边缘切换、手动控制、紧急返回。
- 心跳超时、断线释放远端按键、发送队列拥塞时恢复本机控制。
- 保存设备左右位置、共享偏好和对方地址；配对密钥仅保留在当前进程内。

## 首版边界

- 仅支持 **两台 Windows 电脑**，使用本重写版协议。
- 尚未实现自动发现、自动重连、多设备/上下布局、文件传输、图片剪贴板、托盘、开机启动、Windows 服务和 UAC 安全桌面。
- 输入注入遵循 Windows 权限限制。普通权限程序无法控制较高权限窗口。
- 屏幕布局按 Windows 虚拟桌面的左右外缘处理，复杂多显示器布局和不同 DPI 需进行实机验证。
- 当前未进行两台实体电脑的端到端键鼠验证；自动化测试覆盖协议和连接状态，不能替代双机测试。
- 密钥不持久化，每次重启后需重新配对；本机配置默认保存在 `%LOCALAPPDATA%\MWB-Rust\settings.json`。

## 验证

```powershell
cargo fmt --all -- --check
cargo test --no-default-features --lib --locked
cargo clippy --all-targets --locked -- -D warnings
```

`--no-default-features` 运行核心测试，无需编译 GUI，不安装输入钩子。协议测试包括随机密钥格式、双向加密、TCP 分片、篡改拒绝、错误密钥拒绝和回环连接生命周期。

开发或 UI 验证时可设置 `MWB_CONFIG_DIR` 到独立目录，避免影响正式配置。

## 代码结构

| 文件 | 职责 |
| --- | --- |
| `src/main.rs` | GPUI Kit 界面和用户操作 |
| `src/config.rs` | 本机设置、随机密钥与密钥校验 |
| `src/protocol.rs` | 独立协议和消息类型 |
| `src/transport.rs` | Noise 握手、加密帧、TCP 分片处理 |
| `src/engine.rs` | 连接生命周期、心跳、消息路由、共享状态 |
| `src/platform/windows.rs` | Windows 键鼠捕获、注入、按键释放 |

参考：[GPUI Kit](https://github.com/longbridge/gpui-kit)、[Mouse without Borders 功能说明](https://learn.microsoft.com/en-us/windows/powertoys/mouse-without-borders)。

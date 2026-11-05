# MWB Rust

使用 **Rust + GPUI Kit** 重写 Mouse without Borders 的 Windows 首版。
项目位于 `E:\Code\mwb`，原 Microsoft Garage 安装目录作为功能参考。

**v0.1.2**：修复远端鼠标慢移时丢失小位移、批量移动消息累计不足和 DPI 坐标混用；保留连接后自动共享。

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
2. A 电脑点击「复制密钥」，将密钥粘贴到 B 电脑的配对密钥框。也可以在两端直接输入相同的 **1–64 个字符**短口令，或设置「随机生成」位数后点击「生成密钥」。默认生成 64 位，连接期间无法修改。
3. A 点击「等待连接」。B 输入 A 的局域网 IP 或主机名，点击「连接电脑」。两台电脑均需允许本程序的 TCP **24810** 通信。
4. 显示「设备已连接」后，两端自动开启键鼠共享。通过「交换位置」配置对方相对本机的左右位置；两端布局应互为镜像。
5. 将鼠标移到相邻的屏幕边缘，即可控制对方。也可以点击「控制对方」。从对方的入口边缘移回，即可回到本机。
6. **Ctrl + Alt + Home** 随时暂停共享并回到本机，也可点击「暂停共享」；手动暂停后点击「恢复共享」继续使用。接收端操作自己的物理键鼠时，控制权回到本机，共享保持待命，之后仍可从边缘再次切换。

「发送剪贴板文本」主动发送本机剪贴板中的文本，接收端也须开启共享。单次最多 32 KB（JSON 编码后的加密帧还须小于 64 KB）。此操作会覆盖对方的文本剪贴板，不会自动读取或同步剪贴板。

自动共享在密钥验证和设备握手完成后启用。若 Windows 输入钩子安装失败，底部状态会显示具体错误，仍可手动重试恢复共享。更新前的首版仍需手动开启共享，两端均更新后即可自动使用。

## 鼠标移动精度

鼠标位移按物理像素 1:1 传递，接收端从上一次发送的目标位置连续累计，不依赖 Windows 异步更新的光标读数。绝对输入定位到像素中心，避免取整吞掉 1 像素的细小移动；桌面尺寸与捕获位置统一使用物理坐标。该修复不更改系统的指针速度或加速设置。像素密度不同的屏幕仍可能产生物理距离上的体感差异。

坐标换算参考 [.NET WPF 的输入实现](https://github.com/dotnet/wpf/blob/main/src/Microsoft.DotNet.Wpf/src/UIAutomation/UIAutomationClientSideProviders/MS/Internal/AutomationProxies/Input.cs)。

## 自定义密钥

- 支持 1–64 个字符，可使用数字、字母、符号或中文；自动去除首尾空白，中间空格保留。自定义口令区分大小写。
- 「随机生成」支持 1–64 位，生成十六进制字符（0–9、a–f）；例如填写 `6` 即生成 6 位随机密钥。默认仍为 64 位。
- 现有 64 位十六进制密钥沿用原来的字节解释（大小写等效），可与首版互联；自定义短口令需两端都更新。
- 自定义口令使用 Argon2id 派生 32 字节的 Noise 预共享密钥：版本 0x13、内存 19 MiB、迭代 2 次、并行度 1，公开固定域盐 `mwb-rust/pairing/argon2id/v1`。两端使用相同参数，派生在后台运行。
- 短密钥更容易被猜中。派生算法增加猜测成本，不能赋予短口令与随机长密钥相同的强度；默认随机密钥适合需要更强保护的环境。

## 已实现

- 原生中文界面：设备布局、连接管理、共享偏好、连接记录和实际收发计数。
- 双机 TCP 直连，手动 IP/主机名连接。
- 默认 256 位随机配对密钥，支持自定义短口令和随机位数；Noise NNpsk0 / X25519 / ChaCha20-Poly1305 / BLAKE2s 加密连接，每次连接重新握手。
- Windows 低级键鼠钩子，鼠标移动、五键、水平/垂直滚轮、键盘扫描码注入。
- 配对连接后自动共享键鼠，支持左右屏幕边缘切换、手动控制、暂停/恢复和紧急返回。
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

`--no-default-features` 运行核心测试，无需编译 GUI，不安装输入钩子。协议测试包括随机密钥位数、自定义口令校验与派生、原密钥兼容、短口令握手、双向加密、TCP 分片、篡改拒绝、错误密钥拒绝和回环连接生命周期。连接测试使用可替换的输入后端，验证两端自动启用、鼠标消息路由、本机接管后再次切换、暂停保持和钩子安装失败反馈。

鼠标测试覆盖常见分辨率逐像素定位、负坐标显示器、连续 1 像素慢移、分批/合批距离一致、正反向移动及边缘返回；这些测试不移动真实桌面光标。

开发或 UI 验证时可设置 `MWB_CONFIG_DIR` 到独立目录，避免影响正式配置。

## 代码结构

| 文件 | 职责 |
| --- | --- |
| `src/main.rs` | GPUI Kit 界面和用户操作 |
| `src/config.rs` | 本机设置、随机密钥与密钥校验 |
| `src/protocol.rs` | 独立协议和消息类型 |
| `src/transport.rs` | Noise 握手、加密帧、TCP 分片处理 |
| `src/engine.rs` | 连接生命周期、心跳、消息路由、共享状态 |
| `src/motion.rs` | 物理像素坐标换算、目标位置累计、边缘返回及回归测试 |
| `src/platform/windows.rs` | Windows 键鼠捕获、注入、按键释放 |

参考：[GPUI Kit](https://github.com/longbridge/gpui-kit)、[Mouse without Borders 功能说明](https://learn.microsoft.com/en-us/windows/powertoys/mouse-without-borders)。

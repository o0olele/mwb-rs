# 开发指南

[返回首页](../README.md) · [使用指南](usage.md) · [安装构建设计](install.md#安装构建)

## 环境要求

Windows 10/11，Rust **1.95.0 或更新版本**的 MSVC 工具链、Visual Studio C++ Build Tools、Windows SDK 和 CMake。GPUI Kit 固定为 0.8.1，其依赖 `gpui-pre 0.3.10` 使用的 `std::hint::cold_path()` 从 Rust 1.95.0 起稳定；旧版 Rust 会报 `E0658`。

仓库通过 `rust-toolchain.toml` 固定使用 Rust 1.95.0。使用 rustup 时，在项目目录运行以下命令会自动选择该工具链，无需修改全局默认版本。首次构建可先安装：

    rustup toolchain install 1.95.0 --profile minimal --component rustfmt --component clippy

## 构建与检查

在项目目录确认 `rustc --version` 显示 1.95.0，然后构建与验证：

    cargo run --locked
    cargo fmt --all -- --check
    cargo test --no-default-features --lib --locked
    cargo clippy --all-targets --locked -- -D warnings
    cargo build --release --locked

发布输出为 target\release\mwb.exe 和可选的 target\release\mwb-input-service.exe。测试使用真实 TCP + Noise 和替代输入后端，验证三设备路由、连续跨设备切换、布局同步/冲突、旧会话事件隔离、竞争控制、断线重连、暂停保留及故障恢复，不安装钩子、不移动真实光标、不修改真实剪贴板。

## 安装包构建

开发者安装 WiX Toolset 3.14 并将其加入 PATH 后，运行 `powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\build-installer.ps1` 可构建并校验单文件 MSI，输出到 `artifacts`。打包脚本会核验两个程序的 x64 格式及版本，详见 [安装构建设计](install.md)。

运行 `powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\build-release.ps1` 可一次生成 MSI、便携 ZIP 及各自的 SHA-256 校验文件。便携包内含两个程序、README、更新日志和使用文档，解压后直接运行 `mwb.exe`。该脚本同样支持 `-WixDirectory`、`-BuildDirectory`、`-OutputDirectory` 和 `-SkipBuild`。

## GitHub Release

仓库提供手动触发的 [Release 工作流](../.github/workflows/release.yml)。工作流需要先推送到默认分支，GitHub 才会显示手动运行入口。

1. 准备好待发布的代码和更新日志；发布新版本时同步修改 `Cargo.toml` 与 `Cargo.lock` 中本项目的版本并推送。
2. 打开仓库的 **Actions → Release → Run workflow**，选择待发布分支。
3. 默认自动发布正式 Release；勾选 `draft` 可只创建草稿，勾选 `prerelease` 可标记为预发布。

工作流从所选提交的 `Cargo.toml` 读取版本（例如 `0.5.9`），以该提交创建 `v0.5.9` 标签。它使用仓库固定的 Rust 工具链，在 Windows x64 上执行格式检查、库测试和 Clippy，再构建并严格校验 MSI、生成便携 ZIP，最后自动生成 Release 说明并上传四个文件：

- `mwb-v版本-windows-x64.msi`
- `mwb-v版本-windows-x64.msi.sha256`
- `mwb-v版本-windows-x64-portable.zip`
- `mwb-v版本-windows-x64-portable.zip.sha256`

打包结果还会作为该次运行的 Actions artifact 保留 14 天。发布使用工作流自带的 `GITHUB_TOKEN` 和 `contents: write` 权限，无需额外配置个人令牌。已有同名 Release 或标签指向其他提交时会提前失败；应递增版本，若上次上传失败留下未完成的草稿，则先删除该草稿再重试。

## 专项验证

文件和剪贴板测试覆盖文本/图像防回环、重连版本、旧内容竞争、按需粘贴、多文件、缓存复用、取消重试、中断清理、源文件变更、同名保护、校验和限速期间的输入响应。原生 COM 测试验证格式枚举、Unicode/64 位描述符、跨线程封送、读取、Seek、Clone 和 EOF。大文件和 Windows Shell 接收测试需单独运行：

    cargo test --release --no-default-features --lib --locked file_larger_than_original_100mb_limit_streams_end_to_end -- --ignored --nocapture
    cargo test --no-default-features --lib --locked native_clipboard_paste_larger_than_original_100mb_limit_streams_end_to_end -- --ignored --nocapture
    cargo test --no-default-features --lib --locked windows_shell_pastes_virtual_files -- --ignored --nocapture

Windows Shell 测试直接把原生数据对象交给测试目录的接收接口，验证异步接收与实际文件内容，不修改系统剪贴板。仍需实体电脑确认资源管理器 Ctrl+C/Ctrl+V、不同应用兼容性和无线网络体验。

默认忽略的 standard_mouse_api_accepts_zero_delta 可在交互式桌面单独运行，只注入一次零位移。自动化测试不能代替多台实体电脑的手感、DPI、输入法和复杂多屏验收。

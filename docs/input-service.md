# 锁屏输入服务

本实现针对“控制端保持解锁，被控端已运行 MWB 并连接工作组，随后锁屏”的场景。安装可选服务后，接收端输入由登录会话中的 LocalSystem 代理执行。服务仅接收键鼠命令；网络认证、工作组、剪贴板和文件传输继续由普通用户主程序负责。

Windows 的钩子属于各自的桌面，普通进程不能访问 Winlogon 桌面；服务本身位于会话 0，因此不能直接从服务线程注入到用户桌面。实现使用服务创建会话内代理，代理线程在移除钩子后调用 `SetThreadDesktop`，再安装仅用于本地接管检测的钩子。[桌面隔离](https://learn.microsoft.com/en-us/windows/win32/winstation/desktops)、[服务与用户会话](https://learn.microsoft.com/en-us/windows/win32/services/interactive-services)、[桌面切换限制](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-setthreaddesktop)。

## 安装与更新

1. 构建 `cargo build --release --locked`，得到 `mwb.exe`、`mwb-input-service.exe`。
2. 关闭被控端安装目录中已有的 MWB 窗口，在管理员 PowerShell 中运行：

   ```powershell
   powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\input-service.ps1 -Action Install
   ```

   在其他电脑部署时，复制安装脚本和两个 exe，并用 `-BuildDirectory 'D:\path\to\binaries'` 指定 exe 所在目录。

3. 脚本将两个程序复制到系统 Program Files 下的 `MWB-Rust`，设置仅管理员和 SYSTEM 可写的目录/文件权限，注册并启动 `MwbRustInput` 自动启动服务。不会修改防火墙规则、用户配置或配对密钥。
4. 从安装目录打开 `mwb.exe`，正常创建或加入工作组。开发目录中的 exe 不被服务信任，必须使用安装后的版本。
5. 锁定被控端后，从控制端移动鼠标切入；点击登录界面并输入密码或 PIN。使用本地键鼠会接管，之后可再次切入。

更新时关闭安装目录中的主程序，再执行同一安装命令。脚本会停止旧服务、更换两个 exe、重新启动服务。已安装的服务不会替你自动运行主程序或保存密钥。

## 不注入输入的部署检查

在被控端当前登录用户的 PowerShell 中，等待服务启动数秒，且没有活动远控时运行：

```powershell
$appPath = Join-Path ([Environment]::GetFolderPath('ProgramFiles')) 'MWB-Rust\mwb.exe'
$probe = Start-Process -FilePath $appPath -ArgumentList '--check-input-service' -WindowStyle Hidden -Wait -PassThru
$probe.ExitCode
```

返回 `0` 表示已完成本地管道双向身份校验和握手；`2` 表示当前会话没有输入代理；`3` 表示路径、身份、版本校验失败或代理忙/连接异常。这个检查不安装钩子、不读取剪贴板、不注入键鼠，不能替代锁屏实际输入测试。不要使用其他管理员账户替代当前控制台用户做这个检查。

诊断命令：`Get-Service MwbRustInput`。代理启动或通信错误记录在安装目录的 `input-service.log`，最大约 1 MiB；不记录键值、密码、配对密钥或剪贴板内容。若在锁屏状态遇到服务故障，先用被控端本地键鼠解锁，修复服务后重新开启共享。

## 输入与故障处理

- 首次接收控制时自动尝试本会话服务；未安装时保持原有普通桌面输入方式，锁屏时提示需要输入服务。服务已连接后发生故障会结束当前控制，不会把尚未确认的按键重发到其他后端。
- 接收端每约 8 ms 查询一次输入状态，兼顾光标反馈、服务保活和本地接管；比例模式也参与检查。桌面变化时释放旧按键，恢复鼠标模式与位置，不把旧的按下状态带入登录界面。
- 桌面切换与注入发生竞争时，仅在确认原事件未插入且桌面已变化后重试一次。成功插入的键盘事件不会因后续光标读取失败而重放。
- 单次本地 I/O 等待上限为 750 ms；代理在 1.5 秒没有收到下一条命令时关闭输入会话并释放按键。窗口退出、管道断开、服务停止、非法消息或超时均触发释放。停止服务先等待代理正常退出，任务对象提供异常退出后的进程清理。
- LocalSystem 服务只为当前物理控制台的已登录用户启动代理，用户或会话变化时关闭旧代理。无网络监听、无任意文件/进程操作命令。

## 权限范围

安装后，已配对设备可以向被控端锁屏界面和更高权限窗口输入。仅在信任参与配对的设备时安装，保管好配对密钥。

本地管道拒绝网络客户端，只授权 SYSTEM 和当前登录 SID，禁止普通用户创建同名管道实例；双方校验对端进程身份、会话和安装路径。服务器还校验客户端登录 SID。持有进程句柄避免 PID 重用，消息限制为 4096 字节，使用独立版本和严格递增序号。安装脚本拒绝重解析点和未知目录内容，并将服务及客户端放入受保护目录。[命名管道访问控制](https://learn.microsoft.com/en-us/windows/win32/ipc/named-pipe-security-and-access-rights)。

不支持锁屏控制端的输入捕获、RDP 会话、注销/首次登录之前新建工作组、远程安全注意序列 `Ctrl+Alt+Delete`。若 Windows 策略要求该组合键，仍需在被控端本地触发。多用户切换、UAC 和复杂多屏属于待实机验证范围。

## 验收状态

自动测试使用替代输入后端验证锁屏/解锁状态切换、模式保留、按键释放、桌面切换竞争、本地接管、服务断开后连接保留和重新开启共享。另测试真实 Windows 命名管道的超时取消/收发、对端身份约束和协议拒绝；不在测试机器上真实锁屏或注入输入。

**尚未完成两台实体电脑的锁屏登录验收。** 部署后需分别验证：被控端先锁屏再切入；远控过程中锁屏；密码/PIN 解锁后继续移动；两种鼠标模式；本地键鼠接管；按住按键时退出主程序或停止服务；卸载后普通桌面控制。测试结束确认没有按键或鼠标按钮残留。

## 卸载

关闭安装目录中的主程序，在管理员 PowerShell 运行：

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\input-service.ps1 -Action Uninstall
```

仅移除本服务、安装的两个程序和服务日志；保留用户设置和接收文件。之后可继续从其他目录运行普通模式的 `mwb.exe`。

# 锁屏输入服务

本实现针对“控制端保持解锁，被控端已运行 MWB 并连接工作组，随后锁屏”的场景。安装可选服务后，接收端输入由登录会话中的 LocalSystem 代理执行。服务仅接收键鼠命令；网络认证、工作组、剪贴板和文件传输继续由普通用户主程序负责。

v0.5.2 增加启动时的安装位置提示：已注册服务但主程序从其他目录运行时，“键鼠共享”区域会显示应打开的安装路径。服务启动命令按 Windows 参数规则解析，再比较规范化路径，避免同一安装位置的路径写法不同而误拒绝；仍拒绝其他目录中的便携副本。

v0.5.1 修复“已解锁也因输入服务拒绝访问而退回本机”的回归。普通桌面不再依赖服务是否可连接；客户端核验服务时也不再尝试打开 SYSTEM 进程令牌。更新后请关闭旧窗口、重新安装两个配套 exe，再从安装目录运行新版本。

Windows 的钩子属于各自的桌面，普通进程不能访问 Winlogon 桌面；服务本身位于会话 0，因此不能直接从服务线程注入到用户桌面。实现使用服务创建会话内代理，代理线程在移除钩子后调用 `SetThreadDesktop`，再安装仅用于本地接管检测的钩子。[桌面隔离](https://learn.microsoft.com/en-us/windows/win32/winstation/desktops)、[服务与用户会话](https://learn.microsoft.com/en-us/windows/win32/services/interactive-services)、[桌面切换限制](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-setthreaddesktop)。

## 安装与更新

1. 构建 `cargo build --release --locked`，得到 `mwb.exe`、`mwb-input-service.exe`。
2. 关闭被控端安装目录中已有的 MWB 窗口，在管理员 PowerShell 中运行：

   ```powershell
   powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\input-service.ps1 -Action Install
   ```

   在其他电脑部署时，复制安装脚本和两个 exe，并用 `-BuildDirectory 'D:\path\to\binaries'` 指定 exe 所在目录。

   脚本支持 Windows PowerShell 5.1 和 PowerShell 7。默认构建目录相对于脚本自身解析，不依赖当前工作目录。可先在同一命令后加 `-WhatIf`，检查文件和路径并预览安装操作；这不会写入安装目录或更改服务，也不要求管理员权限。卸载同样支持 `-WhatIf`；实际安装和卸载仍要求管理员权限。

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

若连接记录提示“请使用安装目录中的 MWB 程序”或安装目录不一致，关闭当前主程序，再打开已安装的 `mwb.exe` 并重新连接。若从安装目录运行后仍提示 `SendInput 返回 0/1，Win32 错误码 5`，则已进入代理输入阶段；这是另一个尚未解决的问题，当前修复不能视为 PIN 登录已通过验收。输入失败会暂停共享，所以返回锁屏后也需先在本地解锁并恢复共享。

## 输入与故障处理

- 普通桌面直接注入输入，不尝试连接可选服务。因此服务停止、权限异常或尚未安装，不影响普通桌面切换。切入时已锁屏，或当前控制期间桌面变为不可访问时，再连接本会话服务并迁移鼠标模式与位置。
- 控制一旦迁入服务，该次会话保持服务输入直到释放。服务已连接后发生故障会结束当前控制，不会把尚未确认的按键重发到普通桌面。再次从普通桌面切入时重新选择本地输入。
- 接收端每约 8 ms 查询一次输入状态，兼顾光标反馈、服务保活和本地接管；比例模式也参与检查。桌面变化时释放旧按键，恢复鼠标模式与位置，不把旧的按下状态带入登录界面。
- 桌面切换与注入发生竞争时，仅在确认原事件未插入且桌面已变化后重试一次。成功插入的键盘事件不会因后续光标读取失败而重放。
- 单次本地 I/O 等待上限为 750 ms；代理在 1.5 秒没有收到下一条命令时关闭输入会话并释放按键。窗口退出、管道断开、服务停止、非法消息或超时均触发释放。停止服务先等待代理正常退出，任务对象提供异常退出后的进程清理。
- LocalSystem 服务只为当前物理控制台的已登录用户启动代理，用户或会话变化时关闭旧代理。无网络监听、无任意文件/进程操作命令。

## 权限范围

安装后，已配对设备可以向被控端锁屏界面和更高权限窗口输入。仅在信任参与配对的设备时安装，保管好配对密钥。

本地管道拒绝网络客户端，只授权 SYSTEM 和当前登录 SID，禁止普通用户创建同名管道实例。服务端校验客户端程序、账户和登录会话；客户端通过只读 SCM 查询确认服务使用 LocalSystem、安装路径匹配且正在运行，再核验代理的可执行文件、会话以及操作系统记录的父进程是否为该服务。持有进程句柄避免代理 PID 重用，核验结束再次检查服务未重启。普通用户只获准查询代理进程和等待其退出，没有进程内存写入、创建子进程或 SYSTEM 令牌访问权限。[命名管道访问控制](https://learn.microsoft.com/en-us/windows/win32/ipc/named-pipe-security-and-access-rights)、[跨账户令牌访问限制](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-openprocesstoken)。

消息限制为 4096 字节，使用独立版本和严格递增序号。安装脚本拒绝重解析点和未知目录内容，并将服务及客户端放入受保护目录。服务连接错误会显示管道打开、消息模式设置、SCM 查询或对端核验等具体阶段，方便区分访问拒绝的来源。

代理进程号和会话号直接从已连接的命名管道读取，避免对 SYSTEM 进程调用需要 `PROCESS_QUERY_INFORMATION` 权限的 `ProcessIdToSessionId`。代理仅向当前登录用户开放 `PROCESS_QUERY_LIMITED_INFORMATION` 和同步等待权限。[进程会话查询权限](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-processidtosessionid)、[管道会话查询](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-getnamedpipeserversessionid)。

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

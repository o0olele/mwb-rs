# 锁屏输入服务

本实现针对“控制端保持解锁，被控端已运行 MWB 并连接工作组，随后锁屏”的场景。安装可选服务后，接收端输入由登录会话中的 LocalSystem 代理执行。服务仅接收键鼠命令；网络认证、工作组、剪贴板和文件传输继续由普通用户主程序负责。

v0.5.6 扩展安装版的服务选择时机：用户已确认 v0.5.5 的 PIN 输入正常，但管理员 PowerShell 无法点击，从任务栏切入后失去控制；非管理员 PowerShell 正常，网络仍显示已连接且没有输入错误日志。原实现普通桌面先用普通权限注入，只有桌面不可访问或 `SendInput` 明确失败后才迁入服务，未在高权限窗口获得焦点前选择代理。

现在安装版在接收控制时先完成服务身份校验和 Hello，再通过 SYSTEM 代理执行首次定位、按下、释放及后续输入。同一会话保持同一后端，覆盖普通窗口、任务栏及管理员窗口之间的焦点变化。服务只在主程序路径与注册安装目录匹配时被优先尝试；便携/开发副本保持普通输入。服务不存在、未运行或握手失败时，普通桌面仍可使用直接输入；已发送服务 Enter 或任何输入后发生错误，则结束控制并报告错误，结果不明的事件不会回退重放。

连接记录新增 `接收端输入后端：SYSTEM 服务代理` 或普通桌面提示，便于核验管理员窗口是否实际使用服务。高权限应用的输入受 Windows UIPI 限制，使用 SYSTEM 服务控制此类窗口也是微软官方 Mouse Without Borders 的服务用途，见 [SendInput 限制](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-sendinput)、[服务与高权限应用](https://learn.microsoft.com/en-us/windows/powertoys/mouse-without-borders#install-mouse-without-borders-as-a-service)。本版网络协议仍为 5、本地输入服务协议仍为 3；尚需在用户的管理员 PowerShell 上复测。

v0.5.5 根据用户提供的 v0.5.4 失败日志继续定位：代理与物理控制台会话均为 15，SYSTEM 身份、系统完整性级别、UIAccess、`WinSta0` 及 `Winlogon` 线程/输入桌面都匹配，首次定位仍返回 0/5。这说明该次失败没有缺少前述令牌和桌面条件；此前没有检查**桌面句柄本身的实际访问权限**。

打开输入桌面时原先请求的掩码为 `0x189`，漏了 `DESKTOP_JOURNALPLAYBACK (0x20)`。本版请求 `0x1a9`，并在调用 `SetThreadDesktop` 前通过动态加载的 `NtQueryObject(ObjectBasicInformation)` 核验 `GrantedAccess`。访问位不足会给出具体的 required/granted 错误，权限查询失败也不会被当作注入成功。日志新增 `desktopAccess` 和 `stationAccess`，记录线程桌面及窗口站句柄实际授予的权限。此处使用的是桌面访问位，不安装 journal 钩子。[桌面访问权限](https://learn.microsoft.com/en-us/windows/win32/winstation/desktop-security-and-access-rights)、[NtQueryObject 的 GrantedAccess](https://learn.microsoft.com/en-us/windows/win32/api/winternl/nf-winternl-ntqueryobject)。原 MSDN 作者也记录过同类 LocalSystem/Winlogon 注入失败及补齐该访问位的处理，见 [原文归档镜像](https://www.sysadmins.lv/retired-msft-blogs/alejacma/sendinput-doesnt-work-but-it-doesnt-return-an-error.aspx)。

已在普通 `Default` 桌面用零位移真实注入验证：旧掩码实际授予 `0x189`，返回 0、错误码 5；新掩码实际授予 `0x1a9`，返回 1、错误码 0。直接附加及经过另一不可见测试桌面重新附加两种路径都验证成功。测试不改变可见桌面，不输入按键，隐藏测试桌面结束后释放；尚未在被控端输入真实 PIN。

本版本地输入服务协议为 **3**，拒绝仍使用旧掩码的服务；设备间网络协议仍为 5。

v0.5.4 针对用户在 **Windows 11 25H2** 复测 v0.5.3 时，代理首次定位仍收到 `SendInput 返回 0/1，Win32 错误码 5` 的反馈，补齐以下权限与诊断处理；当前尚未获得本版 PIN 输入成功的实机结果。

- UIAccess 是与 SYSTEM 身份、令牌完整性级别分别查询的属性。服务使用已启用的 `SeTcbPrivilege`，在**新复制的主令牌**上通过 32 位值设置 `TokenUIAccess`，读取核验后才创建代理；代理进程启动后再检查 SYSTEM、会话、完整性级别和 UIAccess。原版 Winlogon 源令牌不被修改。该属性的用途及系统注入限制见 [UI 自动化安全说明](https://learn.microsoft.com/en-us/windows/win32/winauto/uiauto-securityoverview)、[令牌信息类型](https://learn.microsoft.com/en-us/windows/win32/api/winnt/ne-winnt-token_information_class)。这修正了代码未显式配置 UIAccess 的缺口，但仅凭错误码 5 仍不能认定它就是该机器上的唯一原因。
- 代理启动及定位/注入失败记录 `agent` 版本、控制台/令牌会话号、SYSTEM 身份、完整性级别、UIAccess、窗口站、线程桌面和当前输入桌面。输入处理错误直接写入 `input-service.log`；此前这类错误只作为响应返回客户端，未经过外层 I/O 错误日志分支。
- 本地输入服务协议升级为 **2**，防止新主程序继续使用未配置上述权限的旧代理。管道名称保持不变，以便旧服务返回明确的版本不匹配提示；两台设备间的网络协议仍为 5。

v0.5.3 对照原版 E-024 及服务端 `NativeMethods.CreateProcessAsSystemAccountOnSpecificDesktop`，修正 PIN 输入的两处启动差异：复制**当前物理控制台会话**的 Winlogon 主令牌，而非仅修改会话 0 服务令牌的会话号；代理的初始桌面改为 `WinSta0\Winlogon`。复制前核验系统目录中的完整路径、SYSTEM 身份和会话，普通客户端仍不读取 SYSTEM 令牌。另补齐“活动桌面仍拒绝普通输入”时的服务迁移，以及首次鼠标定位遇到桌面切换时的一次重试。用户复测表明这些改动尚未解决该机器上的 PIN 定位失败。

v0.5.2 增加启动时的安装位置提示：已注册服务但主程序从其他目录运行时，“键鼠共享”区域会显示应打开的安装路径。服务启动命令按 Windows 参数规则解析，再比较规范化路径，避免同一安装位置的路径写法不同而误拒绝；仍拒绝其他目录中的便携副本。

v0.5.1 修复“已解锁也因输入服务拒绝访问而退回本机”的回归。普通桌面不再依赖服务是否可连接；客户端核验服务时也不再尝试打开 SYSTEM 进程令牌。更新后请关闭旧窗口、重新安装两个配套 exe，再从安装目录运行新版本。

Windows 的钩子属于各自的桌面，普通进程不能访问 Winlogon 桌面；服务本身位于会话 0，因此不能直接从服务线程注入到用户桌面。实现使用服务创建会话内代理，代理线程在移除钩子后调用 `SetThreadDesktop`，再安装仅用于本地接管检测的钩子。[桌面隔离](https://learn.microsoft.com/en-us/windows/win32/winstation/desktops)、[服务与用户会话](https://learn.microsoft.com/en-us/windows/win32/services/interactive-services)、[桌面切换限制](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-setthreaddesktop)。

## 安装与更新

普通使用推荐双击 `mwb-v0.5.8-windows-x64.msi`，点击「安装」并确认管理员授权，主程序、服务及快捷方式会自动安装。后续更新运行新 MSI，卸载使用 Windows 设置。旧脚本安装可直接升级为 MSI；MSI 管理的目录不再用脚本安装/卸载。配对、布局和接收文件保留。详见 [安装与更新](install.md)。

以下脚本方式用于开发和未交由 MSI 管理的旧部署：

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

更新时关闭安装目录中的主程序，再执行同一安装命令。脚本会停止旧服务、更换两个 exe、重新启动服务。已安装的服务不会替你自动运行主程序或保存密钥；v0.5.7 起主程序在成功配置后加密记住配对，再次打开会自动恢复工作组。

## 不注入输入的部署检查

在被控端当前登录用户的 PowerShell 中，等待服务启动数秒，且没有活动远控时运行：

```powershell
$appPath = Join-Path ([Environment]::GetFolderPath('ProgramFiles')) 'MWB-Rust\mwb.exe'
$probe = Start-Process -FilePath $appPath -ArgumentList '--check-input-service' -WindowStyle Hidden -Wait -PassThru
$probe.ExitCode
```

返回 `0` 表示已完成本地管道双向身份校验和握手；`2` 表示当前会话没有输入代理；`3` 表示路径、身份、版本校验失败或代理忙/连接异常。这个检查不安装钩子、不读取剪贴板、不注入键鼠，不能替代锁屏实际输入测试。不要使用其他管理员账户替代当前控制台用户做这个检查。

诊断命令：`Get-Service MwbRustInput`。代理启动或通信错误记录在安装目录的 `input-service.log`，最大约 1 MiB；不记录键值、密码、配对密钥或剪贴板内容。若在锁屏状态遇到服务故障，先用被控端本地键鼠解锁，修复服务后重新开启共享。

若连接记录提示“请使用安装目录中的 MWB 程序”或安装目录不一致，关闭当前主程序，再打开已安装的 `mwb.exe` 并重新连接。更新 v0.5.6 时请重新执行安装脚本，更换**主程序和输入服务**，再从安装目录运行。若仍遇到 `SendInput 返回 0/1`，记录完整错误链及服务日志：错误中的“会话输入代理在当前桌面定位/注入失败”表示已到达服务输入阶段；“普通桌面定位/输入未插入事件”表示普通输入被拒绝后迁移服务也失败。错误码 0 仅表示系统没有提供更多信息，不能据此判断注入成功或确定是 UIPI。[SendInput 返回值与限制](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-sendinput)。输入失败会暂停共享，需先在本地解锁、修复并恢复共享。

复现失败后，可在被控端解锁并读取最后的诊断记录：

```powershell
Get-Content -LiteralPath (Join-Path ([Environment]::GetFolderPath('ProgramFiles')) 'MWB-Rust\input-service.log') -Tail 20
```

代理启动记录应包含 `agent=0.5.8`、`system=true`、`integrity=0x4000`（或更高）、`uiAccess=true`；令牌会话号应与控制台一致。本地输入协议未变化，v0.5.5/v0.5.6/v0.5.7 服务也能被新版主程序使用，推荐通过 MSI 配套更新。失败记录同时带有 `threadDesktop`、`inputDesktop`、`desktopAccess` 和 `stationAccess`。附加后 `desktopAccess` 应包含 `0x1a9` 的全部访问位；若为 `0x189`，仍缺少已修正的 `0x20`。启动时尚未锁屏，输入桌面显示 `Default` 是正常的，应以失败时的快照为准。日志不记录键值、鼠标位置、PIN 或配对密钥。

## 输入与故障处理

- 安装版优先使用经身份校验的输入服务，以支持普通桌面、管理员窗口与锁屏界面。服务不存在或握手失败时，允许直接注入的普通桌面仍正常工作。切入时已锁屏，或普通输入过程中桌面不可访问、注入明确返回 0 时，仍会按原规则要求服务。
- 输入后端在每次切入时选择，使用服务的会话保持服务输入直到释放。服务实际定位/输入后发生故障会结束当前控制，不会把尚未确认的事件重发到普通桌面；普通输入会话也不会仅因服务恢复而在按住键鼠时切换后端，下次切入重新选择。
- 接收端每约 8 ms 查询一次输入状态，兼顾光标反馈、服务保活和本地接管；比例模式也参与检查。桌面变化时释放旧按键，恢复鼠标模式与位置，不把旧的按下状态带入登录界面。
- 代理中的首次定位、切换后的定位和后续输入遇到桌面竞争时，仅在 `SendInput` 确认未插入且桌面确实变化后重试一次。桌面未变时报告原错误，未知结果和成功插入的键盘事件均不重放；成功输入也不会因后续光标读取失败而重放。
- 单次本地 I/O 等待上限为 750 ms；代理在 1.5 秒没有收到下一条命令时关闭输入会话并释放按键。窗口退出、管道断开、服务停止、非法消息或超时均触发释放。停止服务先等待代理正常退出，任务对象提供异常退出后的进程清理。
- LocalSystem 服务只为当前物理控制台的已登录用户启动代理，用户或会话变化时关闭旧代理。无网络监听、无任意文件/进程操作命令。

## 权限范围

安装后，已配对设备可以向被控端锁屏界面和更高权限窗口输入。仅在信任参与配对的设备时安装，保管好配对密钥。

本地管道拒绝网络客户端，只授权 SYSTEM 和当前登录 SID，禁止普通用户创建同名管道实例。服务端校验客户端程序、账户和登录会话；客户端通过只读 SCM 查询确认服务使用 LocalSystem、安装路径匹配且正在运行，再核验代理的可执行文件、会话以及操作系统记录的父进程是否为该服务。持有进程句柄避免代理 PID 重用，核验结束再次检查服务未重启。普通用户只获准查询代理进程和等待其退出，没有进程内存写入、创建子进程或 SYSTEM 令牌访问权限。[命名管道访问控制](https://learn.microsoft.com/en-us/windows/win32/ipc/named-pipe-security-and-access-rights)、[跨账户令牌访问限制](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-openprocesstoken)。

消息限制为 4096 字节，使用独立版本和严格递增序号。安装脚本拒绝重解析点和未知目录内容，并将服务及客户端放入受保护目录。服务连接错误会显示管道打开、消息模式设置、SCM 查询或对端核验等具体阶段，方便区分访问拒绝的来源。

代理进程号和会话号直接从已连接的命名管道读取，避免对 SYSTEM 进程调用需要 `PROCESS_QUERY_INFORMATION` 权限的 `ProcessIdToSessionId`。代理仅向当前登录用户开放 `PROCESS_QUERY_LIMITED_INFORMATION` 和同步等待权限。[进程会话查询权限](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-processidtosessionid)、[管道会话查询](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-getnamedpipeserversessionid)。

不支持锁屏控制端的输入捕获、RDP 会话、注销/首次登录之前新建工作组、远程安全注意序列 `Ctrl+Alt+Delete`。若 Windows 策略要求该组合键，仍需在被控端本地触发。多用户切换、UAC 和复杂多屏属于待实机验证范围。

## 验收状态

v0.5.6 常规核心测试 **103 项通过，0 失败，5 项默认跳过**，并通过格式检查和全部目标的严格 lint。本次新增 4 项回归，覆盖服务优先处理首次按下/释放及后续移动/滚轮/按键、握手失败的普通桌面降级、服务 Enter 失败不重放，以及服务恢复后在新会话重新选择。未在本机安装服务或操作真实管理员 PowerShell 窗口；用户报告的管理员窗口场景仍需复测。

自动测试使用替代输入后端验证锁屏/解锁状态切换、模式保留、按键释放、桌面切换竞争、本地接管、服务断开后连接保留和重新开启共享。另测试真实 Windows 命名管道的超时取消/收发、对端身份约束和协议拒绝；不在测试机器上真实锁屏或注入输入。

v0.5.3 新增回归覆盖活动桌面拒绝输入（错误码 0/5）后的迁移、首次定位竞争、持续切换时重试上限、未知/非零插入结果不重放，以及成功解锁按键后立即切换桌面不重复输入。令牌复制和 Winlogon 桌面启动需在安装后的 LocalSystem 服务中实测，单元测试不能代替这部分验收。

v0.5.4 新增真实进程令牌/桌面属性的只读查询测试，以及代理权限组合、旧服务/旧客户端协议拒绝测试；自动测试不设置真实进程的 UIAccess、不安装代理服务、不触发锁屏或注入 PIN。服务启动时的权限配置和最终 PIN 登录效果仍需部署后确认。

v0.5.5 常规测试新增实际桌面句柄权限查询和旧掩码拒绝检查。另有默认跳过的真实零位移对照测试，本轮已单独运行通过：

```powershell
cargo test --no-default-features --lib --locked attached_desktop_access_controls_send_input -- --ignored --nocapture
```

该测试仅在已解锁交互桌面运行；创建不可见测试桌面以隔离缓存的附加权限，直接/重新附加时以新掩码成功注入零位移，旧掩码稳定返回 0/5。它不等同于真实 Winlogon/PIN 验收。

**用户已确认 v0.5.5 的 PIN 输入界面恢复正常。** 尚需完整验收：被控端先锁屏再切入、远控过程中锁屏、解锁后继续移动、两种鼠标模式、本地键鼠接管、退出/停止服务后的按键释放，以及 v0.5.6 在管理员 PowerShell、普通窗口和任务栏之间连续切换。测试结束确认没有按键或鼠标按钮残留。

## 卸载

关闭安装目录中的主程序，在管理员 PowerShell 运行：

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\input-service.ps1 -Action Uninstall
```

仅移除本服务、安装的两个程序和服务日志；保留用户设置和接收文件。之后可继续从其他目录运行普通模式的 `mwb.exe`。

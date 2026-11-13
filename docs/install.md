# 安装与更新

## 日常使用

1. 双击 `mwb-v0.5.9-windows-x64.msi`，点击「安装」并确认 Windows 管理员授权。
2. 安装包自动安装主程序和输入服务，创建桌面及开始菜单快捷方式。完成时可直接打开 MWB Rust。
3. 首次在各电脑使用相同密钥创建/加入工作组。已有配对、布局及偏好会继续使用，下次打开自动连接。

更新时运行新版本 MSI 即可。程序正在使用时，按安装向导提示关闭后继续。标准安装位置为系统 Program Files 下的 `MWB-Rust`；快捷方式始终指向安装版，方便使用管理员窗口及锁屏输入服务。

同一版本安装包提供「修复」和「卸载」。也可在 Windows「设置 → 应用 → 安装的应用 → MWB Rust」卸载。卸载会停止并移除输入服务、程序和快捷方式，保留用户配置与已接收文件。

旧版本通过 PowerShell 脚本安装的服务可直接由 MSI 接管；接管后继续使用 MSI 更新或卸载。便携控制端仍可直接运行 `mwb.exe`，需要锁屏或管理员窗口控制的接收端使用安装版。

首次联网时按 Windows 提示允许 MWB 通过专用网络防火墙，详见 README 的端口说明。安装不会更改网络类别、防火墙、配对密钥或用户设置。

## 开机自启动

安装后从桌面快捷方式打开 MWB，在「设置」开启「开机自启动」。默认关闭；安装和修复不会替用户开启。此设置只影响当前 Windows 用户，无需管理员权限。下次登录后会打开普通主窗口，是否恢复工作组由独立的「启动时自动连接」决定；网络尚未就绪时沿用已有自动重连。关闭窗口仍然退出，首次登录前不会连接。

程序使用当前用户的 `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`，仅管理名为 `MWB Rust` 的值，命令为带双引号的完整程序路径及 `--autostart`，不保存配对密钥或配置路径。界面从实际启动项读取状态，不在用户配置中另存开关；普通启动只读取，不重新写入。开关开启表示启动项已登记；如果在 Windows「设置 → 应用 → 启动」或任务管理器中禁用了它，需在那里重新允许，程序不会修改 Windows 的禁用记录。系统可能延迟登录启动，见 [Microsoft Run 文档](https://learn.microsoft.com/en-us/windows/win32/setupapi/run-and-runonce-registry-keys)。

便携版也可开启，请保持程序位置稳定。已有启动项指向其他位置时，界面会显示提示；重新开启「开机自启动」可改用当前版本，也可点击「关闭其他位置的启动项」。移动/删除便携版之前应先关闭自启动；`MWB_CONFIG_DIR` 仅隔离应用配置和单实例锁，不隔离当前 Windows 用户的自启动项，因此不要在真实用户下为测试副本开启此开关。

MSI 更新保留自启动登记。卸载在删除程序前尝试清理执行卸载账户的启动项，且仅在仍指向安装版时移除；其他用户账户的启动项需分别在各自账户中关闭，使用另一管理员账户卸载时也应先在原账户关闭。配对、布局及接收文件继续保留。

同一配置目录只运行一个主程序实例；重复打开会尝试还原并激活已有窗口。锁文件 `%LOCALAPPDATA%\MWB-Rust\mwb.lock` 只保存进程号，由 Windows 文件共享权限保证排他；进程异常退出会自动释放，无需手动删除锁文件。不同 `MWB_CONFIG_DIR` 仍可用于隔离验证。

## 安装构建

构建环境需要 Rust 1.95+ MSVC、Windows SDK 的资源编译器以及 WiX Toolset 3.14。运行：

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\build-installer.ps1
```

也可用 `-WixDirectory` 指定包含 `candle.exe` 和 `light.exe` 的目录。脚本先构建两个程序，核验 x64 PE 格式和 Cargo 对应的 Windows 产品版本，再编译并严格验证 MSI，输出 `artifacts\mwb-v版本-windows-x64.msi` 及 SHA-256 校验文件。已完成同一版本构建时可用 `-SkipBuild`，版本不匹配仍会拒绝打包。

`build.rs` 使用 Windows SDK `rc.exe` 为两个程序嵌入真实文件/产品版本。资源编译器从 `RC`、PATH、`WindowsSdkDir` 或常规 Windows Kits 目录寻找。

MSI 使用固定 UpgradeCode 和稳定组件标识，自动处理服务停止、文件替换、重启服务、修复与卸载。权限由 Windows Installer 的 `MsiLockPermissionsEx` 表设置：只有管理员和 SYSTEM 可写，普通用户可读及执行。程序实际文件版本用于更新判断，避免通过虚报 MSI 文件版本强制覆盖。

安装前通过嵌入的 `mwb-input-service.exe --check-installation` 做只读检查：安装位置必须匹配系统 Program Files；目录及上级不允许重解析点；目录仅允许两个程序和服务日志，文件拒绝硬链接；同名服务必须使用 LocalSystem、正确参数及该安装目录。检查发生在安装事务及旧版移除之前；用户配置不属于 MSI 文件表或删除表。

完成页通过用户界面的 `WixShellExec` 打开程序，沿用启动安装向导的用户身份，避免把配对保存到其他管理员账户。静默安装不打开主程序。

卸载动作通过主程序的 `--remove-autostart` 入口做当前账户的精确匹配清理，不打开窗口、不安装钩子；仅在 `REMOVE="ALL"` 且不是升级卸载时运行，清理失败不阻止程序卸载。

旧脚本保留给开发/便携部署；检测到 MSI 的安装标记后，实际脚本安装和卸载会拒绝继续，避免文件所有权与 Windows Installer 状态不一致。

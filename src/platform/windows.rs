use super::{InputInjectionError, InputServiceError};
use crate::{
    engine::Runtime,
    layout::{DeviceInfo, Point, Rect, Screen},
    motion,
    protocol::{InputEvent, MouseMode},
};
use ::windows::Win32::{
    Foundation::{ERROR_SUCCESS, GetLastError, LPARAM, LRESULT, POINT, SetLastError, WPARAM},
    Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFOEXW},
    System::{
        LibraryLoader::GetModuleHandleW,
        StationsAndDesktops::{GetThreadDesktop, GetUserObjectInformationW, UOI_IO},
        Threading::{
            GetCurrentThread, GetCurrentThreadId, SetThreadPriority, THREAD_PRIORITY_HIGHEST,
        },
    },
    UI::{HiDpi::*, Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
};
use anyhow::{Result, bail, ensure};
use std::{
    cell::Cell,
    collections::HashSet,
    mem::size_of,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, AtomicU8, Ordering},
        mpsc,
    },
    thread,
};

const MARKER: usize = 0x4D574252;
static RUNTIME: OnceLock<Arc<Runtime>> = OnceLock::new();
static HOOKS_READY: AtomicBool = AtomicBool::new(false);
// Suppressed keystrokes do not update GetAsyncKeyState. Track physical modifiers
// here so the emergency chord also works while keyboard forwarding is active.
static MODIFIERS: AtomicU8 = AtomicU8::new(0);
thread_local! {
    static LAST_DIRECTION: Cell<Point> = const { Cell::new(Point { x: 1.0, y: 0.0 }) };
}

pub fn install_hooks(runtime: Arc<Runtime>) -> Result<()> {
    if HOOKS_READY.load(Ordering::SeqCst) {
        return Ok(());
    }
    RUNTIME.get_or_init(|| runtime);
    let (tx, rx) = mpsc::sync_channel(1);
    thread::spawn(move || unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
        let result = (|| -> Result<_> {
            let module = GetModuleHandleW(None)?;
            let mouse = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), Some(module.into()), 0)?;
            match SetWindowsHookExW(WH_KEYBOARD_LL, Some(key_hook), Some(module.into()), 0) {
                Ok(keyboard) => Ok((mouse, keyboard)),
                Err(err) => {
                    let _ = UnhookWindowsHookEx(mouse);
                    Err(err.into())
                }
            }
        })();
        match result {
            Ok((mouse, keyboard)) => {
                HOOKS_READY.store(true, Ordering::SeqCst);
                let _ = tx.send(Ok(()));
                let mut msg = MSG::default();
                while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
                let _ = UnhookWindowsHookEx(mouse);
                let _ = UnhookWindowsHookEx(keyboard);
                HOOKS_READY.store(false, Ordering::SeqCst);
                if let Some(rt) = RUNTIME.get() {
                    rt.reclaim();
                }
            }
            Err(err) => {
                let _ = tx.send(Err(err));
            }
        }
    });
    rx.recv().map_err(|_| anyhow::anyhow!("输入线程启动失败"))?
}

struct PhysicalDpiScope(DPI_AWARENESS_CONTEXT);

impl PhysicalDpiScope {
    fn new() -> Self {
        Self(unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) })
    }
}

impl Drop for PhysicalDpiScope {
    fn drop(&mut self) {
        if !self.0.0.is_null() {
            unsafe {
                SetThreadDpiAwarenessContext(self.0);
            }
        }
    }
}

fn desktop() -> Rect {
    let _dpi = PhysicalDpiScope::new();
    unsafe {
        Rect {
            x: GetSystemMetrics(SM_XVIRTUALSCREEN),
            y: GetSystemMetrics(SM_YVIRTUALSCREEN),
            width: GetSystemMetrics(SM_CXVIRTUALSCREEN).max(2),
            height: GetSystemMetrics(SM_CYVIRTUALSCREEN).max(2),
        }
    }
}
fn primary() -> Rect {
    let _dpi = PhysicalDpiScope::new();
    unsafe {
        Rect {
            x: 0,
            y: 0,
            width: GetSystemMetrics(SM_CXSCREEN).max(2),
            height: GetSystemMetrics(SM_CYSCREEN).max(2),
        }
    }
}
pub fn device_info(id: &str, name: &str) -> DeviceInfo {
    unsafe extern "system" fn monitor(
        monitor: HMONITOR,
        _: HDC,
        _: *mut ::windows::Win32::Foundation::RECT,
        data: LPARAM,
    ) -> ::windows::core::BOOL {
        unsafe {
            let screens = &mut *(data.0 as *mut Vec<Screen>);
            let mut info = MONITORINFOEXW::default();
            info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
            if GetMonitorInfoW(monitor, &mut info.monitorInfo).as_bool() {
                let r = info.monitorInfo.rcMonitor;
                let len = info
                    .szDevice
                    .iter()
                    .position(|c| *c == 0)
                    .unwrap_or(info.szDevice.len());
                screens.push(Screen {
                    name: String::from_utf16_lossy(&info.szDevice[..len]),
                    rect: Rect {
                        x: r.left,
                        y: r.top,
                        width: r.right - r.left,
                        height: r.bottom - r.top,
                    },
                    primary: info.monitorInfo.dwFlags & 1 != 0,
                });
            }
            true.into()
        }
    }
    let _dpi = PhysicalDpiScope::new();
    let mut screens = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(monitor),
            LPARAM((&mut screens as *mut Vec<Screen>) as isize),
        );
    }
    if screens.is_empty() {
        screens.push(Screen {
            name: "Primary".into(),
            rect: primary(),
            primary: true,
        });
    }
    DeviceInfo {
        id: id.into(),
        name: name.into(),
        bounds: desktop(),
        screens,
    }
}
pub fn cursor_position() -> Result<Point> {
    let mut p = POINT::default();
    unsafe {
        GetPhysicalCursorPos(&mut p)?;
    }
    Ok(Point {
        x: p.x as f64,
        y: p.y as f64,
    })
}
pub fn park_cursor() -> Result<()> {
    move_absolute(desktop(), primary().center())
}

/// Authenticate the installed broker without installing hooks or injecting input.
pub fn input_service_ready() -> Result<bool> {
    if let Some(mut client) = crate::input_service::Client::connect()? {
        client.release();
        Ok(true)
    } else {
        Ok(false)
    }
}
fn move_absolute(bounds: Rect, point: Point) -> Result<()> {
    let (x, y) = motion::absolute(bounds, point);
    mouse_input(
        x,
        y,
        0,
        MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
    )
}

unsafe extern "system" fn mouse_hook(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if code < 0 {
            return CallNextHookEx(None, code, wp, lp);
        }
        let event = &*(lp.0 as *const MSLLHOOKSTRUCT);
        if event.dwExtraInfo == MARKER || event.flags & 1 != 0 {
            return CallNextHookEx(None, code, wp, lp);
        }
        let Some(rt) = RUNTIME.get() else {
            return CallNextHookEx(None, code, wp, lp);
        };
        if !rt.sharing_enabled() {
            return CallNextHookEx(None, code, wp, lp);
        }
        if rt.incoming.load(Ordering::Relaxed) {
            rt.return_local();
            return CallNextHookEx(None, code, wp, lp);
        }
        let kind = wp.0 as u32;
        if !rt.remote.load(Ordering::Relaxed)
            && kind == WM_MOUSEMOVE
            && let Ok(previous) = cursor_position()
        {
            let p = Point {
                x: event.pt.x as f64,
                y: event.pt.y as f64,
            };
            let mut delta = Point {
                x: p.x - previous.x,
                y: p.y - previous.y,
            };
            LAST_DIRECTION.with(|last| {
                if delta.x == 0.0 && delta.y == 0.0 {
                    delta = last.get();
                } else {
                    last.set(delta);
                }
            });
            if rt.edge_at(p, delta) {
                if park_cursor().is_err() {
                    rt.reclaim();
                    return CallNextHookEx(None, code, wp, lp);
                }
                return LRESULT(1);
            }
        }
        if !rt.remote.load(Ordering::Relaxed) {
            return CallNextHookEx(None, code, wp, lp);
        }
        let message = match kind {
            WM_MOUSEMOVE => {
                let mut anchor = POINT::default();
                if GetPhysicalCursorPos(&mut anchor).is_err() {
                    rt.reclaim();
                    return CallNextHookEx(None, code, wp, lp);
                }
                return if rt.capture(
                    event.pt.x.saturating_sub(anchor.x),
                    event.pt.y.saturating_sub(anchor.y),
                ) {
                    LRESULT(1)
                } else {
                    CallNextHookEx(None, code, wp, lp)
                };
            }

            WM_LBUTTONDOWN | WM_LBUTTONUP => InputEvent::Button {
                button: 1,
                down: kind == WM_LBUTTONDOWN,
            },
            WM_RBUTTONDOWN | WM_RBUTTONUP => InputEvent::Button {
                button: 2,
                down: kind == WM_RBUTTONDOWN,
            },
            WM_MBUTTONDOWN | WM_MBUTTONUP => InputEvent::Button {
                button: 3,
                down: kind == WM_MBUTTONDOWN,
            },
            WM_XBUTTONDOWN | WM_XBUTTONUP => InputEvent::Button {
                button: if event.mouseData >> 16 == 1 { 4 } else { 5 },
                down: kind == WM_XBUTTONDOWN,
            },
            WM_MOUSEWHEEL | WM_MOUSEHWHEEL => InputEvent::Wheel {
                delta: (event.mouseData >> 16) as i16 as i32,
                horizontal: kind == WM_MOUSEHWHEEL,
            },
            _ => return CallNextHookEx(None, code, wp, lp),
        };
        if rt.send_input(message) {
            LRESULT(1)
        } else {
            CallNextHookEx(None, code, wp, lp)
        }
    }
}

unsafe extern "system" fn key_hook(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if code < 0 {
            return CallNextHookEx(None, code, wp, lp);
        }
        let event = &*(lp.0 as *const KBDLLHOOKSTRUCT);
        if event.dwExtraInfo == MARKER || event.flags.contains(LLKHF_INJECTED) {
            return CallNextHookEx(None, code, wp, lp);
        }
        let Some(rt) = RUNTIME.get() else {
            return CallNextHookEx(None, code, wp, lp);
        };
        let down = matches!(wp.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
        let modifier = match event.vkCode {
            0x11 | 0xA2 => 1,
            0xA3 => 2,
            0x12 | 0xA4 => 4,
            0xA5 => 8,
            _ => 0,
        };
        if down {
            MODIFIERS.fetch_or(modifier, Ordering::Relaxed);
        } else {
            MODIFIERS.fetch_and(!modifier, Ordering::Relaxed);
        }
        let modifiers = MODIFIERS.load(Ordering::Relaxed);
        if event.vkCode == VK_HOME.0 as u32
            && down
            && (modifiers & 3 != 0 || GetAsyncKeyState(VK_CONTROL.0 as i32) < 0)
            && (modifiers & 12 != 0 || GetAsyncKeyState(VK_MENU.0 as i32) < 0)
        {
            rt.reclaim();
            return LRESULT(1);
        }
        if !rt.sharing_enabled() {
            return CallNextHookEx(None, code, wp, lp);
        }
        if rt.incoming.load(Ordering::Relaxed) {
            rt.return_local();
            return CallNextHookEx(None, code, wp, lp);
        }
        if rt.remote.load(Ordering::Relaxed)
            && rt.keyboard.load(Ordering::Relaxed)
            && rt.send_input(InputEvent::Key {
                vk: event.vkCode as u16,
                scan: event.scanCode as u16,
                extended: event.flags.contains(LLKHF_EXTENDED),
                down,
            })
        {
            return LRESULT(1);
        }
        CallNextHookEx(None, code, wp, lp)
    }
}

fn inject_input(input: INPUT, kind: &'static str) -> Result<()> {
    // Capture the error immediately, without attributing every rejection to
    // UIPI. Windows does not reliably report UIPI as a distinct error code.
    unsafe {
        SetLastError(ERROR_SUCCESS);
    }
    let inserted = unsafe { SendInput(&[input], size_of::<INPUT>() as i32) };
    if inserted != 1 {
        let code = unsafe { GetLastError() }.0;
        return Err(InputInjectionError::new(kind, inserted, code).into());
    }
    Ok(())
}

fn mouse_input(dx: i32, dy: i32, data: u32, flags: MOUSE_EVENT_FLAGS) -> Result<()> {
    let mouse = MOUSEINPUT {
        dx,
        dy,
        mouseData: data,
        dwFlags: flags,
        time: 0,
        dwExtraInfo: MARKER,
    };
    // The legacy private InjectMouseInput export can exist but reject the
    // standard MOUSEINPUT ABI (observed error 87). Use the documented API for
    // both absolute and relative motion, retaining the original coordinate modes.
    inject_input(
        INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 { mi: mouse },
        },
        "鼠标",
    )
}

fn key_input(vk: u16, scan: u16, extended: bool, down: bool) -> Result<()> {
    let mut flags = KEYBD_EVENT_FLAGS(0);
    if scan != 0 {
        flags |= KEYEVENTF_SCANCODE;
    }
    if extended {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if !down {
        flags |= KEYEVENTF_KEYUP;
    }
    let input = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(if scan == 0 { vk } else { 0 }),
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: MARKER,
            },
        },
    };
    inject_input(input, "键盘")
}

fn button_input(button: u8, down: bool) -> Result<()> {
    let (flag, data) = match (button, down) {
        (1, true) => (MOUSEEVENTF_LEFTDOWN, 0),
        (1, false) => (MOUSEEVENTF_LEFTUP, 0),
        (2, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
        (2, false) => (MOUSEEVENTF_RIGHTUP, 0),
        (3, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
        (3, false) => (MOUSEEVENTF_MIDDLEUP, 0),
        (4, true) => (MOUSEEVENTF_XDOWN, 1),
        (4, false) => (MOUSEEVENTF_XUP, 1),
        (5, true) => (MOUSEEVENTF_XDOWN, 2),
        (5, false) => (MOUSEEVENTF_XUP, 2),
        _ => bail!("无效的鼠标按键"),
    };
    mouse_input(0, 0, data, flag)
}

#[derive(Default)]
pub struct Injector {
    native: NativeInjector,
    service: Option<crate::input_service::Client>,
}

impl Injector {
    pub fn enter_at(&mut self, point: Point, mode: MouseMode) -> Result<()> {
        self.release_all();
        self.service = crate::input_service::Client::connect()
            .map_err(|e| InputServiceError(format!("锁屏输入服务连接失败：{e:#}")))?;
        if let Some(service) = self.service.as_mut() {
            service.enter(point, mode)
        } else {
            let mut active = ::windows::core::BOOL::default();
            let accessible = unsafe {
                GetThreadDesktop(GetCurrentThreadId()).is_ok_and(|desktop| {
                    GetUserObjectInformationW(
                        ::windows::Win32::Foundation::HANDLE(desktop.0),
                        UOI_IO,
                        Some((&mut active as *mut ::windows::core::BOOL).cast()),
                        size_of::<::windows::core::BOOL>() as u32,
                        None,
                    )
                    .is_ok()
                        && active.as_bool()
                })
            };
            if !accessible {
                return Err(InputServiceError(
                    "当前桌面不可访问；锁屏控制需要在被控端安装输入服务，并运行安装目录中的 MWB"
                        .into(),
                )
                .into());
            }
            self.native.enter_at(point, mode)
        }
    }
    pub fn apply_event(&mut self, event: InputEvent, keyboard: bool) -> Result<()> {
        if let Some(service) = self.service.as_mut() {
            service.apply(event, keyboard)
        } else {
            self.native.apply_event(event, keyboard)
        }
    }
    pub fn position(&mut self) -> Result<Point> {
        if let Some(service) = self.service.as_mut() {
            service.position()
        } else {
            cursor_position()
        }
    }
    pub fn release_all(&mut self) {
        if let Some(mut service) = self.service.take() {
            service.release();
        }
        self.native.release_all();
    }
}
impl Drop for Injector {
    fn drop(&mut self) {
        self.release_all();
    }
}

#[derive(Default)]
pub(crate) struct NativeInjector {
    keys: HashSet<(u16, u16, bool)>,
    buttons: HashSet<u8>,
    mode: Option<MouseMode>,
}
impl NativeInjector {
    pub fn enter_at(&mut self, point: Point, mode: MouseMode) -> Result<()> {
        self.release_all();
        self.mode = Some(mode);
        ensure!(desktop().contains(point), "鼠标位置超出本机桌面");
        move_absolute(desktop(), point)
    }
    pub fn apply_event(&mut self, event: InputEvent, keyboard: bool) -> Result<()> {
        match event {
            InputEvent::Relative { dx, dy } => {
                ensure!(self.mode == Some(MouseMode::Relative), "鼠标模式不匹配");
                ensure!(
                    dx.unsigned_abs() <= 32768 && dy.unsigned_abs() <= 32768,
                    "鼠标位移无效"
                );
                mouse_input(dx, dy, 0, MOUSEEVENTF_MOVE)?;
            }
            InputEvent::Absolute { x, y } => {
                ensure!(
                    self.mode == Some(MouseMode::ScreenRelative),
                    "鼠标模式不匹配"
                );
                let p = Point {
                    x: x as f64,
                    y: y as f64,
                };
                let bounds = desktop();
                ensure!(bounds.contains(p), "鼠标坐标无效");
                move_absolute(bounds, p)?;
            }
            InputEvent::Button { button, down } => {
                button_input(button, down)?;
                if down {
                    self.buttons.insert(button);
                } else {
                    self.buttons.remove(&button);
                }
            }
            InputEvent::Wheel { delta, horizontal } => {
                ensure!(delta.unsigned_abs() <= 32768, "滚轮位移无效");
                mouse_input(
                    0,
                    0,
                    delta as u32,
                    if horizontal {
                        MOUSEEVENTF_HWHEEL
                    } else {
                        MOUSEEVENTF_WHEEL
                    },
                )?;
            }
            InputEvent::Key {
                vk,
                scan,
                extended,
                down,
            } if keyboard => {
                ensure!(vk <= 255 && scan <= 255, "键码无效");
                key_input(vk, scan, extended, down)?;
                if down {
                    self.keys.insert((vk, scan, extended));
                } else {
                    self.keys.remove(&(vk, scan, extended));
                }
            }
            _ => {}
        }
        Ok(())
    }

    pub fn release_all(&mut self) {
        self.mode = None;
        for (vk, scan, extended) in self.keys.drain() {
            let _ = key_input(vk, scan, extended, false);
        }
        for button in self.buttons.drain() {
            let _ = button_input(button, false);
        }
    }
}

impl Drop for NativeInjector {
    fn drop(&mut self) {
        self.release_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monitor_inventory_is_valid_in_physical_pixel_space() {
        let info = device_info("00000000000000000000000000000001", "display-test");
        info.validate().unwrap();
        assert!(info.screens.iter().any(|s| s.primary));
        assert_eq!(info.bounds, desktop());
    }

    #[test]
    #[ignore = "requires an interactive Windows desktop; sends only a zero-distance move"]
    fn standard_mouse_api_accepts_zero_delta() {
        mouse_input(0, 0, 0, MOUSEEVENTF_MOVE)
            .expect("standard SendInput zero-delta injection failed");
    }
}

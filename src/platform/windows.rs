use super::InputInjectionError;
use crate::{
    engine::Runtime,
    motion::{CaptureMotion, DesktopBounds, MotionStep, PixelPoint, PointerMotion, SWITCH_DELAY},
    protocol::{Message, MouseMode},
};
use ::windows::Win32::{
    Foundation::{ERROR_SUCCESS, GetLastError, LPARAM, LRESULT, POINT, SetLastError, WPARAM},
    System::{
        LibraryLoader::GetModuleHandleW,
        Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST},
    },
    UI::{HiDpi::*, Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
};
use anyhow::{Result, bail, ensure};
use std::{
    cell::RefCell,
    collections::HashSet,
    mem::size_of,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, AtomicU8, Ordering},
        mpsc,
    },
    thread,
    time::Instant,
};

const MARKER: usize = 0x4D574252;
static RUNTIME: OnceLock<Arc<Runtime>> = OnceLock::new();
static HOOKS_READY: AtomicBool = AtomicBool::new(false);
// Suppressed keystrokes do not update GetAsyncKeyState. Track physical modifiers
// here so the emergency chord also works while keyboard forwarding is active.
static MODIFIERS: AtomicU8 = AtomicU8::new(0);
thread_local! {
    static CAPTURE: RefCell<Option<(u64, CaptureMotion)>> = const { RefCell::new(None) };
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

fn desktop() -> DesktopBounds {
    // Low-level hook points are physical pixels. Read desktop metrics in the
    // same coordinate space, restoring the UI thread's DPI context afterwards.
    let _dpi = PhysicalDpiScope::new();
    unsafe {
        DesktopBounds::new(
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN).max(2),
            GetSystemMetrics(SM_CYVIRTUALSCREEN).max(2),
        )
    }
}

fn primary() -> DesktopBounds {
    let _dpi = PhysicalDpiScope::new();
    unsafe {
        DesktopBounds::new(
            0,
            0,
            GetSystemMetrics(SM_CXSCREEN),
            GetSystemMetrics(SM_CYSCREEN),
        )
    }
}

fn cursor_position() -> Result<PixelPoint> {
    let mut point = POINT::default();
    unsafe {
        GetPhysicalCursorPos(&mut point)?;
    }
    Ok(PixelPoint {
        x: point.x,
        y: point.y,
    })
}

pub fn control_remote(rt: &Runtime) -> Result<()> {
    ensure!(rt.enabled.load(Ordering::SeqCst), "请先连接并开启共享");
    let point = cursor_position()?;
    if rt.begin_control(
        primary().entry_ratio(rt.right.load(Ordering::SeqCst)),
        desktop().height_ratio(point.y),
    ) && let Err(err) = park_cursor()
    {
        rt.reclaim();
        return Err(err);
    }
    Ok(())
}

fn capture_move(rt: &Runtime, dx: i32, dy: i32) -> Result<Message> {
    CAPTURE.with_borrow_mut(|capture| {
        let epoch = rt.capture_epoch.load(Ordering::SeqCst);
        if capture.as_ref().is_none_or(|(old, _)| *old != epoch) {
            *capture = Some((
                epoch,
                CaptureMotion::enter(
                    primary(),
                    rt.right.load(Ordering::SeqCst),
                    rt.capture_y.load(Ordering::SeqCst),
                    if rt.capture_relative.load(Ordering::SeqCst) {
                        MouseMode::Relative
                    } else {
                        MouseMode::ScreenRelative
                    },
                ),
            ));
        }
        capture
            .as_mut()
            .expect("initialized capture")
            .1
            .advance(dx, dy)
    })
}

pub fn park_cursor() -> Result<()> {
    let bounds = desktop();
    move_absolute(bounds, bounds.center())
}

fn move_absolute(bounds: DesktopBounds, point: PixelPoint) -> Result<()> {
    let (x, y) = bounds.absolute(point);
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
        if !rt.enabled.load(Ordering::Relaxed) {
            return CallNextHookEx(None, code, wp, lp);
        }
        if rt.incoming.load(Ordering::Relaxed) {
            rt.return_local();
            return CallNextHookEx(None, code, wp, lp);
        }
        let kind = wp.0 as u32;
        if !rt.remote.load(Ordering::Relaxed)
            && kind == WM_MOUSEMOVE
            && rt.edge.load(Ordering::Relaxed)
            && rt.can_switch()
        {
            let bounds = desktop();
            let right = rt.right.load(Ordering::Relaxed);
            if ((right && event.pt.x >= bounds.right()) || (!right && event.pt.x <= bounds.left))
                && rt.begin_control(
                    primary().entry_ratio(right),
                    bounds.height_ratio(event.pt.y),
                )
            {
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
                match capture_move(
                    rt,
                    event.pt.x.saturating_sub(anchor.x),
                    event.pt.y.saturating_sub(anchor.y),
                ) {
                    Ok(message) => message,
                    Err(_) => {
                        rt.reclaim();
                        return CallNextHookEx(None, code, wp, lp);
                    }
                }
            }
            WM_LBUTTONDOWN | WM_LBUTTONUP => Message::Button {
                button: 1,
                down: kind == WM_LBUTTONDOWN,
            },
            WM_RBUTTONDOWN | WM_RBUTTONUP => Message::Button {
                button: 2,
                down: kind == WM_RBUTTONDOWN,
            },
            WM_MBUTTONDOWN | WM_MBUTTONUP => Message::Button {
                button: 3,
                down: kind == WM_MBUTTONDOWN,
            },
            WM_XBUTTONDOWN | WM_XBUTTONUP => Message::Button {
                button: if event.mouseData >> 16 == 1 { 4 } else { 5 },
                down: kind == WM_XBUTTONDOWN,
            },
            WM_MOUSEWHEEL | WM_MOUSEHWHEEL => Message::Wheel {
                delta: (event.mouseData >> 16) as i16 as i32,
                horizontal: kind == WM_MOUSEHWHEEL,
            },
            _ => return CallNextHookEx(None, code, wp, lp),
        };
        if rt.send(message) {
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
        if !rt.enabled.load(Ordering::Relaxed) {
            return CallNextHookEx(None, code, wp, lp);
        }
        if rt.incoming.load(Ordering::Relaxed) {
            rt.return_local();
            return CallNextHookEx(None, code, wp, lp);
        }
        if rt.remote.load(Ordering::Relaxed)
            && rt.keyboard.load(Ordering::Relaxed)
            && rt.send(Message::Key {
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
    keys: HashSet<(u16, u16, bool)>,
    buttons: HashSet<u8>,
    motion: PointerMotion,
    entered_at: Option<Instant>,
}

impl Injector {
    pub fn enter(&mut self, from_left: bool, x: u16, y: u16, mode: MouseMode) -> Result<()> {
        self.release_all();
        let bounds = desktop();
        let point = self.motion.enter(bounds, from_left, y, mode, x);
        self.entered_at = Some(Instant::now());
        move_absolute(bounds, point)
    }

    pub fn return_cursor(&mut self, peer_on_right: bool, y: u16) -> Result<()> {
        let bounds = desktop();
        move_absolute(bounds, bounds.entry(!peer_on_right, y))
    }

    fn can_return(&self) -> bool {
        self.entered_at
            .is_some_and(|entered| entered.elapsed() >= SWITCH_DELAY)
    }

    pub fn poll_return(&self) -> Result<Option<u16>> {
        if self.motion.is_relative() && self.can_return() {
            Ok(self.motion.observe(desktop(), cursor_position()?))
        } else {
            Ok(None)
        }
    }

    /// Returns the crossing height when control should return to the source PC.
    pub fn apply(&mut self, message: Message, keyboard: bool) -> Result<Option<u16>> {
        match message {
            Message::Move { dx, dy } => {
                self.motion.relative(dx, dy)?;
                mouse_input(dx, dy, 0, MOUSEEVENTF_MOVE)?;
            }
            Message::MoveAbsolute { x, y } => {
                let bounds = desktop();
                match self.motion.absolute(bounds, x, y, self.can_return())? {
                    MotionStep::ReturnLocal { y } => return Ok(Some(y)),
                    MotionStep::MoveTo(point) => move_absolute(bounds, point)?,
                }
            }
            Message::Button { button, down } => {
                button_input(button, down)?;
                if down {
                    self.buttons.insert(button);
                } else {
                    self.buttons.remove(&button);
                }
            }
            Message::Wheel { delta, horizontal } => {
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
            Message::Key {
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
        Ok(None)
    }

    pub fn release_all(&mut self) {
        // Control ownership is gated by Runtime. A fresh Enter replaces the
        // motion origin; releasing held keys must not erase a newer entry's
        // position if a pending local-return notification is processed later.
        for (vk, scan, extended) in self.keys.drain() {
            let _ = key_input(vk, scan, extended, false);
        }
        for button in self.buttons.drain() {
            let _ = button_input(button, false);
        }
    }
}

impl Drop for Injector {
    fn drop(&mut self) {
        self.release_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires an interactive Windows desktop; sends only a zero-distance move"]
    fn standard_mouse_api_accepts_zero_delta() {
        mouse_input(0, 0, 0, MOUSEEVENTF_MOVE)
            .expect("standard SendInput zero-delta injection failed");
    }
}

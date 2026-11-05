use crate::{
    engine::Runtime,
    motion::{DesktopBounds, MotionStep, PixelPoint, PointerMotion},
    protocol::Message,
};
use ::windows::Win32::{
    Foundation::{LPARAM, LRESULT, POINT, WPARAM},
    System::LibraryLoader::GetModuleHandleW,
    UI::{HiDpi::*, Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
};
use anyhow::{Result, bail, ensure};
use std::{
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

pub fn install_hooks(runtime: Arc<Runtime>) -> Result<()> {
    if HOOKS_READY.load(Ordering::SeqCst) {
        return Ok(());
    }
    RUNTIME.get_or_init(|| runtime);
    let (tx, rx) = mpsc::sync_channel(1);
    thread::spawn(move || unsafe {
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

pub fn park_cursor() {
    let bounds = desktop();
    let _ = move_absolute(bounds, bounds.center());
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
        {
            let bounds = desktop();
            let right = rt.right.load(Ordering::Relaxed);
            if ((right && event.pt.x >= bounds.right()) || (!right && event.pt.x <= bounds.left))
                && rt.send(Message::Enter { from_left: right })
            {
                rt.remote.store(true, Ordering::SeqCst);
                park_cursor();
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
                Message::Move {
                    dx: event.pt.x.saturating_sub(anchor.x),
                    dy: event.pt.y.saturating_sub(anchor.y),
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

fn mouse_input(dx: i32, dy: i32, data: u32, flags: MOUSE_EVENT_FLAGS) -> Result<()> {
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: MARKER,
            },
        },
    };
    ensure!(
        unsafe { SendInput(&[input], size_of::<INPUT>() as i32) } == 1,
        "Windows 拒绝输入注入；请检查两端权限级别"
    );
    Ok(())
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
    ensure!(
        unsafe { SendInput(&[input], size_of::<INPUT>() as i32) } == 1,
        "Windows 拒绝键盘输入"
    );
    Ok(())
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
}

impl Injector {
    pub fn enter(&mut self, from_left: bool) -> Result<()> {
        self.release_all();
        let bounds = desktop();
        let point = self.motion.enter(bounds, from_left);
        move_absolute(bounds, point)
    }

    /// Returns true when the pointer crosses the edge back to its source computer.
    pub fn apply(&mut self, message: Message, keyboard: bool) -> Result<bool> {
        match message {
            Message::Move { dx, dy } => {
                let bounds = desktop();
                match self.motion.advance(bounds, dx, dy)? {
                    MotionStep::ReturnLocal => return Ok(true),
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
        Ok(false)
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

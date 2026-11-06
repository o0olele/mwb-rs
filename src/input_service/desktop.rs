use super::{pipe, session::Backend};
use crate::{
    layout::Point,
    platform::{self, NativeInjector},
    protocol::{InputEvent, MouseMode},
};
use anyhow::{Context, Result};
use std::{
    mem::size_of,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};
use windows::Win32::{
    Foundation::{HANDLE, LPARAM, LRESULT, WPARAM},
    System::{
        LibraryLoader::GetModuleHandleW, StationsAndDesktops::*, Threading::GetCurrentThreadId,
    },
    UI::WindowsAndMessaging::*,
};

static ACTIVE: AtomicBool = AtomicBool::new(false);
static TAKEOVER: AtomicBool = AtomicBool::new(false);

unsafe extern "system" fn mouse_hook(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if code >= 0 && ACTIVE.load(Ordering::Relaxed) {
            let event = &*(lp.0 as *const MSLLHOOKSTRUCT);
            if event.flags & 1 == 0 {
                TAKEOVER.store(true, Ordering::SeqCst);
            }
        }
        CallNextHookEx(None, code, wp, lp)
    }
}
unsafe extern "system" fn key_hook(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if code >= 0 && ACTIVE.load(Ordering::Relaxed) {
            let event = &*(lp.0 as *const KBDLLHOOKSTRUCT);
            if !event.flags.contains(LLKHF_INJECTED) {
                TAKEOVER.store(true, Ordering::SeqCst);
            }
        }
        CallNextHookEx(None, code, wp, lp)
    }
}

/// This thread owns no windows. Hooks are removed before SetThreadDesktop and
/// recreated afterwards; the GPUI thread and network threads never move desks.
pub(super) struct DesktopInput {
    native: NativeInjector,
    original: HDESK,
    current: Option<HDESK>,
    mouse: Option<HHOOK>,
    keyboard: Option<HHOOK>,
}

impl DesktopInput {
    pub fn new() -> Result<Self> {
        Ok(Self {
            native: NativeInjector::default(),
            original: unsafe { GetThreadDesktop(GetCurrentThreadId())? },
            current: None,
            mouse: None,
            keyboard: None,
        })
    }

    fn is_active(&self) -> bool {
        let Some(desktop) = self.current else {
            return false;
        };
        let mut active = windows::core::BOOL::default();
        unsafe {
            GetUserObjectInformationW(
                HANDLE(desktop.0),
                UOI_IO,
                Some((&mut active as *mut windows::core::BOOL).cast()),
                size_of::<windows::core::BOOL>() as u32,
                None,
            )
            .is_ok()
                && active.as_bool()
        }
    }

    fn unhook(&mut self) {
        unsafe {
            if let Some(hook) = self.mouse.take() {
                let _ = UnhookWindowsHookEx(hook);
            }
            if let Some(hook) = self.keyboard.take() {
                let _ = UnhookWindowsHookEx(hook);
            }
        }
    }

    fn attach(&mut self) -> Result<()> {
        unsafe {
            let next = OpenInputDesktop(
                DESKTOP_CONTROL_FLAGS(0),
                false,
                DESKTOP_ACCESS_FLAGS(
                    DESKTOP_READOBJECTS.0
                        | DESKTOP_WRITEOBJECTS.0
                        | DESKTOP_HOOKCONTROL.0
                        | DESKTOP_SWITCHDESKTOP.0,
                ),
            )?;
            if let Err(error) = SetThreadDesktop(next) {
                let _ = CloseDesktop(next);
                return Err(error.into());
            }
            if let Some(previous) = self.current.replace(next) {
                let _ = CloseDesktop(previous);
            }
            let module = GetModuleHandleW(None)?;
            self.mouse = Some(SetWindowsHookExW(
                WH_MOUSE_LL,
                Some(mouse_hook),
                Some(module.into()),
                0,
            )?);
            self.keyboard = Some(SetWindowsHookExW(
                WH_KEYBOARD_LL,
                Some(key_hook),
                Some(module.into()),
                0,
            )?);
        }
        Ok(())
    }
}

impl Backend for DesktopInput {
    fn refresh_desktop(&mut self) -> Result<bool> {
        if self.is_active() && self.mouse.is_some() && self.keyboard.is_some() {
            return Ok(false);
        }
        self.release();
        self.unhook();
        // LogonUI can briefly have no available input desktop during the switch.
        // Retry attachment, never replay an input whose insertion succeeded.
        let deadline = Instant::now() + Duration::from_millis(400);
        loop {
            match self.attach() {
                Ok(()) => return Ok(true),
                Err(error) if Instant::now() >= deadline => {
                    return Err(error).context("无法连接当前输入桌面");
                }
                Err(_) => {
                    self.unhook();
                    pipe::pump();
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
    }
    fn enter(&mut self, point: Point, mode: MouseMode) -> Result<()> {
        self.native.enter_at(point, mode)?;
        TAKEOVER.store(false, Ordering::SeqCst);
        ACTIVE.store(true, Ordering::SeqCst);
        Ok(())
    }
    fn apply(&mut self, event: InputEvent, keyboard: bool) -> Result<()> {
        self.native.apply_event(event, keyboard)
    }
    fn position(&mut self) -> Result<Point> {
        platform::cursor_position()
    }
    fn release(&mut self) {
        ACTIVE.store(false, Ordering::SeqCst);
        self.native.release_all();
    }
    fn taken_over(&mut self) -> bool {
        TAKEOVER.swap(false, Ordering::SeqCst)
    }
}

impl Drop for DesktopInput {
    fn drop(&mut self) {
        self.release();
        self.unhook();
        unsafe {
            let _ = SetThreadDesktop(self.original);
            if let Some(desktop) = self.current.take() {
                let _ = CloseDesktop(desktop);
            }
        }
    }
}

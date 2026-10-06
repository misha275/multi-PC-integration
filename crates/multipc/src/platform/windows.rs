//! Windows input: low-level keyboard/mouse hooks to capture, `SendInput` to inject.
//!
//! Low-level hooks see every keyboard and mouse attached to this PC, so all of
//! them control the shared cursor. The hook callbacks only push events into a
//! channel and return immediately; Windows drops hooks that take too long.

use super::{InputBackend, LocalEvent};
use mpc_core::geometry::{Point, Rect};
use mpc_core::protocol::{InputEvent, MouseButton};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::OnceLock;
use tokio::sync::mpsc::UnboundedSender;
use windows_sys::Win32::Foundation::{LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

/// Marks events we inject ourselves so our own hooks let them through untouched.
pub(super) const OUR_EXTRA_INFO: usize = 0x4D50_4331; // "MPC1"

static EVENTS: OnceLock<UnboundedSender<LocalEvent>> = OnceLock::new();
static CAPTURE: AtomicBool = AtomicBool::new(false);
static PARK: AtomicI64 = AtomicI64::new(0);

fn pack(p: Point) -> i64 {
    ((p.x as i64) << 32) | (p.y as u32 as i64)
}

fn unpack(v: i64) -> Point {
    Point::new((v >> 32) as i32, v as i32)
}

fn emit(ev: LocalEvent) {
    if let Some(tx) = EVENTS.get() {
        let _ = tx.send(ev);
    }
}

unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let info = &*(lparam as *const MSLLHOOKSTRUCT);
        if info.dwExtraInfo != OUR_EXTRA_INFO {
            let capture = CAPTURE.load(Ordering::Relaxed);
            let pt = Point::new(info.pt.x, info.pt.y);
            let wheel = (info.mouseData >> 16) as u16 as i16 as i32;
            let xbutton = if (info.mouseData >> 16) as u16 == XBUTTON1 { MouseButton::X1 } else { MouseButton::X2 };
            let ev = match wparam as u32 {
                WM_MOUSEMOVE => {
                    if !capture {
                        emit(LocalEvent::MouseAt(pt));
                        None
                    } else {
                        // Measure from where the cursor is now, not from the park point:
                        // a window shown on another PC may have moved it here.
                        let mut cur = POINT { x: 0, y: 0 };
                        let from =
                            if GetCursorPos(&mut cur) != 0 { Point::new(cur.x, cur.y) } else { unpack(PARK.load(Ordering::Relaxed)) };
                        let (dx, dy) = (pt.x - from.x, pt.y - from.y);
                        Some((dx != 0 || dy != 0).then_some(InputEvent::MouseMove { dx, dy }))
                    }
                }
                WM_LBUTTONDOWN => Some(Some(InputEvent::MouseButton { button: MouseButton::Left, down: true })),
                WM_LBUTTONUP => Some(Some(InputEvent::MouseButton { button: MouseButton::Left, down: false })),
                WM_RBUTTONDOWN => Some(Some(InputEvent::MouseButton { button: MouseButton::Right, down: true })),
                WM_RBUTTONUP => Some(Some(InputEvent::MouseButton { button: MouseButton::Right, down: false })),
                WM_MBUTTONDOWN => Some(Some(InputEvent::MouseButton { button: MouseButton::Middle, down: true })),
                WM_MBUTTONUP => Some(Some(InputEvent::MouseButton { button: MouseButton::Middle, down: false })),
                WM_XBUTTONDOWN => Some(Some(InputEvent::MouseButton { button: xbutton, down: true })),
                WM_XBUTTONUP => Some(Some(InputEvent::MouseButton { button: xbutton, down: false })),
                WM_MOUSEWHEEL => Some(Some(InputEvent::Wheel { dx: 0, dy: wheel })),
                WM_MOUSEHWHEEL => Some(Some(InputEvent::Wheel { dx: wheel, dy: 0 })),
                _ => None,
            };
            if capture {
                if let Some(ev) = ev {
                    if let Some(ev) = ev {
                        emit(LocalEvent::Captured(ev));
                    }
                    return 1; // swallow: the event belongs to another machine
                }
            }
        }
    }
    CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam)
}

unsafe extern "system" fn keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 && CAPTURE.load(Ordering::Relaxed) {
        let info = &*(lparam as *const KBDLLHOOKSTRUCT);
        if info.dwExtraInfo != OUR_EXTRA_INFO {
            let down = info.flags & LLKHF_UP == 0;
            if info.vkCode == VK_SCROLL as u32 {
                if down {
                    emit(LocalEvent::ReturnHotkey);
                }
            } else {
                emit(LocalEvent::Captured(InputEvent::Key {
                    scan: info.scanCode as u16,
                    extended: info.flags & LLKHF_EXTENDED != 0,
                    vk: info.vkCode as u16,
                    down,
                }));
            }
            let _ = wparam;
            return 1;
        }
    }
    CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam)
}

unsafe extern "system" fn monitor_proc(mon: HMONITOR, _hdc: HDC, _rect: *mut RECT, data: LPARAM) -> windows_sys::core::BOOL {
    let out = &mut *(data as *mut Vec<Rect>);
    let mut info: MONITORINFO = std::mem::zeroed();
    info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    if GetMonitorInfoW(mon, &mut info) != 0 {
        let r = info.rcMonitor;
        out.push(Rect::new(r.left, r.top, r.right - r.left, r.bottom - r.top));
    }
    1
}

pub struct WindowsInput;

impl WindowsInput {
    pub fn start(events: UnboundedSender<LocalEvent>) -> Self {
        unsafe {
            // Work in physical pixels on every monitor, whatever its scaling.
            SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        }
        let _ = EVENTS.set(events);
        std::thread::Builder::new()
            .name("input-hooks".into())
            .spawn(|| unsafe {
                let module = GetModuleHandleW(std::ptr::null());
                let mouse = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), module, 0);
                let keyboard = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), module, 0);
                if mouse.is_null() || keyboard.is_null() {
                    tracing::error!("could not install input hooks");
                    return;
                }
                let mut msg: MSG = std::mem::zeroed();
                while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            })
            .expect("spawn hook thread");
        WindowsInput
    }

    fn send(inputs: &[INPUT]) {
        unsafe {
            SendInput(inputs.len() as u32, inputs.as_ptr(), std::mem::size_of::<INPUT>() as i32);
        }
    }

    fn mouse_input(dx: i32, dy: i32, data: i32, flags: u32) -> INPUT {
        INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 { mi: MOUSEINPUT { dx, dy, mouseData: data as _, dwFlags: flags, time: 0, dwExtraInfo: OUR_EXTRA_INFO } },
        }
    }
}

impl InputBackend for WindowsInput {
    fn supported(&self) -> bool {
        true
    }

    fn monitors(&self) -> Vec<Rect> {
        let mut out: Vec<Rect> = Vec::new();
        unsafe {
            EnumDisplayMonitors(std::ptr::null_mut(), std::ptr::null(), Some(monitor_proc), &mut out as *mut _ as LPARAM);
        }
        out
    }

    fn cursor_pos(&self) -> Point {
        let mut p = POINT { x: 0, y: 0 };
        unsafe {
            GetCursorPos(&mut p);
        }
        Point::new(p.x, p.y)
    }

    fn set_capture(&self, capture: bool, park: Point) {
        PARK.store(pack(park), Ordering::Relaxed);
        CAPTURE.store(capture, Ordering::Relaxed);
        if capture {
            unsafe {
                SetCursorPos(park.x, park.y);
            }
        }
    }

    fn warp(&self, p: Point) {
        unsafe {
            let vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
            let vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
            let vw = GetSystemMetrics(SM_CXVIRTUALSCREEN).max(2);
            let vh = GetSystemMetrics(SM_CYVIRTUALSCREEN).max(2);
            let nx = (((p.x - vx) as i64 * 65535 + (vw as i64 - 1) / 2) / (vw as i64 - 1)) as i32;
            let ny = (((p.y - vy) as i64 * 65535 + (vh as i64 - 1) / 2) / (vh as i64 - 1)) as i32;
            Self::send(&[Self::mouse_input(nx, ny, 0, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK)]);
            // Absolute coordinates are rounded by Windows; make the position exact so
            // screen-edge detection works to the pixel.
            if self.cursor_pos() != p {
                SetCursorPos(p.x, p.y);
            }
        }
    }

    fn inject(&self, ev: &InputEvent) {
        let input = match *ev {
            InputEvent::MouseMove { dx, dy } => {
                let p = self.cursor_pos();
                self.warp(Point::new(p.x + dx, p.y + dy));
                return;
            }
            InputEvent::MouseButton { button, down } => {
                let (flags, data) = match (button, down) {
                    (MouseButton::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
                    (MouseButton::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
                    (MouseButton::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
                    (MouseButton::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
                    (MouseButton::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
                    (MouseButton::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
                    (MouseButton::X1, true) => (MOUSEEVENTF_XDOWN, XBUTTON1 as i32),
                    (MouseButton::X1, false) => (MOUSEEVENTF_XUP, XBUTTON1 as i32),
                    (MouseButton::X2, true) => (MOUSEEVENTF_XDOWN, XBUTTON2 as i32),
                    (MouseButton::X2, false) => (MOUSEEVENTF_XUP, XBUTTON2 as i32),
                };
                Self::mouse_input(0, 0, data, flags)
            }
            InputEvent::Wheel { dx, dy } => {
                if dy != 0 {
                    Self::send(&[Self::mouse_input(0, 0, dy, MOUSEEVENTF_WHEEL)]);
                }
                if dx != 0 {
                    Self::send(&[Self::mouse_input(0, 0, dx, MOUSEEVENTF_HWHEEL)]);
                }
                return;
            }
            InputEvent::Key { scan, extended, vk, down } => {
                let mut flags = 0;
                if !down {
                    flags |= KEYEVENTF_KEYUP;
                }
                if extended {
                    flags |= KEYEVENTF_EXTENDEDKEY;
                }
                // Scan codes keep the receiving machine's keyboard layout; keys without
                // one (some media keys) go by virtual-key code.
                let (w_vk, w_scan) = if scan != 0 {
                    flags |= KEYEVENTF_SCANCODE;
                    (0, scan)
                } else {
                    (vk, 0)
                };
                INPUT {
                    r#type: INPUT_KEYBOARD,
                    Anonymous: INPUT_0 {
                        ki: KEYBDINPUT { wVk: w_vk, wScan: w_scan, dwFlags: flags, time: 0, dwExtraInfo: OUR_EXTRA_INFO },
                    },
                }
            }
        };
        Self::send(&[input]);
    }
}

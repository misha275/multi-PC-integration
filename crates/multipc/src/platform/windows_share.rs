//! Windows side of window sharing.
//!
//! A window can't physically move to another PC: its program runs here. So the
//! window is captured here (`PrintWindow`, which also works for windows that are
//! covered), shown in a viewer window on the other PC, and keyboard and mouse
//! used in the viewer are sent back and replayed on the original window.
//!
//! Viewers live on one UI thread with its own message loop. Other threads hand
//! it commands through a channel and wake it with a thread message.

use super::windows::OUR_EXTRA_INFO;
use super::{Capture, ViewerEvent, WindowBackend, WindowInfo};
use mpc_core::protocol::{MouseButton, WindowInputEvent};
use mpc_core::window::{decode_frame, fit, to_source, Frame};
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::mpsc;
use std::sync::OnceLock;
use tokio::sync::mpsc::UnboundedSender;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::Storage::Xps::PrintWindow;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

/// Render everything, including DirectX/GPU content (Windows 8.1+).
const PW_RENDERFULLCONTENT: u32 = 2;
const VIEWER_CLASS: &str = "MultiPCViewer";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn hwnd(id: u64) -> HWND {
    id as usize as HWND
}

fn window_title(h: HWND) -> String {
    unsafe {
        let len = GetWindowTextLengthW(h);
        if len <= 0 {
            return String::new();
        }
        let mut buf = vec![0u16; len as usize + 1];
        let n = GetWindowTextW(h, buf.as_mut_ptr(), buf.len() as i32);
        String::from_utf16_lossy(&buf[..n.max(0) as usize])
    }
}

fn class_name(h: HWND) -> String {
    let mut buf = [0u16; 128];
    let n = unsafe { GetClassNameW(h, buf.as_mut_ptr(), buf.len() as i32) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

/// Visible bounds of a window, without the invisible resize borders of Windows 10/11.
fn frame_bounds(h: HWND) -> Option<RECT> {
    unsafe {
        let mut r: RECT = std::mem::zeroed();
        let ok =
            DwmGetWindowAttribute(h, DWMWA_EXTENDED_FRAME_BOUNDS as _, &mut r as *mut _ as *mut c_void, std::mem::size_of::<RECT>() as u32)
                == 0;
        if ok || GetWindowRect(h, &mut r) != 0 {
            Some(r)
        } else {
            None
        }
    }
}

fn is_shareable(h: HWND) -> bool {
    unsafe {
        if IsWindowVisible(h) == 0 || !GetWindow(h, GW_OWNER).is_null() || GetWindowTextLengthW(h) == 0 {
            return false;
        }
        let ex = GetWindowLongW(h, GWL_EXSTYLE) as u32;
        if ex & WS_EX_TOOLWINDOW != 0 {
            return false;
        }
        let mut cloaked: u32 = 0;
        DwmGetWindowAttribute(h, DWMWA_CLOAKED as _, &mut cloaked as *mut _ as *mut c_void, 4);
        if cloaked != 0 {
            return false;
        }
        let class = class_name(h);
        !(class == VIEWER_CLASS || class == "Progman" || class == "Shell_TrayWnd" || class == "WorkerW")
    }
}

unsafe extern "system" fn enum_proc(h: HWND, data: LPARAM) -> windows_sys::core::BOOL {
    let out = &mut *(data as *mut Vec<WindowInfo>);
    if is_shareable(h) {
        if let Some(r) = frame_bounds(h) {
            let (w, hh) = ((r.right - r.left).max(0) as u32, (r.bottom - r.top).max(0) as u32);
            if w >= 50 && hh >= 50 {
                out.push(WindowInfo { id: h as usize as u64, title: window_title(h), width: w, height: hh });
            }
        }
    }
    1
}

fn send_inputs(inputs: &[INPUT]) {
    unsafe {
        SendInput(inputs.len() as u32, inputs.as_ptr(), std::mem::size_of::<INPUT>() as i32);
    }
}

fn mouse(data: i32, flags: u32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 { mi: MOUSEINPUT { dx: 0, dy: 0, mouseData: data as _, dwFlags: flags, time: 0, dwExtraInfo: OUR_EXTRA_INFO } },
    }
}

fn bring_to_front(h: HWND) {
    unsafe {
        if IsIconic(h) != 0 {
            ShowWindow(h, SW_RESTORE);
        }
        if GetForegroundWindow() != h {
            SetForegroundWindow(h);
            BringWindowToTop(h);
        }
    }
}

pub struct WindowsShare {
    ui: mpsc::Sender<Cmd>,
    ui_thread: u32,
}

enum Cmd {
    Open { peer: String, share: u64, title: String, width: u32, height: u32 },
    Frame { peer: String, share: u64, jpeg: Vec<u8> },
    Close { peer: String, share: u64 },
}

impl WindowsShare {
    pub fn start(events: UnboundedSender<ViewerEvent>) -> Self {
        let _ = VIEW_EVENTS.set(events);
        let (tx, rx) = mpsc::channel::<Cmd>();
        let (tid_tx, tid_rx) = mpsc::channel::<u32>();
        std::thread::Builder::new().name("window-viewers".into()).spawn(move || ui_thread(rx, tid_tx)).expect("spawn viewer thread");
        let ui_thread = tid_rx.recv().unwrap_or(0);
        WindowsShare { ui: tx, ui_thread }
    }

    fn command(&self, cmd: Cmd) {
        if self.ui.send(cmd).is_ok() {
            unsafe {
                PostThreadMessageW(self.ui_thread, WM_APP, 0, 0);
            }
        }
    }
}

impl WindowBackend for WindowsShare {
    fn supported(&self) -> bool {
        true
    }

    fn list(&self) -> Vec<WindowInfo> {
        let mut out: Vec<WindowInfo> = Vec::new();
        unsafe {
            EnumWindows(Some(enum_proc), &mut out as *mut _ as LPARAM);
        }
        out
    }

    fn title(&self, id: u64) -> Option<String> {
        unsafe { (IsWindow(hwnd(id)) != 0).then(|| window_title(hwnd(id))) }
    }

    fn dragged(&self) -> Option<u64> {
        unsafe {
            if GetAsyncKeyState(VK_LBUTTON as i32) >= 0 {
                return None;
            }
            let fg = GetForegroundWindow();
            if fg.is_null() {
                return None;
            }
            let thread = GetWindowThreadProcessId(fg, std::ptr::null_mut());
            let mut gti: GUITHREADINFO = std::mem::zeroed();
            gti.cbSize = std::mem::size_of::<GUITHREADINFO>() as u32;
            if GetGUIThreadInfo(thread, &mut gti) == 0 || gti.flags & GUI_INMOVESIZE == 0 {
                return None;
            }
            let moving = if gti.hwndMoveSize.is_null() { fg } else { gti.hwndMoveSize };
            let root = GetAncestor(moving, GA_ROOT);
            let root = if root.is_null() { moving } else { root };
            is_shareable(root).then_some(root as usize as u64)
        }
    }

    fn prepare(&self, id: u64) {
        let h = hwnd(id);
        unsafe {
            if GetAsyncKeyState(VK_LBUTTON as i32) < 0 {
                // End the drag: the rest of the mouse movement belongs to the other PC.
                send_inputs(&[mouse(0, MOUSEEVENTF_LEFTUP)]);
                std::thread::sleep(std::time::Duration::from_millis(60));
            }
            if IsIconic(h) != 0 {
                ShowWindow(h, SW_RESTORE);
            }
            // Pull the window back fully onto its monitor; clicks from the other PC
            // are replayed on the screen here and must land on the window.
            let (Some(r), mon) = (frame_bounds(h), MonitorFromWindow(h, MONITOR_DEFAULTTONEAREST)) else { return };
            let mut info: MONITORINFO = std::mem::zeroed();
            info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
            if GetMonitorInfoW(mon, &mut info) == 0 {
                return;
            }
            let work = info.rcWork;
            let (w, hh) = (r.right - r.left, r.bottom - r.top);
            let x = r.left.min(work.right - w).max(work.left);
            let y = r.top.min(work.bottom - hh).max(work.top);
            if (x, y) != (r.left, r.top) {
                let mut wr: RECT = std::mem::zeroed();
                GetWindowRect(h, &mut wr);
                // SetWindowPos uses the full window rect, including invisible borders.
                let (dx, dy) = (x - r.left, y - r.top);
                SetWindowPos(h, std::ptr::null_mut(), wr.left + dx, wr.top + dy, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
            }
        }
    }

    fn capture(&self, id: u64) -> Capture {
        let h = hwnd(id);
        unsafe {
            if IsWindow(h) == 0 {
                return Capture::Gone;
            }
            if IsIconic(h) != 0 {
                return Capture::Unavailable;
            }
            let (Some(frame), true) = (frame_bounds(h), true) else { return Capture::Unavailable };
            let mut wr: RECT = std::mem::zeroed();
            if GetWindowRect(h, &mut wr) == 0 {
                return Capture::Unavailable;
            }
            let (ww, wh) = (wr.right - wr.left, wr.bottom - wr.top);
            let (fw, fh) = (frame.right - frame.left, frame.bottom - frame.top);
            if ww <= 0 || wh <= 0 || fw <= 0 || fh <= 0 {
                return Capture::Unavailable;
            }
            let screen = GetDC(std::ptr::null_mut());
            let mem = CreateCompatibleDC(screen);
            let bmp = CreateCompatibleBitmap(screen, ww, wh);
            let old = SelectObject(mem, bmp);
            let printed = PrintWindow(h, mem, PW_RENDERFULLCONTENT) != 0;

            let mut bmi: BITMAPINFO = std::mem::zeroed();
            bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            bmi.bmiHeader.biWidth = ww;
            bmi.bmiHeader.biHeight = -wh; // top-down rows
            bmi.bmiHeader.biPlanes = 1;
            bmi.bmiHeader.biBitCount = 32;
            bmi.bmiHeader.biCompression = BI_RGB;
            let mut full = vec![0u8; (ww * wh * 4) as usize];
            SelectObject(mem, old);
            let lines = GetDIBits(mem, bmp, 0, wh as u32, full.as_mut_ptr() as *mut c_void, &mut bmi, DIB_RGB_COLORS);
            DeleteObject(bmp);
            DeleteDC(mem);
            ReleaseDC(std::ptr::null_mut(), screen);
            if !printed || lines == 0 {
                return Capture::Unavailable;
            }

            // Crop the invisible borders around the visible frame.
            let (ox, oy) = ((frame.left - wr.left).clamp(0, ww), (frame.top - wr.top).clamp(0, wh));
            let (cw, ch) = (fw.min(ww - ox), fh.min(wh - oy));
            let mut bgra = Vec::with_capacity((cw * ch * 4) as usize);
            for row in oy..oy + ch {
                let start = ((row * ww + ox) * 4) as usize;
                bgra.extend_from_slice(&full[start..start + (cw * 4) as usize]);
            }
            Capture::Frame(Frame { width: cw as u32, height: ch as u32, bgra })
        }
    }

    fn inject(&self, id: u64, ev: &WindowInputEvent) {
        let h = hwnd(id);
        unsafe {
            if IsWindow(h) == 0 {
                return;
            }
            let Some(r) = frame_bounds(h) else { return };
            let at = |x: i32, y: i32| {
                SetCursorPos(r.left + x, r.top + y);
            };
            match *ev {
                WindowInputEvent::MouseMove { x, y } => at(x, y),
                WindowInputEvent::MouseButton { button, down, x, y } => {
                    if down {
                        bring_to_front(h);
                    }
                    at(x, y);
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
                    send_inputs(&[mouse(data, flags)]);
                }
                WindowInputEvent::Wheel { dx, dy, x, y } => {
                    at(x, y);
                    if dy != 0 {
                        send_inputs(&[mouse(dy, MOUSEEVENTF_WHEEL)]);
                    }
                    if dx != 0 {
                        send_inputs(&[mouse(dx, MOUSEEVENTF_HWHEEL)]);
                    }
                }
                WindowInputEvent::Key { scan, extended, vk, down } => {
                    bring_to_front(h);
                    let mut flags = if down { 0 } else { KEYEVENTF_KEYUP };
                    if extended {
                        flags |= KEYEVENTF_EXTENDEDKEY;
                    }
                    let (w_vk, w_scan) = if scan != 0 {
                        flags |= KEYEVENTF_SCANCODE;
                        (0, scan)
                    } else {
                        (vk, 0)
                    };
                    send_inputs(&[INPUT {
                        r#type: INPUT_KEYBOARD,
                        Anonymous: INPUT_0 {
                            ki: KEYBDINPUT { wVk: w_vk, wScan: w_scan, dwFlags: flags, time: 0, dwExtraInfo: OUR_EXTRA_INFO },
                        },
                    }]);
                }
            }
        }
    }

    fn open_viewer(&self, peer: &str, share: u64, title: &str, width: u32, height: u32) {
        self.command(Cmd::Open { peer: peer.into(), share, title: title.into(), width, height });
    }

    fn show_frame(&self, peer: &str, share: u64, jpeg: Vec<u8>) {
        self.command(Cmd::Frame { peer: peer.into(), share, jpeg });
    }

    fn close_viewer(&self, peer: &str, share: u64) {
        self.command(Cmd::Close { peer: peer.into(), share });
    }
}

// ---- viewer windows (UI thread only) ----------------------------------------

static VIEW_EVENTS: OnceLock<UnboundedSender<ViewerEvent>> = OnceLock::new();

struct Viewer {
    peer: String,
    share: u64,
    src_w: u32,
    src_h: u32,
    frame: Option<Frame>,
}

thread_local! {
    static VIEWERS: RefCell<HashMap<usize, Viewer>> = RefCell::new(HashMap::new());
}

fn find_viewer(peer: &str, share: u64) -> Option<HWND> {
    VIEWERS.with(|v| v.borrow().iter().find(|(_, x)| x.peer == peer && x.share == share).map(|(h, _)| *h as HWND))
}

fn emit(ev: ViewerEvent) {
    if let Some(tx) = VIEW_EVENTS.get() {
        let _ = tx.send(ev);
    }
}

fn ui_thread(rx: mpsc::Receiver<Cmd>, tid: mpsc::Sender<u32>) {
    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        // Create this thread's message queue before anyone posts to it.
        PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_NOREMOVE);
        let _ = tid.send(GetCurrentThreadId());

        let class = wide(VIEWER_CLASS);
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(viewer_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: GetModuleHandleW(std::ptr::null()),
            hIcon: std::ptr::null_mut(),
            hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class.as_ptr(),
        };
        RegisterClassW(&wc);

        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            if msg.hwnd.is_null() && msg.message == WM_APP {
                while let Ok(cmd) = rx.try_recv() {
                    handle_cmd(cmd, &class);
                }
                continue;
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

unsafe fn handle_cmd(cmd: Cmd, class: &[u16]) {
    match cmd {
        Cmd::Open { peer, share, title, width, height } => {
            if find_viewer(&peer, share).is_some() {
                return;
            }
            // Fit the copy on the monitor under the cursor.
            let mut cursor = POINT { x: 0, y: 0 };
            GetCursorPos(&mut cursor);
            let mon = MonitorFromPoint(cursor, MONITOR_DEFAULTTONEAREST);
            let mut info: MONITORINFO = std::mem::zeroed();
            info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
            GetMonitorInfoW(mon, &mut info);
            let work = info.rcWork;
            let max_w = ((work.right - work.left) as f64 * 0.9) as u32;
            let max_h = ((work.bottom - work.top) as f64 * 0.9) as u32;
            let (cw, ch) = fit(width.max(1), height.max(1), max_w.max(200), max_h.max(150));
            let mut r = RECT { left: 0, top: 0, right: cw as i32, bottom: ch as i32 };
            AdjustWindowRectEx(&mut r, WS_OVERLAPPEDWINDOW, 0, 0);
            let (w, h) = (r.right - r.left, r.bottom - r.top);
            let x = (cursor.x - w / 2).clamp(work.left, (work.right - w).max(work.left));
            let y = (cursor.y - h / 2).clamp(work.top, (work.bottom - h).max(work.top));
            let caption = wide(&format!("{title} — {peer}"));
            let hwnd = CreateWindowExW(
                0,
                class.as_ptr(),
                caption.as_ptr(),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                x,
                y,
                w,
                h,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                GetModuleHandleW(std::ptr::null()),
                std::ptr::null(),
            );
            if hwnd.is_null() {
                emit(ViewerEvent::Closed { peer, share });
                return;
            }
            VIEWERS.with(|v| v.borrow_mut().insert(hwnd as usize, Viewer { peer, share, src_w: width, src_h: height, frame: None }));
            SetForegroundWindow(hwnd);
        }
        Cmd::Frame { peer, share, jpeg } => {
            let Some(hwnd) = find_viewer(&peer, share) else { return };
            let Ok(frame) = decode_frame(&jpeg) else { return };
            VIEWERS.with(|v| {
                if let Some(x) = v.borrow_mut().get_mut(&(hwnd as usize)) {
                    x.src_w = frame.width;
                    x.src_h = frame.height;
                    x.frame = Some(frame);
                }
            });
            InvalidateRect(hwnd, std::ptr::null(), 0);
        }
        Cmd::Close { peer, share } => {
            if let Some(hwnd) = find_viewer(&peer, share) {
                VIEWERS.with(|v| v.borrow_mut().remove(&(hwnd as usize)));
                DestroyWindow(hwnd);
            }
        }
    }
}

fn lparam_point(lparam: LPARAM) -> (i32, i32) {
    ((lparam & 0xffff) as u16 as i16 as i32, ((lparam >> 16) & 0xffff) as u16 as i16 as i32)
}

unsafe extern "system" fn viewer_proc(h: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // (peer, share, client size, source size) of this viewer, if it is one of ours.
    let lookup = || {
        VIEWERS.with(|v| {
            v.borrow().get(&(h as usize)).map(|x| {
                let mut r: RECT = std::mem::zeroed();
                GetClientRect(h, &mut r);
                (x.peer.clone(), x.share, (r.right.max(1) as u32, r.bottom.max(1) as u32), (x.src_w, x.src_h))
            })
        })
    };
    let send = |ev: WindowInputEvent| {
        if let Some((peer, share, _, _)) = lookup() {
            emit(ViewerEvent::Input { peer, share, ev });
        }
    };
    let pos = |x: i32, y: i32| -> (i32, i32) {
        match lookup() {
            Some((_, _, (cw, ch), (sw, sh))) => to_source(x, y, cw, ch, sw, sh),
            None => (x, y),
        }
    };
    let button = |b: MouseButton, down: bool| {
        let (x, y) = pos(lparam_point(lparam).0, lparam_point(lparam).1);
        if down {
            SetCapture(h);
        } else {
            ReleaseCapture();
        }
        WindowInputEvent::MouseButton { button: b, down, x, y }
    };
    match msg {
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            let hdc = BeginPaint(h, &mut ps);
            let mut r: RECT = std::mem::zeroed();
            GetClientRect(h, &mut r);
            VIEWERS.with(|v| {
                if let Some(Viewer { frame: Some(f), .. }) = v.borrow().get(&(h as usize)) {
                    let mut bmi: BITMAPINFO = std::mem::zeroed();
                    bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
                    bmi.bmiHeader.biWidth = f.width as i32;
                    bmi.bmiHeader.biHeight = -(f.height as i32);
                    bmi.bmiHeader.biPlanes = 1;
                    bmi.bmiHeader.biBitCount = 32;
                    bmi.bmiHeader.biCompression = BI_RGB;
                    SetStretchBltMode(hdc, HALFTONE);
                    StretchDIBits(
                        hdc,
                        0,
                        0,
                        r.right,
                        r.bottom,
                        0,
                        0,
                        f.width as i32,
                        f.height as i32,
                        f.bgra.as_ptr() as *const c_void,
                        &bmi,
                        DIB_RGB_COLORS,
                        SRCCOPY,
                    );
                } else {
                    FillRect(hdc, &r, GetStockObject(GRAY_BRUSH) as HBRUSH);
                }
            });
            EndPaint(h, &ps);
            0
        }
        WM_ERASEBKGND => 1,
        WM_MOUSEMOVE => {
            let (x, y) = lparam_point(lparam);
            let (x, y) = pos(x, y);
            send(WindowInputEvent::MouseMove { x, y });
            0
        }
        WM_LBUTTONDOWN => {
            send(button(MouseButton::Left, true));
            0
        }
        WM_LBUTTONUP => {
            send(button(MouseButton::Left, false));
            0
        }
        WM_RBUTTONDOWN => {
            send(button(MouseButton::Right, true));
            0
        }
        WM_RBUTTONUP => {
            send(button(MouseButton::Right, false));
            0
        }
        WM_MBUTTONDOWN => {
            send(button(MouseButton::Middle, true));
            0
        }
        WM_MBUTTONUP => {
            send(button(MouseButton::Middle, false));
            0
        }
        WM_XBUTTONDOWN | WM_XBUTTONUP => {
            let b = if (wparam >> 16) as u16 == XBUTTON1 { MouseButton::X1 } else { MouseButton::X2 };
            send(button(b, msg == WM_XBUTTONDOWN));
            1
        }
        WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
            // Wheel messages carry screen coordinates.
            let (sx, sy) = lparam_point(lparam);
            let mut p = POINT { x: sx, y: sy };
            ScreenToClient(h, &mut p);
            let (x, y) = pos(p.x, p.y);
            let delta = (wparam >> 16) as u16 as i16 as i32;
            let (dx, dy) = if msg == WM_MOUSEWHEEL { (0, delta) } else { (delta, 0) };
            send(WindowInputEvent::Wheel { dx, dy, x, y });
            0
        }
        WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP => {
            let down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
            send(WindowInputEvent::Key {
                scan: ((lparam >> 16) & 0xff) as u16,
                extended: (lparam >> 24) & 1 == 1,
                vk: wparam as u16,
                down,
            });
            0
        }
        WM_CLOSE => {
            if let Some((peer, share, _, _)) = lookup() {
                VIEWERS.with(|v| v.borrow_mut().remove(&(h as usize)));
                emit(ViewerEvent::Closed { peer, share });
            }
            DestroyWindow(h);
            0
        }
        WM_DESTROY => {
            VIEWERS.with(|v| v.borrow_mut().remove(&(h as usize)));
            0
        }
        _ => DefWindowProcW(h, msg, wparam, lparam),
    }
}

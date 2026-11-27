//! Windows frame handling for the shell's custom title bar.
//!
//! The window hides the system caption so the shell can paint its own title
//! bar. Windows still reserves the non-client sizing frame, but gpui 0.2.2
//! gives that frame no resize cursor and lets the shell's input handling
//! swallow non-client clicks, so the platform's own frame is not usable as it
//! stands. This module subclasses the window procedure and owns three things
//! for the frame band:
//!
//! - `WM_NCHITTEST` returns the sizing code for every edge and corner,
//!   including the client-side gutter the shell paints around its panels.
//! - `WM_SETCURSOR` and the pointer-move messages set the matching resize
//!   cursor, which gpui would otherwise reset to an arrow.
//! - `WM_NCLBUTTONDOWN` starts the system's sizing loop directly, bypassing
//!   gpui's input dispatch, which may consume the click.
//!
//! The caption itself ([`super::caption`]) uses the same direct approach for the
//! move loop ([`begin_move`]) and for closing ([`request_close`]), so neither
//! depends on gpui's non-client routing.
//!
//! The subclass is installed once, on the window's own thread, and removes
//! itself when the window is destroyed.

use std::ffi::c_void;
use std::mem::zeroed;

use gpui::Window;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

/// Identifies this subclass in comctl32's per-window subclass chain.
const SUBCLASS_ID: usize = 0x474E_4E54;

/// Width of the resize band above the content, in logical pixels. Windows
/// keeps the hidden caption's top frame inside the client area, so this band
/// covers the chrome the shell leaves bare above its title bar controls.
const TOP_BAND: f32 = 4.;

/// Width of the resize band on the left, right, and bottom edges, in logical
/// pixels. It spans the system sizing frame plus the gutter the shell paints
/// around its panels, so the cursor works from the very edge of the window.
const SIDE_BAND: f32 = 16.;

const WM_CLOSE: u32 = 0x0010;
const WM_NCHITTEST: u32 = 0x0084;
const WM_SETCURSOR: u32 = 0x0020;
const WM_NCLBUTTONDOWN: u32 = 0x00A1;
const WM_MOUSEMOVE: u32 = 0x0200;
const WM_NCMOUSEMOVE: u32 = 0x00A0;
const WM_NCDESTROY: u32 = 0x0082;

const GWL_STYLE: i32 = -16;
const WS_THICKFRAME: u32 = 0x0004_0000;

const HTCAPTION: isize = 2;
const HTLEFT: isize = 10;
const HTRIGHT: isize = 11;
const HTTOP: isize = 12;
const HTTOPLEFT: isize = 13;
const HTTOPRIGHT: isize = 14;
const HTBOTTOM: isize = 15;
const HTBOTTOMLEFT: isize = 16;
const HTBOTTOMRIGHT: isize = 17;

const IDC_SIZEWE: usize = 32644;
const IDC_SIZENS: usize = 32645;
const IDC_SIZENWSE: usize = 32642;
const IDC_SIZENESW: usize = 32643;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Point {
    x: i32,
    y: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Rect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

type SubclassProc = unsafe extern "system" fn(
    hwnd: *mut c_void,
    msg: u32,
    wparam: usize,
    lparam: isize,
    id: usize,
    ref_data: usize,
) -> isize;

#[link(name = "comctl32")]
unsafe extern "system" {
    fn SetWindowSubclass(hwnd: *mut c_void, proc: SubclassProc, id: usize, ref_data: usize) -> i32;
    fn RemoveWindowSubclass(hwnd: *mut c_void, proc: SubclassProc, id: usize) -> i32;
    fn DefSubclassProc(hwnd: *mut c_void, msg: u32, wparam: usize, lparam: isize) -> isize;
}

#[link(name = "user32")]
unsafe extern "system" {
    fn DefWindowProcW(hwnd: *mut c_void, msg: u32, wparam: usize, lparam: isize) -> isize;
    fn GetWindowRect(hwnd: *mut c_void, rect: *mut Rect) -> i32;
    fn GetWindowLongW(hwnd: *mut c_void, index: i32) -> i32;
    fn GetDpiForWindow(hwnd: *mut c_void) -> u32;
    fn GetCursorPos(point: *mut Point) -> i32;
    fn IsZoomed(hwnd: *mut c_void) -> i32;
    fn LoadCursorW(instance: *mut c_void, name: *const u16) -> *mut c_void;
    fn ReleaseCapture() -> i32;
    fn SetCursor(cursor: *mut c_void) -> *mut c_void;
    fn PostMessageW(hwnd: *mut c_void, msg: u32, wparam: usize, lparam: isize) -> i32;
}

/// The edge or corner of the window frame under a point.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FrameHit {
    Left,
    Right,
    Top,
    Bottom,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

impl FrameHit {
    fn code(self) -> isize {
        match self {
            Self::Left => HTLEFT,
            Self::Right => HTRIGHT,
            Self::Top => HTTOP,
            Self::Bottom => HTBOTTOM,
            Self::TopLeft => HTTOPLEFT,
            Self::TopRight => HTTOPRIGHT,
            Self::BottomLeft => HTBOTTOMLEFT,
            Self::BottomRight => HTBOTTOMRIGHT,
        }
    }

    fn cursor(self) -> *mut c_void {
        let id = match self {
            Self::Left | Self::Right => IDC_SIZEWE,
            Self::Top | Self::Bottom => IDC_SIZENS,
            Self::TopLeft | Self::BottomRight => IDC_SIZENWSE,
            Self::TopRight | Self::BottomLeft => IDC_SIZENESW,
        };
        // SAFETY: every id above is a predefined cursor resource; the null
        // instance is what the predefined-resource lookup expects.
        unsafe { LoadCursorW(std::ptr::null_mut(), id as *const u16) }
    }
}

/// Classifies a screen-space point against a window rectangle. `side_band` and
/// `top_band` are physical pixels; points outside the rectangle are `None`.
fn frame_hit(x: i32, y: i32, rect: Rect, side_band: i32, top_band: i32) -> Option<FrameHit> {
    if x < rect.left || x >= rect.right || y < rect.top || y >= rect.bottom {
        return None;
    }
    let left = x < rect.left + side_band;
    let right = x >= rect.right - side_band;
    let top = y < rect.top + top_band;
    let bottom = y >= rect.bottom - side_band;
    match (left, right, top, bottom) {
        (true, _, true, _) => Some(FrameHit::TopLeft),
        (_, true, true, _) => Some(FrameHit::TopRight),
        (true, _, _, true) => Some(FrameHit::BottomLeft),
        (_, true, _, true) => Some(FrameHit::BottomRight),
        (true, _, _, _) => Some(FrameHit::Left),
        (_, true, _, _) => Some(FrameHit::Right),
        (_, _, true, _) => Some(FrameHit::Top),
        (_, _, _, true) => Some(FrameHit::Bottom),
        _ => None,
    }
}

fn scale_factor(dpi: u32) -> f32 {
    if dpi == 0 { 1. } else { dpi as f32 / 96. }
}

fn physical_band(logical: f32, dpi: u32) -> i32 {
    (logical * scale_factor(dpi)).round().max(1.) as i32
}

/// The frame band under a screen point, or `None` when the window cannot be
/// resized right now.
unsafe fn hit_at(hwnd: *mut c_void, x: i32, y: i32) -> Option<FrameHit> {
    unsafe {
        let style = GetWindowLongW(hwnd, GWL_STYLE) as u32;
        if style & WS_THICKFRAME == 0 || IsZoomed(hwnd) != 0 {
            return None;
        }
        let mut rect: Rect = zeroed();
        if GetWindowRect(hwnd, &mut rect) == 0 {
            return None;
        }
        let dpi = GetDpiForWindow(hwnd);
        frame_hit(
            x,
            y,
            rect,
            physical_band(SIDE_BAND, dpi),
            physical_band(TOP_BAND, dpi),
        )
    }
}

unsafe fn screen_point_of(lparam: isize) -> (i32, i32) {
    let x = (lparam as u32 & 0xffff) as i16 as i32;
    let y = ((lparam as u32 >> 16) & 0xffff) as i16 as i32;
    (x, y)
}

unsafe fn set_cursor(cursor: *mut c_void) {
    if !cursor.is_null() {
        unsafe {
            SetCursor(cursor);
        }
    }
}

/// The window's native handle, or `None` when it has none (gpui's test window).
fn hwnd_of(window: &Window) -> Option<*mut c_void> {
    // `Window` also has an inherent `window_handle` returning an entity handle,
    // so the raw-handle trait has to be named explicitly.
    let handle = HasWindowHandle::window_handle(window).ok()?;
    let RawWindowHandle::Win32(win32) = handle.as_raw() else {
        return None;
    };
    Some(win32.hwnd.get() as *mut c_void)
}

/// Installs the frame handling on the window's own thread. Does nothing when
/// the window cannot expose an HWND (for example, gpui's test window).
pub(crate) fn install(window: &Window) {
    let Some(hwnd) = hwnd_of(window) else {
        return;
    };
    // SAFETY: the window lives on this thread and outlives the subclass, which
    // removes itself in WM_NCDESTROY.
    let installed = unsafe { SetWindowSubclass(hwnd, subclass_proc, SUBCLASS_ID, 0) };
    if installed == 0 {
        tracing::warn!("failed to install the resize frame subclass");
    }
}

/// Hands the window to the system's move loop, as a press on a native caption
/// does.
///
/// The loop is the one Windows enters for `HTCAPTION`, so Aero snap,
/// double-click maximize, and "dragging a maximized window restores it" keep
/// working without being reimplemented. It runs until the button is released,
/// which is why the caption calls it straight from the press handler.
///
/// `DefWindowProcW` is called directly instead of posting the message: the
/// message would come back through this subclass and gpui's input dispatch, and
/// gpui would treat it as an ordinary press before the loop ever started.
pub(crate) fn begin_move(window: &Window) {
    let Some(hwnd) = hwnd_of(window) else {
        return;
    };
    // SAFETY: called on the window's own thread from its input handler; the
    // loop pumps this thread's message queue until the drag ends.
    unsafe {
        ReleaseCapture();
        DefWindowProcW(hwnd, WM_NCLBUTTONDOWN, HTCAPTION as usize, 0);
    }
}

/// Closes the window the way the system close button does.
///
/// Posting `WM_CLOSE` rather than dropping the window keeps the close path —
/// the shell's `should_close` hook and the state it persists — exactly as it is
/// when the user closes the window from its own button.
pub(crate) fn request_close(window: &Window) {
    let Some(hwnd) = hwnd_of(window) else {
        return;
    };
    // SAFETY: posting to a live window owned by this thread; the message is
    // processed by the message loop that is already running.
    unsafe {
        PostMessageW(hwnd, WM_CLOSE, 0, 0);
    }
}

unsafe extern "system" fn subclass_proc(
    hwnd: *mut c_void,
    msg: u32,
    wparam: usize,
    lparam: isize,
    _id: usize,
    _ref_data: usize,
) -> isize {
    // SAFETY: this procedure only forwards to Win32 and never unwinds.
    unsafe {
        match msg {
            WM_NCHITTEST => {
                let (x, y) = screen_point_of(lparam);
                if let Some(hit) = hit_at(hwnd, x, y) {
                    return hit.code();
                }
                DefSubclassProc(hwnd, msg, wparam, lparam)
            }
            WM_SETCURSOR => {
                let hit = (lparam as u32 & 0xffff) as isize;
                if let Some(frame) = frame_of(hit) {
                    set_cursor(frame.cursor());
                    return 1;
                }
                DefSubclassProc(hwnd, msg, wparam, lparam)
            }
            // A press on one of our edges must enter the system sizing loop
            // even though gpui would dispatch it as an ordinary input event.
            WM_NCLBUTTONDOWN if is_frame_code(wparam as isize) => {
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            // gpui resets the cursor to an arrow after most pointer dispatches,
            // so re-assert the frame cursor after it has seen the move.
            WM_MOUSEMOVE | WM_NCMOUSEMOVE => {
                let result = DefSubclassProc(hwnd, msg, wparam, lparam);
                let mut point: Point = zeroed();
                if GetCursorPos(&mut point) != 0
                    && let Some(hit) = hit_at(hwnd, point.x, point.y)
                {
                    set_cursor(hit.cursor());
                }
                result
            }
            WM_NCDESTROY => {
                RemoveWindowSubclass(hwnd, subclass_proc, SUBCLASS_ID);
                DefSubclassProc(hwnd, msg, wparam, lparam)
            }
            _ => DefSubclassProc(hwnd, msg, wparam, lparam),
        }
    }
}

fn frame_of(code: isize) -> Option<FrameHit> {
    match code {
        HTLEFT => Some(FrameHit::Left),
        HTRIGHT => Some(FrameHit::Right),
        HTTOP => Some(FrameHit::Top),
        HTBOTTOM => Some(FrameHit::Bottom),
        HTTOPLEFT => Some(FrameHit::TopLeft),
        HTTOPRIGHT => Some(FrameHit::TopRight),
        HTBOTTOMLEFT => Some(FrameHit::BottomLeft),
        HTBOTTOMRIGHT => Some(FrameHit::BottomRight),
        _ => None,
    }
}

fn is_frame_code(code: isize) -> bool {
    frame_of(code).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    const RECT: Rect = Rect {
        left: 100,
        top: 200,
        right: 1100,
        bottom: 1000,
    };

    fn hit(x: i32, y: i32) -> Option<FrameHit> {
        frame_hit(x, y, RECT, 16, 4)
    }

    #[test]
    fn edges_report_their_side() {
        assert_eq!(hit(101, 600), Some(FrameHit::Left));
        assert_eq!(hit(1090, 600), Some(FrameHit::Right));
        assert_eq!(hit(600, 201), Some(FrameHit::Top));
        assert_eq!(hit(600, 990), Some(FrameHit::Bottom));
    }

    #[test]
    fn the_band_starts_at_the_window_edge() {
        assert_eq!(hit(100, 600), Some(FrameHit::Left));
        assert_eq!(hit(115, 600), Some(FrameHit::Left));
        assert_eq!(hit(116, 600), None);
        assert_eq!(hit(1083, 600), None);
        assert_eq!(hit(1084, 600), Some(FrameHit::Right));
    }

    #[test]
    fn corners_win_over_edges() {
        assert_eq!(hit(103, 202), Some(FrameHit::TopLeft));
        assert_eq!(hit(1097, 202), Some(FrameHit::TopRight));
        assert_eq!(hit(103, 998), Some(FrameHit::BottomLeft));
        assert_eq!(hit(1097, 998), Some(FrameHit::BottomRight));
    }

    #[test]
    fn the_top_band_stays_narrow() {
        assert_eq!(hit(600, 200), Some(FrameHit::Top));
        assert_eq!(hit(600, 203), Some(FrameHit::Top));
        assert_eq!(hit(600, 204), None);
    }

    #[test]
    fn points_outside_the_window_are_ignored() {
        assert_eq!(hit(99, 600), None);
        assert_eq!(hit(1100, 600), None);
        assert_eq!(hit(600, 199), None);
        assert_eq!(hit(600, 1000), None);
    }

    #[test]
    fn every_frame_hit_has_a_distinct_size_cursor() {
        for frame in [
            FrameHit::Left,
            FrameHit::Right,
            FrameHit::Top,
            FrameHit::Bottom,
            FrameHit::TopLeft,
            FrameHit::TopRight,
            FrameHit::BottomLeft,
            FrameHit::BottomRight,
        ] {
            assert_eq!(frame_of(frame.code()), Some(frame));
            assert!(!frame.cursor().is_null());
        }
    }
}

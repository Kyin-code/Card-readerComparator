#![cfg(windows)]

use std::{
    ffi::c_void,
    mem::zeroed,
    ptr::{null, null_mut},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicIsize, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM},
    Graphics::Gdi::{
        BITMAPINFO, BITMAPINFOHEADER, BeginPaint, DIB_RGB_COLORS, EndPaint, PAINTSTRUCT, SRCCOPY,
        StretchDIBits,
    },
    System::LibraryLoader::GetModuleHandleW,
    UI::{
        Input::KeyboardAndMouse::VK_ESCAPE,
        WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect,
            GetMessageW, GetWindowLongPtrW, LoadCursorW, MSG, PostMessageW, PostQuitMessage,
            RegisterClassW, SW_SHOW, SetForegroundWindow, SetWindowLongPtrW, ShowCursor,
            ShowWindow, TranslateMessage, WM_CLOSE, WM_DESTROY, WM_KEYDOWN, WM_PAINT, WNDCLASSW,
            WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
        },
    },
};

use crate::{model::ScreenInfo, pattern::PatternFrame};

const CLASS_NAME: &str = "CardReaderComparatorCalibrationOverlay";
const WINDOW_TITLE: &str = "Card Reader Comparator · Calibration";
static OVERLAY_SEQUENCE: AtomicU64 = AtomicU64::new(1);

pub struct OverlaySession {
    hwnd: Arc<AtomicIsize>,
    closed: Arc<AtomicBool>,
}

impl OverlaySession {
    pub fn open(screen: &ScreenInfo, pattern: &PatternFrame) -> Result<Self, String> {
        if pattern.size != screen.size {
            return Err(format!(
                "标定图尺寸 {}×{} 与屏幕 {}×{} 不一致",
                pattern.size.width, pattern.size.height, screen.size.width, screen.size.height
            ));
        }

        let hwnd = Arc::new(AtomicIsize::new(0));
        let closed = Arc::new(AtomicBool::new(false));
        let thread_hwnd = hwnd.clone();
        let thread_closed = closed.clone();
        let screen = screen.clone();
        let class_name = wide_null(&format!(
            "{CLASS_NAME}-{}",
            OVERLAY_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let context = Box::new(OverlayContext {
            pixels: pattern.bgra_top_down(),
            width: pattern.size.width,
            height: pattern.size.height,
        });
        let (ready_tx, ready_rx) = mpsc::channel();

        thread::Builder::new()
            .name("calibration-overlay".into())
            .spawn(move || {
                let context_ptr = Box::into_raw(context);
                let result = run_overlay(
                    &screen,
                    context_ptr,
                    thread_hwnd.as_ref(),
                    thread_closed.as_ref(),
                    ready_tx,
                    class_name,
                );
                let _ = result;
                // context_ptr is reclaimed after the message loop in run_overlay.
            })
            .map_err(|error| format!("创建全屏标定线程失败: {error}"))?;

        match ready_rx.recv_timeout(Duration::from_secs(4)) {
            Ok(Ok(())) => Ok(Self { hwnd, closed }),
            Ok(Err(error)) => Err(error),
            Err(_) => {
                let handle = hwnd.load(Ordering::Acquire);
                if handle != 0 {
                    unsafe {
                        PostMessageW(handle as HWND, WM_CLOSE, 0, 0);
                    }
                }
                Err("全屏标定窗口启动超时".into())
            }
        }
    }
}

impl Drop for OverlaySession {
    fn drop(&mut self) {
        let handle = self.hwnd.swap(0, Ordering::AcqRel);
        if handle != 0 && !self.closed.load(Ordering::Acquire) {
            unsafe {
                PostMessageW(handle as HWND, WM_CLOSE, 0, 0);
            }
        }
    }
}

struct OverlayContext {
    pixels: Vec<u8>,
    width: u32,
    height: u32,
}

fn run_overlay(
    screen: &ScreenInfo,
    context_ptr: *mut OverlayContext,
    hwnd_slot: &AtomicIsize,
    closed: &AtomicBool,
    ready_tx: mpsc::Sender<Result<(), String>>,
    class_name: Vec<u16>,
) -> Result<(), String> {
    let window_title = wide_null(WINDOW_TITLE);
    unsafe {
        let instance = GetModuleHandleW(null());
        if instance.is_null() {
            mark_closed(closed);
            drop(Box::from_raw(context_ptr));
            let error = "GetModuleHandleW failed".to_string();
            let _ = ready_tx.send(Err(error.clone()));
            return Err(error);
        }
        let cursor = LoadCursorW(null_mut(), 32512_u16 as *const u16);
        let window_class = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(window_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance,
            hIcon: null_mut(),
            hCursor: cursor,
            hbrBackground: null_mut(),
            lpszMenuName: null(),
            lpszClassName: class_name.as_ptr(),
        };
        if RegisterClassW(&window_class) == 0 {
            mark_closed(closed);
            drop(Box::from_raw(context_ptr));
            let error = "RegisterClassW failed".to_string();
            let _ = ready_tx.send(Err(error.clone()));
            return Err(error);
        }

        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            class_name.as_ptr(),
            window_title.as_ptr(),
            WS_POPUP | WS_VISIBLE,
            screen.x,
            screen.y,
            screen.size.width as i32,
            screen.size.height as i32,
            null_mut(),
            null_mut(),
            instance,
            context_ptr.cast::<c_void>(),
        );
        if hwnd.is_null() {
            mark_closed(closed);
            drop(Box::from_raw(context_ptr));
            let error = "CreateWindowExW failed".to_string();
            let _ = ready_tx.send(Err(error.clone()));
            return Err(error);
        }

        hwnd_slot.store(hwnd as isize, Ordering::Release);
        SetWindowLongPtrW(hwnd, -21, context_ptr as isize);
        ShowWindow(hwnd, SW_SHOW);
        SetForegroundWindow(hwnd);
        ShowCursor(0);
        let _ = ready_tx.send(Ok(()));

        let mut message: MSG = zeroed();
        while GetMessageW(&mut message, null_mut(), 0, 0) > 0 {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }

        ShowCursor(1);
        hwnd_slot.store(0, Ordering::Release);
        mark_closed(closed);
        drop(Box::from_raw(context_ptr));
        Ok(())
    }
}

fn mark_closed(closed: &AtomicBool) {
    closed.store(true, Ordering::Release);
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_PAINT => {
            let context_ptr = unsafe { GetWindowLongPtrW(hwnd, -21) as *const OverlayContext };
            let mut paint: PAINTSTRUCT = unsafe { zeroed() };
            let hdc = unsafe { BeginPaint(hwnd, &mut paint) };
            if !context_ptr.is_null() {
                let context = unsafe { &*context_ptr };
                let mut client: RECT = unsafe { zeroed() };
                unsafe {
                    GetClientRect(hwnd, &mut client);
                }
                let bitmap_info = BITMAPINFO {
                    bmiHeader: BITMAPINFOHEADER {
                        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                        biWidth: context.width as i32,
                        biHeight: -(context.height as i32),
                        biPlanes: 1,
                        biBitCount: 32,
                        biCompression: 0,
                        biSizeImage: context.pixels.len() as u32,
                        biXPelsPerMeter: 0,
                        biYPelsPerMeter: 0,
                        biClrUsed: 0,
                        biClrImportant: 0,
                    },
                    bmiColors: [unsafe { zeroed() }],
                };
                unsafe {
                    StretchDIBits(
                        hdc,
                        0,
                        0,
                        client.right - client.left,
                        client.bottom - client.top,
                        0,
                        0,
                        context.width as i32,
                        context.height as i32,
                        context.pixels.as_ptr().cast(),
                        &bitmap_info,
                        DIB_RGB_COLORS,
                        SRCCOPY,
                    );
                    EndPaint(hwnd, &paint);
                }
            }
            0
        }
        WM_KEYDOWN if wparam as u16 == VK_ESCAPE => {
            unsafe {
                DestroyWindow(hwnd);
            }
            0
        }
        WM_CLOSE => {
            unsafe {
                DestroyWindow(hwnd);
            }
            0
        }
        WM_DESTROY => {
            unsafe {
                PostQuitMessage(0);
            }
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

fn wide_null(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

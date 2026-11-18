//! Windows logon launch, notification-area menu and single-instance activation.

mod badge;

use devhub_core::{DeviceInfo, DeviceKind};
use device_hub::app::{Route, Shell};
use gpui_kit::Window;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::cell::{Cell, RefCell};
use std::mem::size_of;
use std::ptr::{null, null_mut};
use std::time::{Duration, Instant};
use windows_sys::core::w;
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, ERROR_CLASS_ALREADY_EXISTS, HANDLE, HWND,
    LPARAM, LRESULT, POINT, WPARAM,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NOTIFYICONDATAW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const CLASS: *const u16 = w!("DeviceHub.NotificationArea");
const INSTANCE: *const u16 = w!("Local\\DeviceHub.NotificationArea");
const TRAY_ID: u32 = 1;
const TRAY_EVENT: u32 = WM_APP + 1;
const SHOW_WINDOW: u32 = WM_APP + 2;
const RETRY_ICON: usize = 1;
const OPEN: i32 = 1;
const QUIT: i32 = 2;
thread_local! {
    static QUITTING: Cell<bool> = const { Cell::new(false) };
    static TASKBAR_CREATED: Cell<u32> = const { Cell::new(0) };
    static TRAY_WINDOW: Cell<HWND> = const { Cell::new(null_mut()) };
    static BATTERY: RefCell<Option<Badge>> = const { RefCell::new(None) };
}

#[derive(Clone, PartialEq, Eq)]
struct Badge {
    name: String,
    percent: u8,
    charging: bool,
    asleep: bool,
}

pub fn update_battery(shell: &Shell) {
    let window = TRAY_WINDOW.get();
    if window.is_null() {
        return;
    }
    let next = battery_mouse(shell).and_then(|device| {
        let battery = device.battery?;
        Some(Badge {
            name: device.name.clone(),
            percent: battery.percent?.min(100),
            charging: battery.charging,
            asleep: device.asleep,
        })
    });
    let has_changed = BATTERY.with(|current| {
        let mut current = current.borrow_mut();
        if *current == next {
            false
        } else {
            *current = next;
            true
        }
    });
    if has_changed {
        // SAFETY: GPUI calls this observer on the thread that owns the tray.
        unsafe { publish_icon(window, true) };
    }
}

fn battery_mouse(shell: &Shell) -> Option<&DeviceInfo> {
    if shell.hub.demo {
        return None;
    }
    let selected = if let Route::Device(id) = &shell.route {
        Some(id)
    } else {
        None
    };
    shell
        .hub
        .devices
        .iter()
        .filter(|device| {
            device.kind == DeviceKind::Mouse && device.battery.is_some_and(|b| b.percent.is_some())
        })
        .min_by_key(|device| (device.asleep, selected != Some(&device.id), &device.id.0))
}

pub struct Instance(HANDLE);

impl Instance {
    /// A second normal launch opens the first window. A logon launch stays quiet.
    pub fn acquire(start_hidden: bool) -> Option<Self> {
        // SAFETY: Named kernel object with the process's default ACL and no initial ownership.
        let handle = unsafe { CreateMutexW(null(), 0, INSTANCE) };
        if handle.is_null() {
            return Some(Self(handle));
        }
        let instance = Self(handle);
        // SAFETY: Reads the result of CreateMutexW for this thread.
        if unsafe { GetLastError() } != ERROR_ALREADY_EXISTS {
            return Some(instance);
        }
        if !start_hidden {
            let deadline = Instant::now() + Duration::from_secs(4);
            loop {
                // SAFETY: The class name is null-terminated and the message carries no pointers.
                let hwnd = unsafe { FindWindowW(CLASS, null()) };
                if !hwnd.is_null() {
                    unsafe { PostMessageW(hwnd, SHOW_WINDOW, 0, 0) };
                    break;
                }
                if Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        None
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: This instance owns the HANDLE from CreateMutexW.
            unsafe { CloseHandle(self.0) };
        }
    }
}

pub struct Tray {
    tray: HWND,
    main: HWND,
}

impl Tray {
    pub fn install(window: &Window) -> Option<Self> {
        let RawWindowHandle::Win32(handle) = HasWindowHandle::window_handle(window).ok()?.as_raw()
        else {
            return None;
        };
        let main = handle.hwnd.get() as HWND;
        // SAFETY: This runs on the window's UI thread. The Win32 class and callback
        // stay valid for the life of the process; the window keeps only an HWND.
        unsafe {
            let module = GetModuleHandleW(null());
            let class = WNDCLASSW {
                hInstance: module,
                lpszClassName: CLASS,
                lpfnWndProc: Some(tray_proc),
                ..Default::default()
            };
            if RegisterClassW(&class) == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
                show(main);
                return None;
            }
            let tray = CreateWindowExW(
                0,
                CLASS,
                w!("Device HUB"),
                WS_POPUP,
                0,
                0,
                0,
                0,
                null_mut(),
                null_mut(),
                module,
                null(),
            );
            if tray.is_null() {
                show(main);
                return None;
            }
            SetWindowLongPtrW(tray, GWLP_USERDATA, main as isize);
            TASKBAR_CREATED.set(RegisterWindowMessageW(w!("TaskbarCreated")));
            TRAY_WINDOW.set(tray);
            publish_icon(tray, false);
            Some(Self { tray, main })
        }
    }

    pub fn close_or_hide(&self) -> bool {
        if QUITTING.get() {
            return true;
        }
        // SAFETY: This callback is attached to the live GPUI window.
        unsafe { ShowWindow(self.main, SW_HIDE) };
        false
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        TRAY_WINDOW.set(null_mut());
        // SAFETY: The tray window belongs to this thread and owns the Shell icon.
        unsafe { DestroyWindow(self.tray) };
    }
}

fn show(hwnd: HWND) {
    // SAFETY: The window handle comes from GPUI. These calls tolerate a stale HWND.
    unsafe {
        ShowWindow(
            hwnd,
            if IsIconic(hwnd) == 0 {
                SW_SHOW
            } else {
                SW_RESTORE
            },
        );
        SetForegroundWindow(hwnd);
    }
}

unsafe fn publish_icon(tray: HWND, update: bool) {
    let mut data = icon_data(tray);
    data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    data.uCallbackMessage = TRAY_EVENT;
    let battery = BATTERY.with(|status| status.borrow().clone());
    let badge = battery.as_ref().map_or(null_mut(), |status| {
        badge::icon(status.percent, status.charging, status.asleep)
    });
    data.hIcon = badge;
    if data.hIcon.is_null() {
        data.hIcon = LoadIconW(GetModuleHandleW(null()), 1usize as *const u16);
    }
    if data.hIcon.is_null() {
        data.hIcon = LoadIconW(null_mut(), IDI_APPLICATION);
    }
    let tip = battery.map_or_else(
        || "Device HUB".into(),
        |status| {
            let suffix = if status.asleep {
                "（前回の残量）"
            } else if status.charging {
                "（充電中）"
            } else {
                ""
            };
            format!("Device HUB · {} · {}%{suffix}", status.name, status.percent)
        },
    );
    let tip_len = data.szTip.len() - 1;
    for (dst, src) in data.szTip[..tip_len].iter_mut().zip(tip.encode_utf16()) {
        *dst = src;
    }
    let published = (update && Shell_NotifyIconW(NIM_MODIFY, &data) != 0)
        || Shell_NotifyIconW(NIM_ADD, &data) != 0;
    if !badge.is_null() {
        DestroyIcon(badge);
    }
    if !published {
        // Explorer may not have started yet during logon.
        SetTimer(tray, RETRY_ICON, 2500, None);
    } else {
        KillTimer(tray, RETRY_ICON);
    }
}

fn icon_data(tray: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: tray,
        uID: TRAY_ID,
        ..Default::default()
    }
}

unsafe extern "system" fn tray_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let main = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as HWND;
    match msg {
        SHOW_WINDOW => show(main),
        TRAY_EVENT => match lparam as u32 {
            WM_LBUTTONUP => show(main),
            WM_RBUTTONUP => tray_menu(hwnd, main),
            _ => {}
        },
        WM_TIMER if wparam == RETRY_ICON => publish_icon(hwnd, false),
        WM_DESTROY => {
            KillTimer(hwnd, RETRY_ICON);
            Shell_NotifyIconW(NIM_DELETE, &icon_data(hwnd));
        }
        _ if msg != 0 && msg == TASKBAR_CREATED.get() => publish_icon(hwnd, false),
        _ => return DefWindowProcW(hwnd, msg, wparam, lparam),
    }
    0
}

unsafe fn tray_menu(tray: HWND, main: HWND) {
    let menu = CreatePopupMenu();
    if menu.is_null() {
        return;
    }
    AppendMenuW(menu, MF_STRING, OPEN as usize, w!("Device HUB を開く"));
    AppendMenuW(menu, MF_STRING, QUIT as usize, w!("終了"));
    let mut point = POINT::default();
    GetCursorPos(&mut point);
    SetForegroundWindow(tray);
    let command = TrackPopupMenu(
        menu,
        TPM_RETURNCMD | TPM_RIGHTBUTTON,
        point.x,
        point.y,
        0,
        tray,
        null(),
    );
    DestroyMenu(menu);
    PostMessageW(tray, WM_NULL, 0, 0);
    match command {
        OPEN => show(main),
        QUIT => {
            QUITTING.set(true);
            PostMessageW(main, WM_CLOSE, 0, 0);
        }
        _ => {}
    }
}

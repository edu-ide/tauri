use cef::*;
use std::sync::LazyLock;

use crate::cef_webview::CefBrowserExt;
use windows::{
  core::{HRESULT, HSTRING, PCSTR},
  Win32::{
    Foundation::*,
    Graphics::Gdi::*,
    System::LibraryLoader::*,
    UI::{HiDpi::*, WindowsAndMessaging::*},
  },
};

impl CefBrowserExt for cef::Browser {
  fn hwnd(&self) -> Option<HWND> {
    let host = self.host()?;
    let hwnd = host.window_handle();
    Some(HWND(hwnd.0 as _))
  }

  fn bounds(&self) -> cef::Rect {
    let Some(hwnd) = self.hwnd() else {
      return cef::Rect::default();
    };

    let mut rect = RECT::default();
    let _ = unsafe { GetClientRect(hwnd, &mut rect) };

    let position_point = &mut [POINT {
      x: rect.left,
      y: rect.top,
    }];
    unsafe { MapWindowPoints(Some(hwnd), GetParent(hwnd).ok(), position_point) };

    // Callers work in DIP like the CEF Views and macOS paths; Win32 reports physical pixels.
    let physical = cef::Rect {
      x: position_point[0].x,
      y: position_point[0].y,
      width: (rect.right - rect.left) as i32,
      height: (rect.bottom - rect.top) as i32,
    };
    scale_rect(&physical, 1.0 / self.scale_factor())
  }

  fn set_bounds(&self, rect: Option<&cef::Rect>) {
    let Some(rect) = rect else {
      return;
    };

    let Some(hwnd) = self.hwnd() else {
      return;
    };

    // `rect` is in DIP; SetWindowPos takes physical pixels. Without this a child browser at
    // 200% display scaling fills only the top-left quarter of its window.
    let rect = scale_rect(rect, self.scale_factor());
    let _ = unsafe {
      SetWindowPos(
        hwnd,
        None,
        rect.x,
        rect.y,
        rect.width,
        rect.height,
        SWP_ASYNCWINDOWPOS | SWP_NOACTIVATE | SWP_NOZORDER,
      )
    };
  }

  fn scale_factor(&self) -> f64 {
    let Some(hwnd) = self.hwnd() else {
      return 1.0;
    };

    let dpi = unsafe { hwnd_dpi(hwnd) };
    dpi_to_scale_factor(dpi)
  }

  fn set_background_color(&self, color: cef::Color) {
    // TODO:
  }

  fn set_visible(&self, visible: i32) {
    let Some(hwnd) = self.hwnd() else {
      return;
    };

    let cmd = if visible != 0 { SW_SHOW } else { SW_HIDE };
    let _ = unsafe { ShowWindow(hwnd, cmd) };
  }

  fn close(&self) {
    let Some(hwnd) = self.hwnd() else {
      return;
    };

    let _ = unsafe { DestroyWindow(hwnd) };
  }

  fn set_parent(&self, parent: &cef::Window) {
    let Some(hwnd) = self.hwnd() else {
      return;
    };

    let parent_hwnd = HWND(parent.window_handle().0 as _);
    let _ = unsafe { SetParent(hwnd, Some(parent_hwnd)) };
  }
}

fn get_function_impl(library: &str, function: &str) -> FARPROC {
  let library = HSTRING::from(library);
  assert_eq!(function.chars().last(), Some('\0'));

  // Library names we will use are ASCII so we can use the A version to avoid string conversion.
  let module = unsafe { LoadLibraryW(&library) }.unwrap_or_default();
  if module.is_invalid() {
    return None;
  }

  unsafe { GetProcAddress(module, PCSTR::from_raw(function.as_ptr())) }
}

macro_rules! get_function {
  ($lib:expr, $func:ident) => {
    get_function_impl($lib, concat!(stringify!($func), '\0'))
      .map(|f| unsafe { std::mem::transmute::<_, $func>(f) })
  };
}

pub type GetDpiForWindow = unsafe extern "system" fn(hwnd: HWND) -> u32;
pub type GetDpiForMonitor = unsafe extern "system" fn(
  hmonitor: HMONITOR,
  dpi_type: MONITOR_DPI_TYPE,
  dpi_x: *mut u32,
  dpi_y: *mut u32,
) -> HRESULT;

static GET_DPI_FOR_WINDOW: LazyLock<Option<GetDpiForWindow>> =
  LazyLock::new(|| get_function!("user32.dll", GetDpiForWindow));
static GET_DPI_FOR_MONITOR: LazyLock<Option<GetDpiForMonitor>> =
  LazyLock::new(|| get_function!("shcore.dll", GetDpiForMonitor));

/// Converts a rect between DIP and physical pixels, rounding each edge so adjacent views
/// that share an edge in one space still share it in the other.
fn scale_rect(rect: &cef::Rect, factor: f64) -> cef::Rect {
  if !factor.is_finite() || factor <= 0.0 || factor == 1.0 {
    return rect.clone();
  }
  let left = (rect.x as f64 * factor).round() as i32;
  let top = (rect.y as f64 * factor).round() as i32;
  let right = ((rect.x + rect.width) as f64 * factor).round() as i32;
  let bottom = ((rect.y + rect.height) as f64 * factor).round() as i32;
  cef::Rect {
    x: left,
    y: top,
    width: right - left,
    height: bottom - top,
  }
}

pub const BASE_DPI: u32 = 96;
pub fn dpi_to_scale_factor(dpi: u32) -> f64 {
  dpi as f64 / BASE_DPI as f64
}

#[allow(non_snake_case)]
pub unsafe fn hwnd_dpi(hwnd: HWND) -> u32 {
  if let Some(GetDpiForWindow) = *GET_DPI_FOR_WINDOW {
    // We are on Windows 10 Anniversary Update (1607) or later.
    match GetDpiForWindow(hwnd) {
      0 => BASE_DPI, // 0 is returned if hwnd is invalid
      #[allow(clippy::unnecessary_cast)]
      dpi => dpi as u32,
    }
  } else if let Some(GetDpiForMonitor) = *GET_DPI_FOR_MONITOR {
    // We are on Windows 8.1 or later.
    let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
    if monitor.is_invalid() {
      return BASE_DPI;
    }

    let mut dpi_x = 0;
    let mut dpi_y = 0;
    #[allow(clippy::unnecessary_cast)]
    if GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) == S_OK {
      dpi_x as u32
    } else {
      BASE_DPI
    }
  } else {
    let hdc = GetDC(Some(hwnd));
    if hdc.is_invalid() {
      return BASE_DPI;
    }

    // We are on Vista or later.
    if IsProcessDPIAware().as_bool() {
      // If the process is DPI aware, then scaling must be handled by the application using
      // this DPI value.
      GetDeviceCaps(Some(hdc), LOGPIXELSX) as u32
    } else {
      // If the process is DPI unaware, then scaling is performed by the OS; we thus return
      // 96 (scale factor 1.0) to prevent the window from being re-scaled by both the
      // application and the WM.
      BASE_DPI
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn child_bounds_convert_between_dip_and_physical_pixels() {
    let dip = cef::Rect { x: 264, y: 55, width: 880, height: 661 };
    let physical = scale_rect(&dip, 2.0);
    assert_eq!((physical.x, physical.y, physical.width, physical.height), (528, 110, 1760, 1322));
    let back = scale_rect(&physical, 0.5);
    assert_eq!((back.x, back.y, back.width, back.height), (264, 55, 880, 661));
    // 150%: edges round independently so neighbours stay flush.
    let left = scale_rect(&cef::Rect { x: 0, y: 0, width: 101, height: 10 }, 1.5);
    let right = scale_rect(&cef::Rect { x: 101, y: 0, width: 99, height: 10 }, 1.5);
    assert_eq!(left.x + left.width, right.x);
    assert_eq!(scale_rect(&dip, 1.0).width, 880);
    assert_eq!(scale_rect(&dip, f64::NAN).width, 880);
  }
}

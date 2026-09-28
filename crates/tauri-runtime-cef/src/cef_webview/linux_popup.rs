//! Keep a window that a page opened with `window.open` above the app window it came from.
//!
//! CEF gives such a popup (NICE 본인인증 at 420x690, 홈택스's certificate viewer) its own
//! top-level X window with no name and no WM_TRANSIENT_FOR. The first click on the app window
//! let the window manager raise the app window over it, so the page's button looked dead
//! (2026-09-28, 도공기술마켓 회원가입). Mark the popup transient for the app window, name it
//! after its page, and bring it forward once the window manager shows it. The opener link
//! (`window.opener`) is Chromium's and is not touched here.

use cef::{ImplBrowser, ImplBrowserHost};
use std::ffi::CString;
use x11_dl::xlib;

use super::linux::X11;

/// A popup browser was created. `owner` is the client window of the app window whose page
/// opened it, when the app has one.
pub(crate) fn adopt(popup: &cef::Browser, owner: Option<xlib::Window>) {
  let Some(handle) = native_handle(popup) else {
    return;
  };
  let window = with_display(|xlib, display| unsafe {
    let window = client_toplevel(xlib, display, handle);
    let owner = owner.filter(|owner| *owner > 1 && *owner != window);
    if let Some(owner) = owner {
      (xlib.XSetTransientForHint)(display, window, owner);
    }
    let owner_name = owner.and_then(|owner| window_name(xlib, display, owner));
    set_name(xlib, display, window, &popup_title("", owner_name.as_deref()));
    window
  });
  if let Some(window) = window {
    super::linux_stacking::present_when_shown(window);
  }
}

/// The popup's page changed its title.
pub(crate) fn retitle(popup: &cef::Browser, page_title: &str) {
  let Some(handle) = native_handle(popup) else {
    return;
  };
  with_display(|xlib, display| unsafe {
    let window = client_toplevel(xlib, display, handle);
    let owner_name = transient_for(xlib, display, window).and_then(|owner| window_name(xlib, display, owner));
    set_name(xlib, display, window, &popup_title(page_title, owner_name.as_deref()));
  });
}

/// "<page> — <app window>", so the popup can be told apart in the window list.
fn popup_title(page: &str, owner: Option<&str>) -> String {
  let page = page.trim();
  let owner = owner.map(str::trim).filter(|owner| !owner.is_empty());
  match (page.is_empty(), owner) {
    (false, Some(owner)) => format!("{page} — {owner}"),
    (false, None) => page.to_string(),
    (true, Some(owner)) => owner.to_string(),
    (true, None) => "Popup".to_string(),
  }
}

fn native_handle(browser: &cef::Browser) -> Option<xlib::Window> {
  let handle = browser.host()?.window_handle() as xlib::Window;
  (handle > 1).then_some(handle)
}

fn with_display<R>(run: impl FnOnce(&xlib::Xlib, *mut xlib::Display) -> R) -> Option<R> {
  // X11 installs an error handler that ignores BadWindow from windows that are already gone.
  let xlib = X11.as_ref()?;
  unsafe {
    let display = (xlib.XOpenDisplay)(std::ptr::null());
    if display.is_null() {
      return None;
    }
    let result = run(xlib, display);
    (xlib.XFlush)(display);
    (xlib.XCloseDisplay)(display);
    Some(result)
  }
}

unsafe fn atom(xlib: &xlib::Xlib, display: *mut xlib::Display, name: &str) -> xlib::Atom {
  let name = CString::new(name).expect("atom names have no NUL");
  (xlib.XInternAtom)(display, name.as_ptr(), xlib::False)
}

/// The window the window manager manages for `window`: the first of it and its ancestors
/// carrying WM_STATE, or its top-level while the window manager has not taken it yet.
unsafe fn client_toplevel(
  xlib: &xlib::Xlib,
  display: *mut xlib::Display,
  mut window: xlib::Window,
) -> xlib::Window {
  let wm_state = atom(xlib, display, "WM_STATE");
  let root = (xlib.XDefaultRootWindow)(display);
  for _ in 0..16 {
    if property(xlib, display, window, wm_state, 0).is_some() {
      return window;
    }
    match parent_of(xlib, display, window) {
      Some(parent) if parent > 1 && parent != root => window = parent,
      _ => return window,
    }
  }
  window
}

unsafe fn parent_of(
  xlib: &xlib::Xlib,
  display: *mut xlib::Display,
  window: xlib::Window,
) -> Option<xlib::Window> {
  let (mut root, mut parent, mut children, mut count) = (0, 0, std::ptr::null_mut(), 0u32);
  if (xlib.XQueryTree)(display, window, &mut root, &mut parent, &mut children, &mut count) == 0 {
    return None;
  }
  if !children.is_null() {
    (xlib.XFree)(children.cast());
  }
  Some(parent)
}

/// Up to `max_bytes` of a property's value, or None when the window does not have it.
unsafe fn property(
  xlib: &xlib::Xlib,
  display: *mut xlib::Display,
  window: xlib::Window,
  name: xlib::Atom,
  max_bytes: usize,
) -> Option<Vec<u8>> {
  let (mut actual, mut format, mut items, mut after) = (0, 0, 0, 0);
  let mut data: *mut u8 = std::ptr::null_mut();
  let status = (xlib.XGetWindowProperty)(
    display,
    window,
    name,
    0,
    max_bytes.div_ceil(4) as std::os::raw::c_long,
    xlib::False,
    xlib::AnyPropertyType as xlib::Atom,
    &mut actual,
    &mut format,
    &mut items,
    &mut after,
    &mut data,
  );
  let value = (status == xlib::Success as i32 && actual != 0).then(|| {
    if data.is_null() || format != 8 {
      Vec::new()
    } else {
      std::slice::from_raw_parts(data, items as usize).to_vec()
    }
  });
  if !data.is_null() {
    (xlib.XFree)(data.cast());
  }
  value
}

unsafe fn window_name(xlib: &xlib::Xlib, display: *mut xlib::Display, window: xlib::Window) -> Option<String> {
  [atom(xlib, display, "_NET_WM_NAME"), xlib::XA_WM_NAME]
    .into_iter()
    .filter_map(|name| property(xlib, display, window, name, 1024))
    .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
    .find(|name| !name.trim().is_empty())
}

unsafe fn set_name(xlib: &xlib::Xlib, display: *mut xlib::Display, window: xlib::Window, name: &str) {
  let utf8 = atom(xlib, display, "UTF8_STRING");
  for property in [atom(xlib, display, "_NET_WM_NAME"), xlib::XA_WM_NAME] {
    (xlib.XChangeProperty)(
      display,
      window,
      property,
      utf8,
      8,
      xlib::PropModeReplace,
      name.as_ptr(),
      name.len() as std::os::raw::c_int,
    );
  }
}

unsafe fn transient_for(
  xlib: &xlib::Xlib,
  display: *mut xlib::Display,
  window: xlib::Window,
) -> Option<xlib::Window> {
  let mut owner = 0;
  ((xlib.XGetTransientForHint)(display, window, &mut owner) != 0 && owner > 1).then_some(owner)
}

#[cfg(test)]
mod tests {
  use super::popup_title;

  #[test]
  fn popup_titles_name_the_page_and_the_app_window() {
    assert_eq!(
      popup_title("휴대폰 본인확인 - 통신사 선택", Some("Ugot Agent Browser — 윤여원")),
      "휴대폰 본인확인 - 통신사 선택 — Ugot Agent Browser — 윤여원"
    );
    assert_eq!(popup_title("  ", Some("Ugot Agent Browser")), "Ugot Agent Browser");
    assert_eq!(popup_title("Report", None), "Report");
    assert_eq!(popup_title("", Some("  ")), "Popup");
  }
}

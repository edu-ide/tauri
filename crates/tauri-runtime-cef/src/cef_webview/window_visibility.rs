use windows::Win32::{
  Foundation::HWND,
  UI::WindowsAndMessaging::{
    SetWindowPos, ShowWindow, HWND_TOP, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW,
    SW_HIDE,
  },
};

// Browser HWNDs are siblings of the shell HWND. Showing an already-visible browser must
// also raise it above the shell, without moving, resizing, or activating either window.
pub(crate) fn set_window_visible(hwnd: HWND, visible: bool) {
  if visible {
    let _ = unsafe {
      SetWindowPos(
        hwnd,
        Some(HWND_TOP),
        0,
        0,
        0,
        0,
        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
      )
    };
  } else {
    let _ = unsafe { ShowWindow(hwnd, SW_HIDE) };
  }
}

#[cfg(all(test, windows))]
mod tests {
  use super::set_window_visible;
  use windows::{
    core::w,
    Win32::{
      Foundation::{HWND, RECT},
      UI::{
        Input::KeyboardAndMouse::{GetActiveWindow, GetFocus},
        WindowsAndMessaging::*,
      },
    },
  };

  struct HiddenParent(HWND);

  impl HiddenParent {
    fn new() -> Self {
      // Never show the parent: these regression fixtures must not affect real app windows.
      let hwnd = unsafe {
        CreateWindowExW(
          WINDOW_EX_STYLE::default(),
          w!("STATIC"),
          w!("CEF visibility regression"),
          WS_OVERLAPPEDWINDOW,
          17,
          23,
          320,
          240,
          None,
          None,
          None,
          None,
        )
      }
      .expect("create hidden fixture parent");
      assert!(!unsafe { IsWindowVisible(hwnd) }.as_bool());
      Self(hwnd)
    }

    fn child(&self, x: i32, y: i32, width: i32, height: i32) -> HWND {
      unsafe {
        CreateWindowExW(
          WINDOW_EX_STYLE::default(),
          w!("STATIC"),
          w!("CEF fixture child"),
          WS_CHILD | WS_VISIBLE,
          x,
          y,
          width,
          height,
          Some(self.0),
          None,
          None,
          None,
        )
      }
      .expect("create fixture child")
    }
  }

  impl Drop for HiddenParent {
    fn drop(&mut self) {
      let _ = unsafe { DestroyWindow(self.0) };
    }
  }

  fn rect(hwnd: HWND) -> (i32, i32, i32, i32) {
    let mut rect = RECT::default();
    unsafe { GetWindowRect(hwnd, &mut rect) }.expect("read fixture rectangle");
    (rect.left, rect.top, rect.right, rect.bottom)
  }

  fn has_visible_style(hwnd: HWND) -> bool {
    (unsafe { GetWindowLongW(hwnd, GWL_STYLE) } as u32 & WS_VISIBLE.0) != 0
  }

  fn focus_state() -> (HWND, HWND, HWND) {
    unsafe { (GetFocus(), GetActiveWindow(), GetForegroundWindow()) }
  }

  #[test]
  fn showing_visible_browser_raises_it_without_moving_resizing_or_activating() {
    let parent = HiddenParent::new();
    let browser = parent.child(11, 13, 100, 80);
    let shell = parent.child(0, 0, 300, 200);
    unsafe {
      SetWindowPos(
        shell,
        Some(HWND_TOP),
        0,
        0,
        0,
        0,
        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
      )
    }
    .expect("put shell above browser");
    assert_eq!(unsafe { GetTopWindow(Some(parent.0)) }.unwrap(), shell);
    assert!(has_visible_style(browser));
    let before_rect = rect(browser);
    let before_focus = focus_state();

    set_window_visible(browser, true);

    assert_eq!(unsafe { GetTopWindow(Some(parent.0)) }.unwrap(), browser);
    assert_eq!(rect(browser), before_rect);
    assert_eq!(focus_state(), before_focus);
    assert_eq!(unsafe { GetParent(browser) }.unwrap(), parent.0);
    assert!(has_visible_style(browser));
    assert!(!unsafe { IsWindowVisible(parent.0) }.as_bool());
  }

  #[test]
  fn hiding_then_showing_browser_restores_visibility_and_top_order() {
    let parent = HiddenParent::new();
    let browser = parent.child(7, 9, 90, 60);
    let shell = parent.child(0, 0, 300, 200);
    unsafe {
      SetWindowPos(
        shell,
        Some(HWND_TOP),
        0,
        0,
        0,
        0,
        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
      )
    }
    .expect("put shell above browser");
    assert_eq!(unsafe { GetTopWindow(Some(parent.0)) }.unwrap(), shell);
    let before_rect = rect(browser);
    let before_focus = focus_state();

    set_window_visible(browser, false);

    assert!(!has_visible_style(browser));
    assert_eq!(unsafe { GetTopWindow(Some(parent.0)) }.unwrap(), shell);
    assert_eq!(rect(browser), before_rect);
    assert_eq!(focus_state(), before_focus);

    set_window_visible(browser, true);

    assert!(has_visible_style(browser));
    assert_eq!(unsafe { GetTopWindow(Some(parent.0)) }.unwrap(), browser);
    assert_eq!(rect(browser), before_rect);
    assert_eq!(focus_state(), before_focus);
    assert_eq!(unsafe { GetParent(browser) }.unwrap(), parent.0);
    assert!(!unsafe { IsWindowVisible(parent.0) }.as_bool());
  }
}

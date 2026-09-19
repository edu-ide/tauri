//! Keep native Browser children above the host's recreated ANGLE surface.
//!
//! ANGLE's WindowSurfaceGLX creates and maps a full-size X11 child when an
//! EGL surface is recreated. It becomes the top sibling, covering independently
//! parented Browser windows even while their renderers and CDP remain healthy.
//! Observe only parents supplied by CefWindow/BrowserHost; never enumerate or
//! restack other desktop windows, infer tabs from URLs, or map hidden children.

use std::{
  cell::RefCell,
  collections::{HashMap, HashSet},
  time::{Duration, Instant},
};
use x11_dl::xlib;

use super::linux::X11;

thread_local! {
  static GUARD: RefCell<Option<ChildStackingGuard>> = const { RefCell::new(None) };
}

pub(super) fn register(parent: xlib::Window, child: xlib::Window) {
  // CEF reports PointerRoot (1) until the Views toplevel exists. Registering
  // that handle never sees the real children, so the Alloy shell stays on top
  // of every tab (2026-09-16: white content hole, chrome still painted).
  if parent <= 1 || child <= 1 || parent == child {
    return;
  }
  GUARD.with(|guard| {
    let mut guard = guard.borrow_mut();
    if guard.is_none() {
      *guard = ChildStackingGuard::new();
    }
    if let Some(guard) = guard.as_mut() {
      guard.register(parent, child);
    }
  });
}

pub(super) fn unregister(child: xlib::Window) {
  GUARD.with(|guard| {
    if let Some(guard) = guard.borrow_mut().as_mut() {
      guard.forget_child(child);
    }
  });
}

pub(crate) fn process_events() {
  GUARD.with(|guard| {
    if let Some(guard) = guard.borrow_mut().as_mut() {
      guard.process_events();
    }
  });
}

struct ChildStackingGuard {
  display: *mut xlib::Display,
  parents: HashMap<xlib::Window, HashSet<xlib::Window>>,
  last_event_check: Instant,
}

impl ChildStackingGuard {
  fn new() -> Option<Self> {
    let xlib = X11.as_ref()?;
    let display = unsafe { (xlib.XOpenDisplay)(std::ptr::null()) };
    (!display.is_null()).then(|| Self {
      display,
      parents: HashMap::new(),
      last_event_check: Instant::now(),
    })
  }

  fn register(&mut self, parent: xlib::Window, child: xlib::Window) {
    let Some(xlib) = X11.as_ref() else { return };
    self.forget_child(child);
    let children = self.parents.entry(parent).or_default();
    if children.is_empty() {
      // This is a separate connection: CEF retains its own event selection.
      unsafe {
        (xlib.XSelectInput)(
          self.display,
          parent,
          xlib::SubstructureNotifyMask | xlib::StructureNotifyMask,
        );
        (xlib.XFlush)(self.display);
      }
    }
    children.insert(child);
  }

  fn forget_child(&mut self, child: xlib::Window) {
    for children in self.parents.values_mut() {
      children.remove(&child);
    }
  }

  fn process_events(&mut self) {
    // The external CEF message pump can spin without sleeping. Read the event
    // socket at most once per frame. Always re-check our own parents: ANGLE
    // remaps to the top without an event this connection sees, and XGrabServer
    // here froze GNOME so the titlebar could not move the window.
    if self.last_event_check.elapsed() < Duration::from_millis(16) {
      return;
    }
    self.last_event_check = Instant::now();
    let Some(xlib) = X11.as_ref() else { return };
    unsafe {
      while (xlib.XPending)(self.display) > 0 {
        let mut event: xlib::XEvent = std::mem::zeroed();
        (xlib.XNextEvent)(self.display, &mut event);
        match event.get_type() {
          xlib::DestroyNotify => {
            self.parents.remove(&event.destroy_window.window);
            self.forget_child(event.destroy_window.window);
          }
          xlib::ReparentNotify => {
            let event = event.reparent;
            self.forget_child(event.window);
            if event.parent > 1 {
              if let Some(children) = self.parents.get_mut(&event.parent) {
                children.insert(event.window);
              }
            }
          }
          _ => {}
        }
      }
      let parents: Vec<(xlib::Window, HashSet<xlib::Window>)> = self
        .parents
        .iter()
        .map(|(p, c)| (*p, c.clone()))
        .collect();
      for (parent, children) in parents {
        let _ = repair_parent(xlib, self.display, parent, &children);
      }
    }
  }
}

impl Drop for ChildStackingGuard {
  fn drop(&mut self) {
    if let Some(xlib) = X11.as_ref() {
      unsafe { (xlib.XCloseDisplay)(self.display) };
    }
  }
}

struct ServerGrab<'a> {
  xlib: &'a xlib::Xlib,
  display: *mut xlib::Display,
}

impl<'a> ServerGrab<'a> {
  unsafe fn new(xlib: &'a xlib::Xlib, display: *mut xlib::Display) -> Self {
    (xlib.XGrabServer)(display);
    // Receive every DestroyNotify preceding the grab before consulting IDs.
    (xlib.XSync)(display, xlib::False);
    Self { xlib, display }
  }
}

impl Drop for ServerGrab<'_> {
  fn drop(&mut self) {
    unsafe {
      (self.xlib.XUngrabServer)(self.display);
      (self.xlib.XFlush)(self.display);
    }
  }
}

unsafe fn children_of(
  xlib: &xlib::Xlib,
  display: *mut xlib::Display,
  window: xlib::Window,
) -> Option<(xlib::Window, Vec<xlib::Window>)> {
  let (mut root, mut parent, mut count) = (0, 0, 0);
  let mut children = std::ptr::null_mut();
  let success = (xlib.XQueryTree)(
    display,
    window,
    &mut root,
    &mut parent,
    &mut children,
    &mut count,
  );
  let result = if success == 0 {
    None
  } else if children.is_null() {
    Some((parent, Vec::new()))
  } else {
    Some((
      parent,
      std::slice::from_raw_parts(children, count as usize).to_vec(),
    ))
  };
  if !children.is_null() {
    (xlib.XFree)(children.cast());
  }
  result
}

#[derive(Clone, Copy)]
struct Child {
  id: xlib::Window,
  managed: bool,
  visible: bool,
  host_surface: bool,
  inset: bool,
}

fn stacking_actions(children: &[Child]) -> (Vec<xlib::Window>, Vec<xlib::Window>, Vec<xlib::Window>) {
  let lower = children
    .iter()
    .filter(|c| c.visible && c.host_surface)
    .map(|c| c.id)
    .collect();
  let raise_shell = children
    .iter()
    .filter(|c| c.visible && !c.host_surface && !c.inset)
    .map(|c| c.id)
    .collect();
  // Inset tabs are raised even when register() missed them. The Alloy shell
  // is a full-size managed sibling; raising it every frame without this
  // leaves the white content hole on top (2026-09-16).
  let raise_tabs = children
    .iter()
    .filter(|c| c.visible && c.inset)
    .map(|c| c.id)
    .collect();
  (lower, raise_shell, raise_tabs)
}

fn stacking_ok(children: &[Child]) -> bool {
  let vis: Vec<&Child> = children.iter().filter(|c| c.visible).collect();
  if vis.is_empty() {
    return true;
  }
  let mut seen_non_host = false;
  let mut seen_inset = false;
  for child in &vis {
    if seen_non_host && child.host_surface {
      return false;
    }
    if !child.host_surface {
      seen_non_host = true;
    }
    if seen_inset && !child.inset {
      return false;
    }
    if child.inset {
      seen_inset = true;
    }
  }
  !vis.iter().any(|c| c.inset) || vis.last().is_some_and(|c| c.inset)
}

// QueryTree is ordered bottom to top. Preserve the relative order of every
// managed webview (including shell vs active tab) and every unrelated sibling.
fn surface_repairs(children: &[Child]) -> Vec<(xlib::Window, xlib::Window)> {
  let Some((bottom, anchor)) = children
    .iter()
    .enumerate()
    .find(|(_, c)| c.managed && c.visible)
  else {
    return Vec::new();
  };
  children
    .iter()
    .enumerate()
    .filter_map(|(index, child)| {
      (!child.managed && child.visible && child.host_surface && index > bottom)
        .then_some((child.id, anchor.id))
    })
    .collect()
}

fn matches_host_surface(attrs: &xlib::XWindowAttributes, parent: &xlib::XWindowAttributes) -> bool {
  attrs.map_state == xlib::IsViewable
    && attrs.class == xlib::InputOutput
    && attrs.override_redirect == xlib::False
    && attrs.all_event_masks == xlib::ExposureMask
    && attrs.do_not_propagate_mask == 0
    && attrs.border_width == 0
    && attrs.x == 0
    && attrs.y == 0
    && attrs.width == parent.width
    && attrs.height == parent.height
}

/// ANGLE's WindowSurfaceGLX is sometimes 1–2px larger than the CefWindow
/// (2026-09-17: 1400×901 on a 1400×900 parent, ExposureMask only). That child
/// still paints the host surface. Treating it as an inset tab raises it above
/// chrome and the active page.
fn covers_host_surface(attrs: &xlib::XWindowAttributes, parent: &xlib::XWindowAttributes) -> bool {
  attrs.x == 0
    && attrs.y == 0
    && attrs.border_width == 0
    && attrs.width >= parent.width
    && attrs.height >= parent.height
    && attrs.width <= parent.width.saturating_add(2)
    && attrs.height <= parent.height.saturating_add(2)
}

fn looks_like_angle(attrs: &xlib::XWindowAttributes, parent: &xlib::XWindowAttributes) -> bool {
  if matches_host_surface(attrs, parent) {
    return true;
  }
  attrs.map_state == xlib::IsViewable
    && attrs.class == xlib::InputOutput
    && attrs.override_redirect == xlib::False
    && covers_host_surface(attrs, parent)
    && (attrs.all_event_masks & !xlib::ExposureMask) == 0
}

fn looks_like_inset(attrs: &xlib::XWindowAttributes, parent: &xlib::XWindowAttributes) -> bool {
  attrs.map_state == xlib::IsViewable
    && attrs.class == xlib::InputOutput
    && attrs.override_redirect == xlib::False
    && !covers_host_surface(attrs, parent)
    && (attrs.width != parent.width || attrs.height != parent.height || attrs.x != 0 || attrs.y != 0)
}

unsafe fn repair_parent(
  xlib: &xlib::Xlib,
  display: *mut xlib::Display,
  parent: xlib::Window,
  managed: &HashSet<xlib::Window>,
) -> Vec<(xlib::Window, xlib::Window)> {
  let Some((_, children)) = children_of(xlib, display, parent) else {
    return Vec::new();
  };
  let mut parent_attrs: xlib::XWindowAttributes = std::mem::zeroed();
  if (xlib.XGetWindowAttributes)(display, parent, &mut parent_attrs) == 0 {
    return Vec::new();
  }
  let mut snapshot = Vec::with_capacity(children.len());
  for child in children {
    let mut attrs: xlib::XWindowAttributes = std::mem::zeroed();
    if (xlib.XGetWindowAttributes)(display, child, &mut attrs) == 0 {
      continue;
    }
    let is_managed = managed.contains(&child);
    let visible = attrs.map_state == xlib::IsViewable;
    let inset = looks_like_inset(&attrs, &parent_attrs);
    // ExposureMask-only host-sized windows are ANGLE, including a 1–2px GLX
    // overflow. The Alloy shell is also 0,0×parent but has pointer masks —
    // lowering it hid the page under chrome, and raising a misclassified
    // overflow surface as a tab covered chrome (2026-09-17).
    let host_surface = !inset && looks_like_angle(&attrs, &parent_attrs);
    snapshot.push(Child {
      id: child,
      managed: is_managed,
      visible,
      host_surface,
      inset,
    });
  }
  if !snapshot
    .iter()
    .any(|c| c.managed || c.host_surface || c.inset)
  {
    return Vec::new();
  }
  let repaired = surface_repairs(&snapshot);
  // XLower/XRaise every 16ms even when order is already correct flashes the
  // compositor and steals the pointer grab, so the window cannot be dragged
  // (2026-09-16).
  if !stacking_ok(&snapshot) {
    let (lower, raise_shell, raise_tabs) = stacking_actions(&snapshot);
    for id in lower {
      (xlib.XLowerWindow)(display, id);
    }
    for id in raise_shell {
      (xlib.XRaiseWindow)(display, id);
    }
    for id in raise_tabs {
      (xlib.XRaiseWindow)(display, id);
    }
  }
  repaired
}

#[cfg(test)]
mod tests {
  use super::*;

  fn child(id: xlib::Window, managed: bool, visible: bool, host_surface: bool) -> Child {
    Child {
      id,
      managed,
      visible,
      host_surface,
      inset: false,
    }
  }

  fn inset_tab(id: xlib::Window, managed: bool) -> Child {
    Child {
      id,
      managed,
      visible: true,
      host_surface: false,
      inset: true,
    }
  }

  #[test]
  fn recreated_surface_moves_below_shell_preserving_active_tab_order() {
    let children = [
      child(1, true, true, false),
      child(2, true, true, false),
      child(3, false, true, true),
    ];
    assert_eq!(surface_repairs(&children), [(3, 1)]);
    let repaired = [children[2], children[0], children[1]];
    assert!(surface_repairs(&repaired).is_empty());
  }

  #[test]
  fn unregistered_inset_tab_is_raised_above_full_size_shell() {
    let children = [
      child(1, true, true, false),
      inset_tab(2, false),
      child(3, false, true, true),
    ];
    let (lower, raise_shell, raise_tabs) = stacking_actions(&children);
    assert_eq!(lower, vec![3]);
    assert_eq!(raise_shell, vec![1]);
    assert_eq!(raise_tabs, vec![2]);
    assert!(!stacking_ok(&children));
  }

  #[test]
  fn already_correct_stack_skips_restack() {
    let children = [
      child(3, false, true, true),
      child(1, true, true, false),
      inset_tab(2, false),
    ];
    assert!(stacking_ok(&children));
  }

  #[test]
  fn hidden_tabs_popups_and_unowned_hosts_do_not_authorize_restacking() {
    let children = [
      child(1, true, false, false),
      child(2, false, true, false),
      child(3, false, true, true),
    ];
    assert!(surface_repairs(&children).is_empty());
    let children = [
      child(1, true, true, false),
      child(2, false, true, false),
      child(3, false, false, true),
    ];
    assert!(surface_repairs(&children).is_empty());
  }

  #[test]
  fn surface_requires_exact_host_geometry_without_native_input() {
    let mut parent: xlib::XWindowAttributes = unsafe { std::mem::zeroed() };
    parent.width = 1201;
    parent.height = 1008;
    let mut surface = parent;
    surface.class = xlib::InputOutput;
    surface.map_state = xlib::IsViewable;
    surface.all_event_masks = xlib::ExposureMask;
    assert!(matches_host_surface(&surface, &parent));
    for mutate in [
      |a: &mut xlib::XWindowAttributes| a.all_event_masks |= xlib::ButtonPressMask,
      |a: &mut xlib::XWindowAttributes| a.override_redirect = xlib::True,
      |a: &mut xlib::XWindowAttributes| a.x = 1,
      |a: &mut xlib::XWindowAttributes| a.width -= 1,
      |a: &mut xlib::XWindowAttributes| a.map_state = xlib::IsUnmapped,
    ] {
      let mut other = surface;
      mutate(&mut other);
      assert!(!matches_host_surface(&other, &parent));
    }
  }

  #[test]
  fn one_pixel_taller_glx_surface_is_host_not_tab() {
    let mut parent: xlib::XWindowAttributes = unsafe { std::mem::zeroed() };
    parent.width = 1400;
    parent.height = 900;
    let mut surface = parent;
    surface.class = xlib::InputOutput;
    surface.map_state = xlib::IsViewable;
    surface.all_event_masks = xlib::ExposureMask;
    surface.height = 901;
    assert!(looks_like_angle(&surface, &parent));
    assert!(!looks_like_inset(&surface, &parent));
    assert!(!matches_host_surface(&surface, &parent));

    let mut tab = surface;
    tab.width = 1060;
    tab.height = 816;
    tab.y = 84;
    tab.all_event_masks = xlib::StructureNotifyMask;
    assert!(looks_like_inset(&tab, &parent));
    assert!(!looks_like_angle(&tab, &parent));
  }
}

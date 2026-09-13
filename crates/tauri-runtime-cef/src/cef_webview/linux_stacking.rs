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
  if parent == 0 || child == 0 || parent == child {
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
    self.parents.retain(|_, children| {
      children.remove(&child);
      !children.is_empty()
    });
  }

  fn process_events(&mut self) {
    // The external CEF message pump can spin without sleeping. Read the event
    // socket at most once per frame; no periodic window-tree query is needed.
    if self.last_event_check.elapsed() < Duration::from_millis(16) {
      return;
    }
    self.last_event_check = Instant::now();
    let Some(xlib) = X11.as_ref() else { return };
    unsafe {
      if (xlib.XPending)(self.display) == 0 {
        return;
      }

      // Freeze other clients only while draining already-pending structural
      // events and repairing their stack. In particular, a GPU process cannot
      // destroy a queried surface between QueryTree and ConfigureWindow.
      let _grab = ServerGrab::new(xlib, self.display);
      let mut changed = HashSet::new();
      let mut repaired = Vec::new();
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
            if event.parent != event.event {
              if let Some(children) = self.parents.get_mut(&event.event) {
                children.remove(&event.window);
              }
            }
            changed.insert(event.parent);
          }
          xlib::CreateNotify => {
            changed.insert(event.create_window.parent);
          }
          xlib::MapNotify => {
            changed.insert(event.map.event);
          }
          xlib::ConfigureNotify => {
            changed.insert(event.configure.event);
          }
          _ => {}
        }
      }
      for parent in changed {
        if let Some(children) = self.parents.get(&parent) {
          repaired.extend(
            repair_parent(xlib, self.display, parent, children)
              .into_iter()
              .map(|(surface, sibling)| (parent, surface, sibling)),
          );
        }
      }
      drop(_grab);
      // A redirected stderr pipe can block. Never hold the display-wide grab
      // while writing diagnostics or waiting for anything outside this X client.
      for (parent, surface, sibling) in repaired {
        eprintln!("[tauri-cef] native host surface restacked: parent={parent:#x} surface={surface:#x} below={sibling:#x}");
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

unsafe fn repair_parent(
  xlib: &xlib::Xlib,
  display: *mut xlib::Display,
  parent: xlib::Window,
  managed: &HashSet<xlib::Window>,
) -> Vec<(xlib::Window, xlib::Window)> {
  let Some((_, children)) = children_of(xlib, display, parent) else {
    return Vec::new();
  };
  // These native BrowserHost IDs must still be direct children of this exact
  // CefWindow. A moved/closed webview cannot authorize changes to another host.
  if !children.iter().any(|child| managed.contains(child)) {
    return Vec::new();
  }
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
    // Match ANGLE's input-free, full-host backing surface, not native popups,
    // dialogs, registered webviews or arbitrary property-bearing windows.
    let mut host_surface = !is_managed && matches_host_surface(&attrs, &parent_attrs);
    if host_surface {
      host_surface = matches!(children_of(xlib, display, child), Some((p, ref c)) if p == parent && c.is_empty());
      let mut property_count = 0;
      let properties = (xlib.XListProperties)(display, child, &mut property_count);
      host_surface &= property_count == 0;
      if !properties.is_null() {
        (xlib.XFree)(properties.cast());
      }
    }
    snapshot.push(Child {
      id: child,
      managed: is_managed,
      visible,
      host_surface,
    });
  }
  let repaired = surface_repairs(&snapshot);
  for &(surface, sibling) in &repaired {
    let mut changes: xlib::XWindowChanges = std::mem::zeroed();
    changes.sibling = sibling;
    changes.stack_mode = xlib::Below;
    (xlib.XConfigureWindow)(
      display,
      surface,
      (xlib::CWSibling | xlib::CWStackMode).into(),
      &mut changes,
    );
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
}

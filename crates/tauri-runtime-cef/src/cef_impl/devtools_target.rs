// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use std::sync::{Arc, Mutex};

use cef::{rc::*, *};

#[derive(Default)]
struct TargetState {
  pending_message_id: Option<i32>,
  submitting: bool,
  early_results: Vec<(i32, Option<String>)>,
  completed: bool,
  target_id: Option<String>,
}

impl TargetState {
  fn begin_request(&mut self) -> bool {
    if self.completed || self.submitting || self.pending_message_id.is_some() {
      return false;
    }
    self.submitting = true;
    self.early_results.clear();
    true
  }

  fn finish_submission(&mut self, message_id: i32) {
    self.submitting = false;
    self.pending_message_id = (message_id != 0).then_some(message_id);
    if message_id != 0 {
      if let Some(index) = self.early_results.iter().position(|(id, _)| *id == message_id) {
        let (_, target_id) = self.early_results.swap_remove(index);
        self.complete(target_id);
      }
    }
    self.early_results.clear();
  }

  fn complete(&mut self, target_id: Option<String>) {
    self.pending_message_id = None;
    self.completed = true;
    self.target_id = target_id;
  }

  fn receive_result(&mut self, message_id: i32, success: i32, result: Option<&[u8]>) {
    if self.pending_message_id != Some(message_id) && !self.submitting {
      return;
    }
    let target_id = if success == 1 {
      result.and_then(parse_target_id)
    } else {
      None
    };
    if self.submitting {
      // Browser-side methods can respond inside ExecuteDevToolsMethod before
      // it returns the assigned ID. Retain only parsed identity results and
      // accept exactly that returned ID when the call finishes.
      self.early_results.push((message_id, target_id));
    } else {
      self.complete(target_id);
    }
  }

  fn detached(&mut self) {
    // Target IDs belong to the Browser, not its renderer or DevTools session.
    // Only an unfinished lookup needs to be retried after a detach.
    if !self.completed {
      self.pending_message_id = None;
    }
  }
}

fn resolve_target_id(state: &Mutex<TargetState>, dispatch: impl FnOnce() -> i32) -> Option<String> {
  {
    let mut state = state.lock().ok()?;
    if !state.begin_request() {
      return state.target_id.clone();
    }
  }
  // Do not hold the state lock across native dispatch: CEF may call the
  // observer synchronously, or enqueue the response for a later message loop.
  let message_id = dispatch();
  let mut state = state.lock().ok()?;
  state.finish_submission(message_id);
  state.target_id.clone()
}

fn parse_target_id(result: &[u8]) -> Option<String> {
  let result: serde_json::Value = serde_json::from_slice(result).ok()?;
  result.get("targetInfo")?.get("targetId")?.as_str()
    .filter(|id| !id.is_empty())
    .map(str::to_owned)
}

wrap_dev_tools_message_observer! {
  struct TargetIdentityObserver {
    browser_id: i32,
    state: Arc<Mutex<TargetState>>,
  }

  impl DevToolsMessageObserver {
    fn on_dev_tools_method_result(
      &self,
      browser: Option<&mut Browser>,
      message_id: i32,
      success: i32,
      result: Option<&[u8]>,
    ) {
      if !browser.is_some_and(|browser| browser.identifier() == self.browser_id) {
        return;
      }
      if let Ok(mut state) = self.state.lock() {
        if state.submitting || state.pending_message_id == Some(message_id) {
          let phase = if state.submitting { "submitting" } else { "pending" };
          eprintln!("[tauri-cef] native target response: browser_id={} message_id={message_id} success={success} phase={phase}", self.browser_id);
        }
        state.receive_result(message_id, success, result);
      }
    }

    fn on_dev_tools_agent_detached(&self, browser: Option<&mut Browser>) {
      if browser.is_some_and(|browser| browser.identifier() == self.browser_id) {
        if let Ok(mut state) = self.state.lock() {
          state.detached();
        }
      }
    }
  }
}

/// A browser-owned, native identity lookup. The registration is retained by the
/// AppWebview and dropped with it; the observer owns only the response state.
pub(crate) struct NativeDevToolsTarget {
  browser_id: i32,
  state: Arc<Mutex<TargetState>>,
  _registration: Registration,
}

impl NativeDevToolsTarget {
  pub(crate) fn new(browser: &Browser) -> Option<Self> {
    let host = browser.host()?;
    let state = Arc::new(Mutex::new(TargetState::default()));
    let mut observer = TargetIdentityObserver::new(browser.identifier(), state.clone());
    let registration = host.add_dev_tools_message_observer(Some(&mut observer))?;
    Some(Self {
      browser_id: browser.identifier(),
      state,
      _registration: registration,
    })
  }

  pub(crate) fn browser_id(&self) -> i32 {
    self.browser_id
  }

  pub(crate) fn target_id(&self, browser: &Browser) -> Option<String> {
    if browser.identifier() != self.browser_id || browser.is_valid() == 0 {
      return None;
    }
    let host = browser.host()?;
    // CEF 144 has no GetDevToolsURL API. Ask this exact native BrowserHost for
    // its identity instead of matching remote target URLs or evaluating page
    // JavaScript. Use CEF's assigned IDs to avoid collisions with Inspector
    // subscriptions and other native clients sharing this browser's session.
    resolve_target_id(&self.state, || {
      let message_id = host.execute_dev_tools_method(
        0,
        Some(&CefString::from("Target.getTargetInfo")),
        None,
      );
      eprintln!("[tauri-cef] native target request submitted: browser_id={} message_id={message_id}", self.browser_id);
      message_id
    })
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn uses_only_native_target_identity_not_page_metadata() {
    assert_eq!(
      parse_target_id(br#"{"targetInfo":{"targetId":"native-target","url":"https://same.example/","title":"same"}}"#),
      Some("native-target".into())
    );
    for result in [
      br#"{"targetInfo":{"url":"https://same.example/","title":"native-target"}}"#.as_slice(),
      br#"{"targetInfo":{"targetId":""}}"#.as_slice(),
      br#"{"targetInfo":{"targetId":42}}"#.as_slice(),
      br#"{"targetId":"wrong-level"}"#.as_slice(),
      b"invalid json".as_slice(),
    ] {
      assert_eq!(parse_target_id(result), None);
    }
  }

  #[test]
  fn matches_native_request_and_preserves_identity_across_renderer_detach() {
    let mut state = TargetState {
      pending_message_id: Some(42),
      ..Default::default()
    };
    let result = Some(br#"{"targetInfo":{"targetId":"native-target"}}"#.as_slice());
    state.receive_result(41, 1, result);
    assert_eq!(state.pending_message_id, Some(42));
    assert_eq!(state.target_id, None);
    state.receive_result(42, 1, result);
    assert_eq!(state.target_id.as_deref(), Some("native-target"));
    state.detached();
    assert!(state.completed);
    assert_eq!(state.target_id.as_deref(), Some("native-target"));
  }

  #[test]
  fn detached_pending_lookup_can_retry_but_error_cannot_supply_an_identity() {
    let mut state = TargetState {
      pending_message_id: Some(42),
      ..Default::default()
    };
    state.detached();
    assert_eq!(state.pending_message_id, None);
    assert!(!state.completed);
    state.pending_message_id = Some(43);
    state.receive_result(43, 0, Some(br#"{"targetInfo":{"targetId":"untrusted-error"}}"#));
    assert!(state.completed);
    assert_eq!(state.target_id, None);
  }

  #[test]
  fn accepts_a_native_response_before_dispatch_returns_its_assigned_id() {
    let state = Mutex::new(TargetState::default());
    let result = resolve_target_id(&state, || {
      let mut state = state.lock().unwrap();
      // An unrelated observer response cannot become this webview's identity.
      state.receive_result(41, 1, Some(br#"{"targetInfo":{"targetId":"other-request"}}"#));
      state.receive_result(42, 1, Some(br#"{"targetInfo":{"targetId":"native-target"}}"#));
      42
    });
    assert_eq!(result.as_deref(), Some("native-target"));
    assert_eq!(state.lock().unwrap().pending_message_id, None);
    assert!(state.lock().unwrap().early_results.is_empty());
    assert_eq!(
      resolve_target_id(&state, || panic!("resolved identity must not dispatch again")),
      Some("native-target".into())
    );
  }

  #[test]
  fn accepts_a_native_response_after_dispatch_without_duplicate_requests() {
    let state = Mutex::new(TargetState::default());
    assert_eq!(resolve_target_id(&state, || 42), None);
    assert_eq!(resolve_target_id(&state, || panic!("lookup is already pending")), None);
    state.lock().unwrap().receive_result(42, 1, Some(br#"{"targetInfo":{"targetId":"native-target"}}"#));
    assert_eq!(
      resolve_target_id(&state, || panic!("resolved identity must not dispatch again")),
      Some("native-target".into())
    );
  }
}

// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use std::{
  borrow::Cow,
  collections::VecDeque,
  io::{Cursor, Read},
  sync::{Arc, Mutex},
  time::{Duration, Instant},
};

use cef::{rc::*, *};
use crate::thread_safe_cell::RefCell;
use html5ever::{interface::QualName, namespace_url, ns, LocalName};
use http::{
  header::{CONTENT_SECURITY_POLICY, CONTENT_TYPE},
  HeaderMap, HeaderName, HeaderValue,
};
use kuchiki::NodeRef;
use tauri_runtime::{webview::UriSchemeProtocolHandler, UserEvent};
use tauri_utils::{
  config::{Csp, CspDirectiveSources},
  html::{parse as parse_html, serialize_node},
};
use url::Url;

use crate::cef_impl::Context;

use super::CefInitScript;

fn csp_inject_initialization_scripts_hashes(
  existing_csp: String,
  initialization_scripts: &[CefInitScript],
) -> String {
  if initialization_scripts.is_empty() {
    return existing_csp;
  }

  // For custom schemes, include ALL script hashes (we inject all scripts into HTML)
  // This matches the HTML injection behavior in inject_scripts_into_html_body
  let script_hashes: Vec<String> = initialization_scripts
    .iter()
    .map(|s| s.hash.clone())
    .collect();

  if script_hashes.is_empty() {
    return existing_csp;
  }

  // Parse CSP using tauri-utils
  let mut csp_map: std::collections::HashMap<String, CspDirectiveSources> =
    Csp::Policy(existing_csp.to_string()).into();

  // Update or create script-src directive with script hashes
  let script_src = csp_map
    .entry("script-src".to_string())
    .or_insert_with(|| CspDirectiveSources::List(vec!["'self'".to_string()]));

  // Extend with script hashes
  script_src.extend(script_hashes);

  // Convert back to CSP string
  Csp::DirectiveMap(csp_map).to_string()
}

/// Helper function to inject initialization scripts into HTML body
fn inject_scripts_into_html_body(
  body: &[u8],
  initialization_scripts: &[CefInitScript],
) -> Option<Vec<u8>> {
  // Check if body is valid UTF-8 HTML
  let Ok(body_str) = std::str::from_utf8(body) else {
    return None;
  };

  // Parse HTML and inject scripts
  let document = parse_html(body_str.to_string());

  let head = if let Ok(ref head_node) = document.select_first("head") {
    head_node.as_node().clone()
  } else {
    let head_node = NodeRef::new_element(
      QualName::new(None, ns!(html), LocalName::from("head")),
      None,
    );
    document.prepend(head_node.clone());
    head_node
  };

  // Inject initialization scripts (for custom schemes, inject all scripts)
  for init_script in initialization_scripts.iter().rev() {
    let script_el = NodeRef::new_element(QualName::new(None, ns!(html), "script".into()), None);
    script_el.append(NodeRef::new_text(init_script.script.script.as_str()));
    head.prepend(script_el);
  }

  // Serialize the modified HTML
  Some(serialize_node(&document))
}

wrap_resource_request_handler! {
  pub struct WebResourceRequestHandler {
    initialization_scripts: Arc<Vec<CefInitScript>>,
  }

  impl ResourceRequestHandler {


    fn on_before_resource_load(
      &self,
      _browser: Option<&mut Browser>,
      _frame: Option<&mut Frame>,
      _request: Option<&mut Request>,
      _callback: Option<&mut Callback>,
    ) -> ReturnValue {
      sys::cef_return_value_t::RV_CONTINUE.into()
    }
  }
}

const RENDERER_RECOVERY_WINDOW: Duration = Duration::from_secs(60);
const MAX_RENDERER_RECOVERIES: usize = 3;

/// Owned by BrowserClient, since CEF can request a new RequestHandler after a crash.
#[derive(Default)]
pub struct RendererRecoveryState {
  attempts: VecDeque<Instant>,
}

impl RendererRecoveryState {
  fn reserve_attempt(&mut self, now: Instant) -> Option<usize> {
    while let Some(attempt) = self.attempts.front() {
      if now.saturating_duration_since(*attempt) < RENDERER_RECOVERY_WINDOW {
        break;
      }
      self.attempts.pop_front();
    }
    if self.attempts.len() >= MAX_RENDERER_RECOVERIES {
      return None;
    }
    self.attempts.push_back(now);
    Some(self.attempts.len())
  }
}

pub(super) fn renderer_recovery_origin(
  initial_url: Option<&str>,
  custom_scheme_domain_names: &[String],
  custom_protocol_scheme: &str,
) -> Option<Url> {
  let url = Url::parse(initial_url?).ok()?;
  if !matches!(custom_protocol_scheme, "http" | "https")
    || url.scheme() != custom_protocol_scheme
    || !url.username().is_empty()
    || url.password().is_some()
    || !custom_scheme_domain_names
      .iter()
      .any(|domain| Some(domain.as_str()) == url.host_str())
  {
    return None;
  }
  Some(url)
}

fn matches_recovery_origin(origin: &Url, current_url: &str) -> bool {
  let Ok(current) = Url::parse(current_url) else {
    return false;
  };
  current.username().is_empty()
    && current.password().is_none()
    && current.origin() == origin.origin()
}

wrap_task! {
  struct RecoverInternalRendererTask {
    browser: Browser,
    origin: Url,
  }

  impl Task {
    fn execute(&self) {
      if self.browser.is_valid() == 0 {
        return;
      }
      let Some(frame) = self.browser.main_frame() else {
        return;
      };
      // A navigation or close may have happened while this UI task was queued.
      let current_url = CefString::from(&frame.url()).to_string();
      if !matches_recovery_origin(&self.origin, &current_url) {
        return;
      }
      eprintln!(
        "[tauri-cef] recovering internal renderer: browser_id={}",
        self.browser.identifier()
      );
      self.browser.reload();
    }
  }
}

wrap_request_handler! {
  pub struct WebRequestHandler {
    initialization_scripts: Arc<Vec<CefInitScript>>,
    navigation_handler: Option<Arc<tauri_runtime::webview::NavigationHandler>>,
    recovery_origin: Option<Url>,
    renderer_recovery: Arc<Mutex<RendererRecoveryState>>,
  }

  impl RequestHandler {
    fn on_render_process_terminated(
      &self,
      browser: Option<&mut Browser>,
      status: TerminationStatus,
      error_code: ::std::os::raw::c_int,
      _error_string: Option<&CefString>,
    ) {
      let Some(browser) = browser else {
        return;
      };
      let browser_id = browser.identifier();
      eprintln!(
        "[tauri-cef] renderer terminated: browser_id={browser_id} status={status:?} error_code={error_code}"
      );
      // Other Tauri applications may own unsaved documents. Recovery is opt-in
      // for applications that can safely reconstruct their internal views.
      if std::env::var("TAURI_CEF_RECOVER_APP_RENDERERS").as_deref() != Ok("1") {
        return;
      }
      let Some(origin) = &self.recovery_origin else {
        return;
      };
      if browser.is_valid() == 0 {
        return;
      }
      let Some(frame) = browser.main_frame() else {
        return;
      };
      let current_url = CefString::from(&frame.url()).to_string();
      if !matches_recovery_origin(origin, &current_url) {
        return;
      }
      let attempt = match self.renderer_recovery.lock() {
        Ok(mut recovery) => recovery.reserve_attempt(Instant::now()),
        Err(_) => {
          eprintln!("[tauri-cef] renderer recovery state unavailable: browser_id={browser_id}");
          return;
        }
      };
      let Some(attempt) = attempt else {
        eprintln!(
          "[tauri-cef] renderer recovery suppressed: browser_id={browser_id} limit={MAX_RENDERER_RECOVERIES}/60s"
        );
        return;
      };
      eprintln!(
        "[tauri-cef] renderer recovery queued: browser_id={browser_id} attempt={attempt}/{MAX_RENDERER_RECOVERIES}"
      );
      // Reloading within the termination callback can reenter CEF while it tears
      // down the old renderer. Run it in a separate UI task instead.
      let mut task = RecoverInternalRendererTask::new(browser.clone(), origin.clone());
      if cef::post_task(sys::cef_thread_id_t::TID_UI.into(), Some(&mut task)) == 0 {
        eprintln!("[tauri-cef] renderer recovery task rejected: browser_id={browser_id}");
      }
    }

    fn on_before_browse(
      &self,
      _browser: Option<&mut Browser>,
      frame: Option<&mut Frame>,
      request: Option<&mut Request>,
      _user_gesture: ::std::os::raw::c_int,
      _is_redirect: ::std::os::raw::c_int,
    ) -> ::std::os::raw::c_int {
      let Some(frame) = frame else {
        return 0;
      };
      // we only fire main frame navigation events to match the behavior of the wry runtime
      if frame.is_main() == 0 {
        return 0;
      }
      let Some(handler) = &self.navigation_handler else {
        return 0;
      };
      let Some(request) = request else {
        return 0;
      };

      let url_str = CefString::from(&request.url()).to_string();
      let Ok(url) = url::Url::parse(&url_str) else {
        return 0;
      };
      let should_navigate = handler(&url);
      if should_navigate {
        0
      } else {
        1
      }
    }

    fn resource_request_handler(
      &self,
      _browser: Option<&mut Browser>,
      _frame: Option<&mut Frame>,
      _request: Option<&mut Request>,
      _is_navigation: ::std::os::raw::c_int,
      _is_download: ::std::os::raw::c_int,
      _request_initiator: Option<&CefString>,
      _disable_default_handling: Option<&mut ::std::os::raw::c_int>,
    ) -> Option<ResourceRequestHandler> {
      Some(WebResourceRequestHandler::new(
        self.initialization_scripts.clone(),
      ))
    }
  }
}

wrap_resource_handler! {
  pub struct WebResourceHandler {
    webview_label: String,
    handler: Arc<Box<UriSchemeProtocolHandler>>,
    initialization_scripts: Arc<Vec<CefInitScript>>,
    // we clone response to send it to the handler thread
    response: Arc<RefCell<Option<http::Response<Cursor<Vec<u8>>>>>>,
  }

  impl ResourceHandler {
    fn process_request(
      &self,
      request: Option<&mut Request>,
      callback: Option<&mut Callback>,
    ) -> ::std::os::raw::c_int {
      let Some(request) = request else { return 0 };
      let Some(callback) = callback else { return 0 };

      let url = CefString::from(&request.url()).to_string();
      let url = Url::parse(&url).ok();

      if let Some(url) = url {
        let callback = ThreadSafe(callback.clone());
        let response_store = ThreadSafe(self.response.clone());
        let initialization_scripts = self.initialization_scripts.clone();
        let responder = Box::new(move |response: http::Response<Cow<'static, [u8]>>| {
          // Check if this is an HTML response that needs script injection
          let content_type = response.headers().get(CONTENT_TYPE);
          let is_html = content_type
            .and_then(|ct| ct.to_str().ok())
            .map(|ct| ct.to_lowercase().starts_with("text/html"))
            .unwrap_or(false);

          let (parts, body) = response.into_parts();
          let body_bytes = body.into_owned();

          let modified_body = if is_html {
            inject_scripts_into_html_body(&body_bytes, &initialization_scripts)
              .unwrap_or(body_bytes)
          } else {
            body_bytes
          };

          let mut response = http::Response::from_parts(parts, Cursor::new(modified_body));


          let csp = response
            .headers_mut()
            .get_mut(CONTENT_SECURITY_POLICY);

          if let Some(csp) = csp {
            let csp_string = csp.to_str().unwrap().to_string();
            let new_csp = csp_inject_initialization_scripts_hashes(
              csp_string,
              &initialization_scripts,
            );
            *csp = HeaderValue::from_str(&new_csp).unwrap();
          }


          response_store.into_owned().borrow_mut().replace(response);

          let callback = callback.into_owned();
          callback.cont();
        });

        let label = self.webview_label.clone();
        let handler = self.handler.clone();

        let data = read_request_body(request);
        let headers = get_request_headers(request);
        let method_str = CefString::from(&request.method()).to_string();
        let method = http::Method::from_bytes(method_str.as_bytes())
          .unwrap_or(http::Method::GET);

        std::thread::spawn(move || {
          let mut http_request = http::Request::builder().method(method).uri(url.as_str()).body(data).unwrap();
          *http_request.headers_mut() = headers;
          // handler is Arc<Box<UriSchemeProtocol>>, so we need to dereference to call it
          (**handler)(&label, http_request, responder);
        });
        1
      } else {
        0
      }
    }

    fn read(
      &self,
      data_out: *mut u8,
      bytes_to_read: ::std::os::raw::c_int,
      bytes_read: Option<&mut ::std::os::raw::c_int>,
      _callback: Option<&mut ResourceReadCallback>,
    ) -> ::std::os::raw::c_int {
      let Ok(bytes_to_read) = usize::try_from(bytes_to_read) else {
        return 0;
      };
      let data_out = unsafe { std::slice::from_raw_parts_mut(data_out, bytes_to_read) };
      let count = self.response.borrow_mut().as_mut().and_then(|response| response.body_mut().read(data_out).ok()).unwrap_or(0);
      if let Some(bytes_read) = bytes_read {
        let Ok(count) = count.try_into() else {
          return 0;
        };
        *bytes_read = count;
        if count > 0 {
          return 1;
        }
      }
      0
    }

    fn response_headers(
      &self,
      response: Option<&mut Response>,
      response_length: Option<&mut i64>,
      redirect_url: Option<&mut CefString>,
    ) {
      let (Some(response), Some(response_data)) = (response, &*self.response.borrow()) else { return };

      response.set_status(response_data.status().as_u16() as i32);
      let mut content_type = None;

      // First pass: collect CSP header and set other headers
      for (name, value) in response_data.headers() {
        let Ok(value) = value.to_str() else { continue; };

        response.set_header_by_name(Some(&name.as_str().into()), Some(&value.into()), 0);

        if name == CONTENT_TYPE {
          content_type.replace(value.to_string());
        }
      }

      response.set_header_by_name(
        Some(&"Cache-Control".into()),
        Some(&"no-store".into()),
        1,
      );

      let mime_type = content_type
        .as_ref()
        .and_then(|t| t.split(';').next())
        .map(str::trim)
        .unwrap_or("text/plain");
      response.set_mime_type(Some(&mime_type.into()));

      if let Some(length) = response_length { *length = -1; }

      if let Some(redirect_url) = redirect_url {
        let _ = std::mem::take(redirect_url);
      }
    }
  }
}

wrap_scheme_handler_factory! {
  pub struct UriSchemeHandlerFactory<T: UserEvent> {
    context: Context<T>,
    scheme: String,
  }

  impl SchemeHandlerFactory {
    fn create(
      &self,
      browser: Option<&mut Browser>,
      _frame: Option<&mut Frame>,
      _scheme_name: Option<&CefString>,
      _request: Option<&mut Request>,
    ) -> Option<ResourceHandler> {
      let browser = browser?;
      let id = browser.identifier();

      // get handler from AppWebview - UriSchemeFactory can be overwritten
      // when registered on multiple RequestContexts sharing the same cache path
      let (webview_label, handler, initialization_scripts) = self.context.windows.borrow().values().find_map(|window| {
        window.webviews.iter().find(|webview| *webview.browser_id.borrow() == id)
        .and_then(|webview| {
          webview.uri_scheme_protocols.get(&self.scheme).map(|handler| {
            (webview.label.clone(), handler.clone(), webview.initialization_scripts.clone())
          })
        })
      })?;

      Some(WebResourceHandler::new(webview_label, handler, initialization_scripts, Arc::new(RefCell::new(None))))
    }
  }
}

struct ThreadSafe<T>(T);

impl<T> ThreadSafe<T> {
  fn into_owned(self) -> T {
    self.0
  }
}

unsafe impl<T> Send for ThreadSafe<T> {}
unsafe impl<T> Sync for ThreadSafe<T> {}

fn read_request_body(request: &mut Request) -> Vec<u8> {
  let mut body = Vec::new();

  if let Some(post_data) = request.post_data() {
    let mut elements = vec![None; post_data.element_count()];
    post_data.elements(Some(&mut elements));
    for element in elements.into_iter().flatten() {
      match element.get_type().as_ref() {
        sys::cef_postdataelement_type_t::PDE_TYPE_BYTES => {
          let size = element.bytes_count();
          if size > 0 {
            let mut buf = vec![0u8; size];
            // Copy bytes into our buffer
            let copied = element.bytes(size, buf.as_mut_ptr());
            // Safety: CEF promises it wrote `copied` bytes into buf
            unsafe {
              buf.set_len(copied);
            }
            body.extend(buf);
          }
        }
        sys::cef_postdataelement_type_t::PDE_TYPE_FILE => {
          // Read file from disk
          let file_path = CefString::from(&element.file()).to_string();
          if let Ok(mut file) = std::fs::File::open(&file_path) {
            use std::io::Read;
            let mut buf = Vec::new();
            if file.read_to_end(&mut buf).is_ok() {
              body.extend(buf);
            }
          }
        }
        _ => {}
      }
    }
  }

  body
}

fn get_request_headers(request: &mut Request) -> HeaderMap {
  let mut headers = HeaderMap::new();

  let mut map = CefStringMultimap::new();

  request.header_map(Some(&mut map));

  // Iterate through all entries
  for (name, value) in map {
    for v in value {
      headers.append(
        HeaderName::from_bytes(name.as_bytes()).unwrap(),
        HeaderValue::from_str(&v).unwrap(),
      );
    }
  }

  headers
}

#[cfg(test)]
mod renderer_recovery_tests {
  use super::*;

  #[test]
  fn recovery_requires_a_registered_initial_app_origin() {
    let domains = vec!["tauri.localhost".to_string()];
    let origin = renderer_recovery_origin(
      Some("http://tauri.localhost/shell.html"),
      &domains,
      "http",
    )
    .unwrap();
    assert_eq!(origin.host_str(), Some("tauri.localhost"));
    for initial in [
      None,
      Some("not a url"),
      Some("https://www.google.com/"),
      Some("http://localhost/"),
      Some("http://127.0.0.1/"),
      Some("http://other.localhost/"),
      Some("http://tauri.localhost.example.com/"),
      Some("https://tauri.localhost/"),
      Some("http://user@tauri.localhost/"),
    ] {
      assert!(
        renderer_recovery_origin(initial, &domains, "http").is_none(),
        "unexpected recovery origin for {initial:?}"
      );
    }
    assert!(renderer_recovery_origin(Some(origin.as_str()), &[], "http").is_none());
    assert!(renderer_recovery_origin(
      Some("https://tauri.localhost/"),
      &domains,
      "https"
    )
    .is_some());
  }

  #[test]
  fn recovery_rechecks_the_current_scheme_host_and_port() {
    let origin = Url::parse("http://tauri.localhost/shell.html").unwrap();
    assert!(matches_recovery_origin(
      &origin,
      "http://tauri.localhost:80/menu.html?panel=profile#details"
    ));
    for current in [
      "",
      "about:blank",
      "https://www.google.com/",
      "http://localhost/",
      "https://tauri.localhost/",
      "http://tauri.localhost:8080/",
      "http://tauri.localhost.example.com/",
      "http://user:password@tauri.localhost/",
    ] {
      assert!(
        !matches_recovery_origin(&origin, current),
        "must not reload navigation to {current}"
      );
    }
  }

  #[test]
  fn recovery_budget_expires_attempts_individually_after_sixty_seconds() {
    let mut state = RendererRecoveryState::default();
    let now = Instant::now();
    assert_eq!(state.reserve_attempt(now), Some(1));
    assert_eq!(state.reserve_attempt(now + Duration::from_secs(10)), Some(2));
    assert_eq!(state.reserve_attempt(now + Duration::from_secs(20)), Some(3));
    assert_eq!(state.reserve_attempt(now + Duration::from_secs(59)), None);
    assert_eq!(state.reserve_attempt(now + Duration::from_secs(60)), Some(3));
    assert_eq!(state.reserve_attempt(now + Duration::from_secs(69)), None);
    assert_eq!(state.reserve_attempt(now + Duration::from_secs(70)), Some(3));
    assert_eq!(state.reserve_attempt(now + Duration::from_secs(131)), Some(1));
  }

  #[test]
  fn recovery_budget_is_shared_by_handlers_but_independent_between_browsers() {
    let client_state = Arc::new(Mutex::new(RendererRecoveryState::default()));
    let now = Instant::now();
    for attempt in 1..=MAX_RENDERER_RECOVERIES {
      let handler_state = client_state.clone();
      assert_eq!(
        handler_state.lock().unwrap().reserve_attempt(now),
        Some(attempt)
      );
    }
    assert_eq!(client_state.lock().unwrap().reserve_attempt(now), None);
    assert_eq!(RendererRecoveryState::default().reserve_attempt(now), Some(1));
  }
}

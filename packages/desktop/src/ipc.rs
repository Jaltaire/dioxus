use serde::{Deserialize, Serialize};
use tao::window::WindowId;

/// The events the desktop renderer sends itself through the event loop.
///
/// An application sees them through `use_wry_event_handler`, and may act on
/// the ones that concern it -- a page lost to the platform, say -- while the
/// renderer does its own part.
#[non_exhaustive]
#[derive(Debug, Clone)]
#[allow(missing_docs)]
pub enum UserWindowEvent {
    /// A global hotkey event
    #[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
    GlobalHotKeyEvent(global_hotkey::GlobalHotKeyEvent),

    /// A menu bar event
    #[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
    MudaMenuEvent(muda::MenuEvent),

    /// A tray icon event
    #[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
    TrayIconEvent(tray_icon::TrayIconEvent),

    /// A tray menu event
    #[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
    TrayMenuEvent(tray_icon::menu::MenuEvent),

    /// Poll the shared VirtualDom.
    Poll,

    /// This window's page is lost - the process drawing it was terminated
    /// by the platform, the activity holding it was created again, or the
    /// page is on screen showing nothing of what the virtual dom holds - and
    /// has to be loaded again.
    PageLost(WindowId),

    /// The time a lost page was given to load again is up. If the page has
    /// still not reported in, this attempt failed, and another is made. The
    /// loss is named so that a clock started for an earlier loss of the same
    /// window says nothing about a later one.
    PageReloadDue {
        id: WindowId,
        loss: u64,
        attempt: u32,
    },

    /// Handle an ipc message eminating from the window.postMessage of a given webview
    Ipc {
        id: WindowId,
        msg: IpcMessage,
    },

    /// Handle a hotreload event, basically telling us to update our templates
    #[cfg(all(feature = "devtools", debug_assertions))]
    HotReloadEvent(dioxus_devtools::DevserverMsg),

    // Windows-only drag-n-drop fix events.
    WindowsDragDrop(WindowId),
    WindowsDragOver(WindowId, i32, i32),
    WindowsDragLeave(WindowId),

    /// Create a new window
    NewWindow,

    /// Request that a given window close, honoring its close behavior and component lifecycle.
    RequestWindowClose(WindowId),

    /// Destroy a native window after its Dioxus owner has released the portal.
    DestroyWindow(WindowId),

    /// Gracefully shutdown the entire app
    Shutdown,
}

/// A message struct that manages the communication between the webview and the eventloop code
///
/// This needs to be serializable across the JS boundary, so the method names and structs are sensitive.
#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct IpcMessage {
    method: String,
    params: serde_json::Value,
}

/// What a page says when it reports in: which page it is.
#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PageReport {
    pub(crate) page: crate::edits::Page,
}

/// A set of known messages that we need to respond to
#[derive(Deserialize, Serialize, Debug, Clone)]
pub enum IpcMethod<'a> {
    UserEvent,
    Query,
    BrowserOpen,
    Initialize,
    Other(&'a str),
}

impl IpcMessage {
    pub(crate) fn method(&self) -> IpcMethod<'_> {
        match self.method.as_str() {
            "user_event" => IpcMethod::UserEvent,
            "query" => IpcMethod::Query,
            "browser_open" => IpcMethod::BrowserOpen,
            "initialize" => IpcMethod::Initialize,
            _ => IpcMethod::Other(&self.method),
        }
    }

    pub(crate) fn params(self) -> serde_json::Value {
        self.params
    }

    pub(crate) fn page_event(self) -> Option<(crate::edits::Page, dioxus_html::HtmlEvent)> {
        let serde_json::Value::Object(mut fields) = self.params else {
            return None;
        };
        let page = serde_json::from_value(fields.remove("page")?).ok()?;
        let event = serde_json::from_value(serde_json::Value::Object(fields)).ok()?;
        Some((page, event))
    }
}

#[cfg(test)]
mod tests {
    use super::IpcMessage;
    use dioxus_html::EventData;
    use dioxus_vdom::ElementId;

    fn message(params: serde_json::Value) -> IpcMessage {
        serde_json::from_value(serde_json::json!({ "method": "user_event", "params": params }))
            .expect("A message is read from its method and parameters.")
    }

    #[test]
    fn an_event_that_names_its_page_is_read_with_that_page() {
        let (page, event) = message(serde_json::json!({
            "name": "mounted",
            "element": 5,
            "data": null,
            "bubbles": false,
            "page": 3,
        }))
        .page_event()
        .expect("An event naming its page is read.");
        assert_eq!(page.to_string(), "3");
        assert_eq!(event.name, "mounted");
        assert_eq!(event.element, ElementId::from_raw(5));
        assert!(!event.bubbles);
        assert!(matches!(event.data, EventData::Mounted));
    }

    #[test]
    fn a_registered_custom_event_that_names_its_page_is_read_with_its_detail() {
        dioxus_html::events::register_custom_event("ipc-notice", true);
        let (page, event) = message(serde_json::json!({
            "name": "ipc-notice",
            "element": 9,
            "data": { "detail": "hello" },
            "bubbles": true,
            "page": 4,
        }))
        .page_event()
        .expect("A registered custom event naming its page is read.");
        assert_eq!(page.to_string(), "4");
        assert_eq!(event.name, "ipc-notice");
        assert!(event.bubbles);
        assert_eq!(
            event.data,
            EventData::Custom(dioxus_html::SerializedCustomData::new(Some(
                "hello".to_string()
            )))
        );
    }

    #[test]
    fn an_event_that_does_not_name_its_page_is_not_read() {
        assert!(
            message(serde_json::json!({
                "name": "mounted",
                "element": 5,
                "data": null,
                "bubbles": false,
            }))
            .page_event()
            .is_none()
        );
    }

    #[test]
    fn parameters_that_are_not_an_object_hold_no_event() {
        assert!(
            message(serde_json::json!([3, "mounted"]))
                .page_event()
                .is_none()
        );
    }

    #[test]
    fn a_page_with_no_event_beside_it_holds_no_event() {
        assert!(
            message(serde_json::json!({ "page": 3, "element": 5 }))
                .page_event()
                .is_none()
        );
    }
}

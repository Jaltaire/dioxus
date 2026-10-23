use crate::WeakDesktopContext;
use crate::desktop_context::{PendingDesktopWindow, PendingWindowCancellation};
use crate::desktop_state::DesktopAppContext;
use crate::file_upload::{DesktopFileData, DesktopFileDragEvent};
use crate::menubar::DioxusMenu;
use crate::{
    DesktopContext, DesktopService, WindowConfig, assets::AssetHandlerRegistry, edits::WryQueue,
    file_upload::NativeFileHover, ipc::UserWindowEvent, protocol, protocol::PageAddress,
};
use crate::{element::DesktopElement, file_upload::DesktopFormData};
use base64::prelude::BASE64_STANDARD;
use dioxus_hooks::to_owned;
use dioxus_html::{FileData, FormValue, HtmlEvent, PlatformEventData, SerializedFileData};
use dioxus_vdom::{RenderTargetId, Runtime, VirtualDom};
use std::cell::{OnceCell, RefCell};
use std::collections::BTreeMap;
use std::rc::{Rc, Weak};
use std::sync::{Arc, atomic::AtomicBool};
use std::time::Duration;
use wry::{DragDropEvent, RequestAsyncResponder, WebContext, WebViewBuilder, WebViewId};

fn restore_window_state(
    window: &tao::window::Window,
    explicit_inner_size: Option<tao::dpi::Size>,
    explicit_window_position: Option<tao::dpi::Position>,
) {
    if cfg!(target_os = "android") || cfg!(target_os = "ios") || !cfg!(debug_assertions) {
        return;
    }

    let Ok(state) = std::fs::read_to_string(crate::app::restore_file()) else {
        return;
    };
    let Ok(state) = serde_json::from_str::<crate::app::PreservedWindowState>(&state) else {
        return;
    };

    let position = (state.x, state.y);
    let size = (state.width, state.height);

    if explicit_window_position.is_none() {
        if cfg!(target_os = "macos") {
            window.set_outer_position(tao::dpi::LogicalPosition::new(position.0, position.1));
        } else {
            window.set_outer_position(tao::dpi::PhysicalPosition::new(position.0, position.1));
        }
    }

    if explicit_inner_size.is_none() {
        if cfg!(target_os = "macos") {
            window.set_inner_size(tao::dpi::LogicalSize::new(size.0, size.1));
        } else {
            window.set_inner_size(tao::dpi::PhysicalSize::new(size.0, size.1));
        }
    }
}

#[derive(Clone)]
pub(crate) struct SharedDom {
    pub(crate) dom: Rc<RefCell<VirtualDom>>,
    pub(crate) queues: Rc<RefCell<BTreeMap<RenderTargetId, WryQueue>>>,
}

#[derive(Clone)]
pub(crate) struct WebviewEdits {
    runtime: Rc<Runtime>,
    target_id: RenderTargetId,
    pub wry_queue: WryQueue,
    desktop_context: Rc<OnceCell<WeakDesktopContext>>,
    dom: Weak<RefCell<VirtualDom>>,
    queues: Weak<RefCell<BTreeMap<RenderTargetId, WryQueue>>>,
}

const ANSWERED_IN_THEIR_FRAME: &[&str] = &["resize"];

pub(crate) fn answered_in_its_frame(name: &str) -> bool {
    ANSWERED_IN_THEIR_FRAME.contains(&name)
}

impl WebviewEdits {
    pub(crate) fn new(
        runtime: Rc<Runtime>,
        target_id: RenderTargetId,
        wry_queue: WryQueue,
        shared: &SharedDom,
    ) -> Self {
        Self {
            runtime,
            target_id,
            wry_queue,
            desktop_context: Default::default(),
            dom: Rc::downgrade(&shared.dom),
            queues: Rc::downgrade(&shared.queues),
        }
    }

    fn render_in_frame(&self) -> Option<String> {
        let dom = self.dom.upgrade()?;
        let queues = self.queues.upgrade()?;
        if !self.wry_queue.is_idle() {
            return None;
        }
        let mut writer: BTreeMap<RenderTargetId, WryQueue> = queues
            .try_borrow()
            .ok()?
            .iter()
            .map(|(target_id, queue)| (*target_id, queue.clone()))
            .collect();
        if writer
            .iter()
            .any(|(target_id, queue)| *target_id != self.target_id && queue.has_edits_in_flight())
        {
            return None;
        }
        let mut dom = dom.try_borrow_mut().ok()?;
        for queue in writer.values() {
            queue.clear_touched();
        }
        dom.render_immediate(&mut writer);
        for (target_id, queue) in &writer {
            if *target_id != self.target_id && queue.is_touched() {
                queue.send_edits();
            }
        }
        self.wry_queue.clear_touched();
        Some(base64::Engine::encode(
            &BASE64_STANDARD,
            self.wry_queue.take_rendered(),
        ))
    }

    pub fn handle_event(
        &self,
        request: wry::http::Request<Vec<u8>>,
        responder: crate::RequestAsyncResponder,
    ) {
        let body = self
            .try_handle_event(request)
            .expect("Writing bodies to succeed");
        responder.respond(wry::http::Response::new(body))
    }

    pub fn try_handle_event(
        &self,
        request: wry::http::Request<Vec<u8>>,
    ) -> Result<Vec<u8>, serde_json::Error> {
        use serde::de::Error;

        // todo(jon):
        //
        // I'm a small bit worried about the size of the header being too big on some platforms.
        // It's unlikely we'll hit the 256k limit (from 2010 browsers...) but it's important to think about
        // https://stackoverflow.com/questions/3326210/can-http-headers-be-too-big-for-browsers
        //
        // Also important to remember here that we don't pass a body from the JavaScript side of things
        let data = request
            .headers()
            .get("dioxus-data")
            .ok_or_else(|| Error::custom("dioxus-data header not set"))?;

        let as_utf = std::str::from_utf8(data.as_bytes())
            .map_err(|_| Error::custom("dioxus-data header is not a valid (utf-8) string"))?;

        let data_from_header = base64::Engine::decode(&BASE64_STANDARD, as_utf)
            .map_err(|_| Error::custom("dioxus-data header is not a base64 string"))?;

        let page = request
            .headers()
            .get(crate::edits::PAGE_HEADER)
            .and_then(|page| page.to_str().ok())
            .and_then(|page| page.parse::<crate::edits::Page>().ok());

        let response = match serde_json::from_slice(&data_from_header) {
            Ok(event) => {
                // we need to wait for the mutex lock to let us munge the main thread..
                #[cfg(target_os = "android")]
                let _lock = crate::android_sync_lock::android_runtime_lock();
                match page {
                    Some(page) if self.wry_queue.is_drawn(page) => self.handle_html_event(event),
                    Some(page) => {
                        tracing::info!(
                            "An event from page {page}, which is not the page being drawn into, \
                             is dropped."
                        );
                        SynchronousEventResponse::new(false)
                    }
                    None => {
                        tracing::warn!(
                            "An event that does not name the page it came from is dropped."
                        );
                        SynchronousEventResponse::new(false)
                    }
                }
            }
            Err(err) => {
                tracing::error!(
                    "Error parsing user_event: {:?}. \n Contents: {:?}, \nraw: {:#?}",
                    err,
                    String::from_utf8(request.body().to_vec()),
                    request
                );
                SynchronousEventResponse::new(false)
            }
        };

        serde_json::to_vec(&response).inspect_err(|err| {
            tracing::error!("failed to serialize SynchronousEventResponse: {err:?}");
        })
    }

    pub fn handle_html_event(&self, event: HtmlEvent) -> SynchronousEventResponse {
        let HtmlEvent {
            element,
            name,
            bubbles,
            data,
        } = event;
        let desktop_context = || {
            let Some(desktop_context) = self.desktop_context.get() else {
                tracing::error!(
                    "Tried to handle event before setting the desktop context on the event handler"
                );
                return None;
            };
            Some(desktop_context.upgrade().unwrap())
        };

        // check for a mounted event placeholder and replace it with a desktop specific element
        let as_any = match data {
            dioxus_html::EventData::Mounted => {
                let Some(desktop_context) = desktop_context() else {
                    return Default::default();
                };
                let query = desktop_context.query.clone();
                let element = DesktopElement::new(element, desktop_context, query);
                Rc::new(PlatformEventData::new(Box::new(element)))
            }
            dioxus_html::EventData::Form(form) => {
                Rc::new(PlatformEventData::new(Box::new(DesktopFormData {
                    value: form.value,
                    valid: form.valid,
                    values: form
                        .values
                        .into_iter()
                        .map(|obj| {
                            if let Some(text) = obj.text {
                                return (obj.key, FormValue::Text(text));
                            }

                            if let Some(file_data) = obj.file {
                                if file_data.path.capacity() == 0 {
                                    return (obj.key, FormValue::File(None));
                                }

                                return (
                                    obj.key,
                                    FormValue::File(Some(FileData::new(DesktopFileData(
                                        file_data.path,
                                    )))),
                                );
                            };

                            (obj.key, FormValue::Text(String::new()))
                        })
                        .collect(),
                })))
            }
            // Which also includes drops...
            dioxus_html::EventData::Drag(ref drag) => {
                let Some(desktop_context) = desktop_context() else {
                    return Default::default();
                };
                // we want to override this with a native file engine, provided by the most recent drag event
                let full_file_paths = desktop_context.file_hover.current_paths();

                let xfer_data = drag.data_transfer.clone();
                let new_file_data = xfer_data
                    .files
                    .iter()
                    .map(|f| {
                        let new_path = full_file_paths
                            .iter()
                            .find(|p| p.ends_with(&f.path))
                            .unwrap_or(&f.path);
                        SerializedFileData {
                            path: new_path.clone(),
                            ..f.clone()
                        }
                    })
                    .collect::<Vec<_>>();
                let new_xfer_data = dioxus_html::SerializedDataTransfer {
                    files: new_file_data,
                    ..xfer_data
                };

                Rc::new(PlatformEventData::new(Box::new(DesktopFileDragEvent {
                    mouse: drag.mouse.clone(),
                    data_transfer: new_xfer_data,
                    files: full_file_paths,
                })))
            }
            _ => data.into_any(),
        };

        let event = dioxus_vdom::Event::new(as_any, bubbles);
        self.runtime
            .handle_event_for_target(self.target_id, &name, event.clone(), element);

        // Get the response from the event
        SynchronousEventResponse {
            prevent_default: !event.default_action_enabled(),
            edits: answered_in_its_frame(&name)
                .then(|| self.render_in_frame())
                .flatten(),
        }
    }
}

/// A custom protocol handler marked as `Send + Sync`, which wry requires since 0.56.
///
/// Our handlers hold `Rc`s and are not thread safe. Wry only calls them on the thread that created
/// the webview, except on Android where they run on WebView worker threads. Wry used to hold a
/// mutex around those calls and its removal of the handler, and since 0.56 no longer does, so
/// `with` and `drop` take a lock of our own there.
struct ProtocolHandler<F> {
    handler: std::mem::ManuallyDrop<F>,
    #[cfg(not(target_os = "android"))]
    created_on: std::thread::ThreadId,
}

// SAFETY: upheld by the caller of `new`. `with` checks the calling thread, or locks on Android,
// where `drop` locks too.
unsafe impl<F> Send for ProtocolHandler<F> {}
unsafe impl<F> Sync for ProtocolHandler<F> {}

impl<F> ProtocolHandler<F> {
    /// # Safety
    ///
    /// `handler` must only be handed to wry as a custom protocol handler, from the thread that
    /// creates the webview, and never be called on another thread except through `with`.
    unsafe fn new(handler: F) -> Self {
        Self {
            handler: std::mem::ManuallyDrop::new(handler),
            #[cfg(not(target_os = "android"))]
            created_on: std::thread::current().id(),
        }
    }

    fn with<R>(&self, f: impl FnOnce(&F) -> R) -> R {
        #[cfg(not(target_os = "android"))]
        assert_eq!(
            self.created_on,
            std::thread::current().id(),
            "custom protocol handler called off the thread that created the webview"
        );

        #[cfg(target_os = "android")]
        let _lock = android_handler_lock();

        f(&self.handler)
    }
}

impl<F> Drop for ProtocolHandler<F> {
    fn drop(&mut self) {
        // Wry drops the handler on the activity's thread or a WebView worker thread
        #[cfg(target_os = "android")]
        let _lock = android_handler_lock();

        // SAFETY: the handler is not used again
        unsafe { std::mem::ManuallyDrop::drop(&mut self.handler) }
    }
}

/// Serializes calls to and drops of all protocol handlers.
///
/// Not `android_runtime_lock`: handlers take that themselves.
#[cfg(target_os = "android")]
fn android_handler_lock() -> std::sync::MutexGuard<'static, ()> {
    static HANDLER_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    HANDLER_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

pub(crate) struct WebviewInstance {
    pub edits: WebviewEdits,
    pub desktop_context: DesktopContext,

    // Wry assumes the webcontext is alive for the lifetime of the webview.
    // We need to keep the webcontext alive, otherwise the webview will crash
    _web_context: WebContext,

    // Same with the menu.
    // Currently it's a DioxusMenu because 1) we don't touch it and 2) we support a number of platforms
    // like ios where muda does not give us a menu type. It sucks but alas.
    //
    // This would be a good thing for someone looking to contribute to fix.
    _menu: Option<DioxusMenu>,
}

impl WebviewInstance {
    /// The render target this webview draws. Stored once in [`WebviewEdits`];
    /// this exposes it as the webview's identity to the app layer.
    pub(crate) fn target_id(&self) -> RenderTargetId {
        self.desktop_context.target_id
    }

    pub(crate) fn new(
        mut cfg: WindowConfig,
        target_id: RenderTargetId,
        shared: &SharedDom,
        app_context: Rc<DesktopAppContext>,
    ) -> WebviewInstance {
        let mut window = cfg.window.clone();
        let explicit_window_size = cfg.window.window.inner_size;
        let explicit_window_position = cfg.window.window.position;

        // tao makes small windows for some reason, make them bigger on desktop
        //
        // on mobile, we want them to be `None` so tao makes them the size of the screen. Otherwise we
        // get a window that is not the size of the screen and weird black bars.
        #[cfg(not(any(target_os = "ios", target_os = "android")))]
        {
            if cfg.window.window.inner_size.is_none() {
                window = window.with_inner_size(tao::dpi::LogicalSize::new(800.0, 600.0));
            }
        }

        // We assume that if the icon is None in cfg, then the user just didnt set it
        if cfg.window.window.window_icon.is_none() {
            window = window.with_window_icon(crate::default_icon().ok());
        }

        let window = Arc::new(window.build(&app_context.target).unwrap());
        restore_window_state(&window, explicit_window_size, explicit_window_position);
        if let Some(on_build) = cfg.on_window.as_mut() {
            on_build(window.clone(), &mut shared.dom.borrow_mut());
        }

        // https://developer.apple.com/documentation/appkit/nswindowcollectionbehavior/nswindowcollectionbehaviormanaged
        #[cfg(target_os = "macos")]
        {
            use objc2::rc::Retained;
            use objc2_app_kit::{NSWindow, NSWindowCollectionBehavior};
            use tao::platform::macos::WindowExtMacOS;
            let ns_window: Retained<NSWindow> =
                unsafe { Retained::retain(window.ns_window().cast()) }.unwrap();
            ns_window.setCollectionBehavior(NSWindowCollectionBehavior::Managed)
        }

        let mut web_context = WebContext::new(cfg.data_dir.clone().or_else(|| {
            // On Windows, WebView2 defaults to storing its data next to the executable.
            // This fails on certain drives (e.g. ReFS dev drives, Program Files) where the
            // directory may not be writable. Fall back to %LOCALAPPDATA%/<exe_name> automatically.
            if cfg!(windows) {
                let exe = std::env::current_exe().ok()?;
                let name = exe.file_stem()?.to_str()?;
                let local_app_data = std::env::var("LOCALAPPDATA").ok()?;
                Some(std::path::PathBuf::from(local_app_data).join(name))
            } else {
                None
            }
        }));
        let edit_queue = app_context.websocket.create_queue();
        let asset_handlers = AssetHandlerRegistry::new();
        let edits = WebviewEdits::new(
            shared.dom.borrow().runtime(),
            target_id,
            edit_queue.clone(),
            shared,
        );
        let file_hover = NativeFileHover::default();
        let headless = !cfg.window.window.visible;

        // Set when the page's own document is asked for, which the platform
        // does only once the process that will draw it is up and loading: the
        // one sign that a load of the page is truly under way, as opposed to
        // asked for. Cleared when the page is lost.
        let page_begun = Arc::new(AtomicBool::new(false));

        let request_handler = {
            to_owned![
                cfg.custom_head,
                cfg.custom_index,
                cfg.root_name,
                app_context.resource_response_policy,
                asset_handlers,
                edits
            ];
            let page_begun = page_begun.clone();

            #[cfg(feature = "tokio_runtime")]
            let tokio_rt = tokio::runtime::Handle::current();

            let handler = move |request: wry::http::Request<Vec<u8>>,
                                responder: RequestAsyncResponder| {
                if request.uri().path() == "/" {
                    page_begun.store(true, std::sync::atomic::Ordering::SeqCst);
                }

                protocol::desktop_handler(
                    request,
                    asset_handlers.clone(),
                    crate::RequestAsyncResponder::new(move |response| responder.respond(response)),
                    &edits,
                    custom_head.clone(),
                    custom_index.clone(),
                    &root_name,
                    headless,
                    resource_response_policy.clone(),
                )
            };
            // SAFETY: only moved into the protocol handler passed to wry below
            let handler = unsafe { ProtocolHandler::new(handler) };

            move |_id: WebViewId, request, responder: RequestAsyncResponder| {
                #[cfg(feature = "tokio_runtime")]
                let _guard = tokio_rt.enter();

                handler.with(|handler| handler(request, responder))
            }
        };

        let ipc_handler = {
            let window_id = window.id();
            to_owned![app_context.proxy];
            move |payload: wry::http::Request<String>| {
                // defer the event to the main thread
                let body = payload.into_body();
                if let Ok(msg) = serde_json::from_str(&body) {
                    _ = proxy.send_event(UserWindowEvent::Ipc { id: window_id, msg });
                }
            }
        };

        let file_drop_handler = {
            to_owned![file_hover];
            let (proxy, window_id) = (app_context.proxy.to_owned(), window.id());
            move |evt: DragDropEvent| {
                if cfg!(not(windows)) {
                    // Update the most recent file drop event - when the event comes in from the webview we can use the
                    // most recent event to build a new event with the files in it.
                    file_hover.set(evt);
                } else {
                    // Windows webview blocks HTML-native events when the drop handler is provided.
                    // The problem is that the HTML-native events don't provide the file, so we need this.
                    // Solution: this glue code to mimic drag drop events.
                    file_hover.set(evt.clone());
                    match evt {
                        wry::DragDropEvent::Drop {
                            paths: _,
                            position: _,
                        } => {
                            _ = proxy.send_event(UserWindowEvent::WindowsDragDrop(window_id));
                        }
                        wry::DragDropEvent::Over { position } => {
                            _ = proxy.send_event(UserWindowEvent::WindowsDragOver(
                                window_id, position.0, position.1,
                            ));
                        }
                        wry::DragDropEvent::Leave => {
                            _ = proxy.send_event(UserWindowEvent::WindowsDragLeave(window_id));
                        }
                        _ => {}
                    }
                }

                false
            }
        };

        let navigation_handler = cfg.navigation_handler.take();
        // Whether the page is up and talking to us.
        //
        // Shared rather than owned by the navigation handler alone, because the
        // web content process can die underneath a running application -- iOS
        // reclaims it while the app is in the background -- and when it does,
        // the page this guards is already gone.
        let page_loaded = Arc::new(AtomicBool::new(false));

        // Set from the moment the page is lost until its replacement reports
        // in. While it is set, every navigation to the page is allowed: the
        // platform reloads the page on its own after taking its process, the
        // application asks for it too, and a load refused in that window
        // leaves the frame holding a document with none of the page's
        // scripts in it -- one that can never report in, however many times
        // it is asked for.
        let page_awaited = Arc::new(AtomicBool::new(false));

        let mut webview = WebViewBuilder::new_with_web_context(&mut web_context)
            .with_bounds(wry::Rect {
                position: wry::dpi::Position::Logical(wry::dpi::LogicalPosition::new(0.0, 0.0)),
                size: wry::dpi::Size::Physical(wry::dpi::PhysicalSize::new(
                    window.inner_size().width,
                    window.inner_size().height,
                )),
            })
            .with_transparent(cfg.window.window.transparent)
            .with_url(PageAddress::CustomScheme.index())
            .with_ipc_handler(ipc_handler)
            .with_navigation_handler({
                let page_loaded = page_loaded.clone();
                let page_awaited = page_awaited.clone();
                move |var: String| {
                    // Serve the index and assets.
                    if PageAddress::serves(&var) {
                        // After the page has loaded once, don't allow any more navigation
                        let page_loaded =
                            page_loaded.swap(true, std::sync::atomic::Ordering::SeqCst);
                        return !page_loaded
                            || page_awaited.load(std::sync::atomic::Ordering::SeqCst);
                    }

                    // External links always open somewhere else. Prevents the webview from navigating
                    if var.starts_with("http://")
                        || var.starts_with("https://")
                        || var.starts_with("mailto:")
                    {
                        _ = webbrowser::open(&var);
                        return false;
                    }

                    // By default, external links are allowed. This keeps things like iframes working.
                    // However, users can customize this to allow/disallow domains/routes/patterns.
                    navigation_handler.as_ref().map(|f| f(&var)).unwrap_or(true)
                }
            })
            .with_asynchronous_custom_protocol(String::from("dioxus"), request_handler);

        // The web content process is not ours to keep. iOS reclaims it from a
        // backgrounded application, and when the application is opened again
        // the view is still there with nothing in it -- which the platform
        // itself tries to put right by reloading the page.
        //
        // That reload is a navigation to `dioxus://`, and the guard above
        // refuses every one of those after the first, so the reload is
        // cancelled and the application is left showing an empty webview until
        // it is force quit. Clearing the flag is the whole of the fix: the page
        // it was guarding no longer exists, so there is nothing left to guard,
        // and the next load is allowed exactly once more.
        #[cfg(any(target_os = "ios", target_os = "macos"))]
        {
            use wry::WebViewBuilderExtDarwin;

            let page_loaded = page_loaded.clone();
            let page_awaited = page_awaited.clone();
            let (proxy, window_id) = (app_context.proxy.to_owned(), window.id());
            webview = webview.with_on_web_content_process_terminate_handler(move || {
                tracing::warn!(
                    "The web content process was terminated. The page will be loaded again so \
                     the application can come back."
                );
                // Cleared before the reload is asked for, because the guard
                // would refuse the very navigation being asked for.
                page_awaited.store(true, std::sync::atomic::Ordering::SeqCst);
                page_loaded.store(false, std::sync::atomic::Ordering::SeqCst);
                _ = proxy.send_event(UserWindowEvent::PageLost(window_id));
            });
        };

        // Enable https scheme on android, needed for secure context API, like the geolocation API
        #[cfg(target_os = "android")]
        {
            use wry::WebViewBuilderExtAndroid as _;

            webview = webview.with_https_scheme(true);

            let page_loaded = page_loaded.clone();
            let page_awaited = page_awaited.clone();
            let (proxy, window_id) = (app_context.proxy.to_owned(), window.id());
            webview = webview.with_on_page_lost_handler(move |lost| {
                match lost {
                    wry::PageLost::RenderProcessCrashed => tracing::warn!(
                        "The web view's renderer process crashed. The page will be loaded again so \
                         the application can come back."
                    ),
                    wry::PageLost::RenderProcessKilled => tracing::warn!(
                        "The web view's renderer process was ended by the system. The page will be \
                         loaded again so the application can come back."
                    ),
                    wry::PageLost::ActivityRecreated => tracing::info!(
                        "The activity was created again, and its web view with it. The page will \
                         be loaded again so the application can come back."
                    ),
                }
                page_awaited.store(true, std::sync::atomic::Ordering::SeqCst);
                page_loaded.store(false, std::sync::atomic::Ordering::SeqCst);
                _ = proxy.send_event(UserWindowEvent::PageLost(window_id));
            });
        };

        // Disable the webview default shortcuts to disable the reload shortcut
        #[cfg(target_os = "windows")]
        {
            use wry::WebViewBuilderExtWindows;
            webview = webview.with_browser_accelerator_keys(false);
        }

        if !cfg.disable_file_drop_handler {
            webview = webview.with_drag_drop_handler(file_drop_handler);
        }

        if let Some(color) = cfg.background_color {
            webview = webview.with_background_color(color);
        }

        if let Some(policy) = cfg.background_throttling.clone() {
            webview = webview.with_background_throttling(policy);
        }

        for (name, handler) in cfg.protocols.drain(..) {
            #[cfg(feature = "tokio_runtime")]
            let tokio_rt = tokio::runtime::Handle::current();

            // SAFETY: only moved into the protocol handler passed to wry below
            let handler = unsafe { ProtocolHandler::new(handler) };
            webview = webview.with_custom_protocol(name, move |a, b| {
                #[cfg(feature = "tokio_runtime")]
                let _guard = tokio_rt.enter();
                handler.with(|handler| handler(a, b))
            });
        }

        for (name, handler) in cfg.asynchronous_protocols.drain(..) {
            #[cfg(feature = "tokio_runtime")]
            let tokio_rt = tokio::runtime::Handle::current();

            // SAFETY: only moved into the protocol handler passed to wry below
            let handler = unsafe { ProtocolHandler::new(handler) };
            webview = webview.with_asynchronous_custom_protocol(name, move |a, b, c| {
                #[cfg(feature = "tokio_runtime")]
                let _guard = tokio_rt.enter();
                handler.with(|handler| handler(a, b, c))
            });
        }

        const INITIALIZATION_SCRIPT: &str = r#"
        if (document.addEventListener) {
            document.addEventListener('contextmenu', function(e) {
                e.preventDefault();
            }, false);
        } else {
            document.attachEvent('oncontextmenu', function() {
                window.event.returnValue = false;
            });
        }
        "#;

        if cfg.disable_context_menu {
            // in release mode, we don't want to show the dev tool or reload menus
            webview = webview.with_initialization_script(INITIALIZATION_SCRIPT)
        } else {
            // in debug, we are okay with the reload menu showing and dev tool
            webview = webview.with_devtools(true);
        }

        let menu = if cfg!(not(any(target_os = "android", target_os = "ios"))) {
            let menu_option = cfg.menu.into();
            if let Some(menu) = &menu_option {
                crate::menubar::init_menu_bar(menu, &window);
            }
            menu_option
        } else {
            None
        };

        #[cfg(target_os = "windows")]
        {
            use wry::WebViewBuilderExtWindows;
            if let Some(additional_windows_args) = &cfg.additional_windows_args {
                webview = webview.with_additional_browser_args(additional_windows_args);
            }
        }

        #[cfg(any(
            target_os = "windows",
            target_os = "macos",
            target_os = "ios",
            target_os = "android"
        ))]
        let webview = if cfg.as_child_window {
            webview.build_as_child(&window)
        } else {
            webview.build(&window)
        };

        #[cfg(not(any(
            target_os = "windows",
            target_os = "macos",
            target_os = "ios",
            target_os = "android"
        )))]
        let webview = {
            use tao::platform::unix::WindowExtUnix;
            use wry::WebViewBuilderExtUnix;
            let vbox = window.default_vbox().unwrap();
            webview.build_gtk(vbox)
        };
        let webview = webview.unwrap();

        let desktop_context = Rc::from(DesktopService::new(
            webview,
            window,
            app_context.clone(),
            target_id,
            asset_handlers,
            file_hover,
            cfg.window_close_behavior,
            page_loaded,
            page_awaited,
            page_begun,
        ));

        // Provide the desktop context to the edit handler.
        _ = edits.desktop_context.set(Rc::downgrade(&desktop_context));

        // Request an initial redraw
        desktop_context.window.request_redraw();

        WebviewInstance {
            edits,
            desktop_context,
            _menu: menu,
            _web_context: web_context,
        }
    }

    #[cfg(all(feature = "devtools", debug_assertions))]
    pub fn kick_stylsheets(&self) {
        // run eval in the webview to kick the stylesheets by appending a query string
        // we should do something less clunky than this
        _ = self
            .desktop_context
            .webview
            .evaluate_script("window.interpreter.kickAllStylesheetsOnPage()");
    }

    /// Displays a toast to the developer.
    pub(crate) fn show_toast(
        &self,
        header_text: &str,
        message: &str,
        level: &str,
        duration: Duration,
        after_reload: bool,
    ) {
        let as_ms = duration.as_millis();

        let js_fn_name = match after_reload {
            true => "scheduleDXToast",
            false => "showDXToast",
        };

        _ = self.desktop_context.webview.evaluate_script(&format!(
            r#"
                if (typeof {js_fn_name} !== "undefined") {{
                    window.{js_fn_name}("{header_text}", "{message}", "{level}", {as_ms});
                }}
                "#,
        ));
    }
}

/// A synchronous response to a browser event which may prevent the default browser's action
#[derive(serde::Serialize, Default)]
pub struct SynchronousEventResponse {
    #[serde(rename = "preventDefault")]
    prevent_default: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    edits: Option<String>,
}

impl SynchronousEventResponse {
    /// Create a new SynchronousEventResponse
    #[allow(unused)]
    pub fn new(prevent_default: bool) -> Self {
        Self {
            prevent_default,
            edits: None,
        }
    }
}

/// A webview that is queued to be created. We can't spawn webviews outside of the main event loop because it may
/// block on windows, so the app context queues them until the main event loop is ready.
pub(crate) struct PendingWebview {
    target_id: RenderTargetId,
    cfg: WindowConfig,
    sender: futures_channel::oneshot::Sender<DesktopContext>,
}

impl PendingWebview {
    pub(crate) fn new(
        target_id: RenderTargetId,
        cfg: WindowConfig,
    ) -> (Self, PendingDesktopWindow) {
        let (sender, receiver) = futures_channel::oneshot::channel();
        let webview = Self {
            target_id,
            cfg,
            sender,
        };
        let pending = PendingDesktopWindow {
            target_id,
            receiver,
            cancellation: PendingWindowCancellation::default(),
        };
        (webview, pending)
    }

    pub(crate) fn create_window(
        self,
        shared: &SharedDom,
        app_context: &Rc<DesktopAppContext>,
    ) -> WebviewInstance {
        let window = WebviewInstance::new(self.cfg, self.target_id, shared, app_context.clone());
        shared
            .queues
            .borrow_mut()
            .insert(self.target_id, window.edits.wry_queue.clone());

        _ = self.sender.send(window.desktop_context.clone());

        window
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dioxus_vdom::{Element, VNode};
    use futures_util::FutureExt;

    fn empty_app() -> Element {
        VNode::empty()
    }

    #[test]
    fn canceling_pending_webview_cancels_receiver_before_reusing_target() {
        let dom = VirtualDom::new(empty_app);

        dom.in_runtime(|| {
            let target_id = Runtime::current().create_render_target();
            let (pending_webview, pending_window) =
                PendingWebview::new(target_id, WindowConfig::new());
            let cancellation = pending_window.cancellation();

            cancellation.cancel();
            assert!(cancellation.is_canceled());

            drop(pending_webview);

            assert!(Runtime::current().remove_render_target(target_id));
            assert_eq!(Runtime::current().create_render_target(), target_id);
            assert!(
                pending_window
                    .try_resolve()
                    .now_or_never()
                    .expect("dropped pending webview should cancel immediately")
                    .is_err()
            );
        });
    }

    #[test]
    fn a_resize_is_answered_in_its_frame() {
        assert!(answered_in_its_frame("resize"));
    }

    #[test]
    fn every_other_event_is_answered_the_usual_way() {
        for name in [
            "click",
            "scroll",
            "touchstart",
            "touchmove",
            "input",
            "mounted",
            "visible",
            "resized",
            "Resize",
            "",
        ] {
            assert!(!answered_in_its_frame(name), "{name}");
        }
    }

    #[test]
    fn a_response_without_edits_says_nothing_of_them() {
        let response = serde_json::to_value(SynchronousEventResponse::new(true)).unwrap();
        assert_eq!(response, serde_json::json!({ "preventDefault": true }));
    }

    #[test]
    fn a_response_with_edits_carries_them_beside_the_default_action() {
        let response = SynchronousEventResponse {
            prevent_default: false,
            edits: Some("AAEC".to_owned()),
        };
        assert_eq!(
            serde_json::to_value(response).unwrap(),
            serde_json::json!({ "preventDefault": false, "edits": "AAEC" })
        );
    }
}

#[cfg(test)]
mod page_tests {
    use super::*;
    use crate::edits::{Arrival, EditWebsocket, PAGE_HEADER, Page};
    use dioxus::prelude::*;
    use dioxus_html::{SerializedResizeData, geometry::PixelsSize};
    use dioxus_vdom::{ElementId, Mutation, Mutations};
    use std::cell::Cell;

    thread_local! {
        static HEARD: Cell<usize> = const { Cell::new(0) };
    }

    fn measured() -> Element {
        let mut sized = use_signal(|| 0);
        rsx! {
            div {
                onresize: move |_| {
                    HEARD.with(|heard| heard.set(heard.get() + 1));
                    sized += 1;
                },
                "{sized}"
            }
        }
    }

    struct Scene {
        shared: SharedDom,
        edits: WebviewEdits,
    }

    impl Scene {
        fn new() -> Self {
            HEARD.with(|heard| heard.set(0));
            dioxus_html::set_event_converter(Box::new(crate::events::SerializedHtmlEventConverter));
            let shared = SharedDom {
                dom: Rc::new(RefCell::new(VirtualDom::new(measured))),
                queues: Default::default(),
            };
            let queue = EditWebsocket::start().create_queue();
            shared
                .queues
                .borrow_mut()
                .insert(RenderTargetId::ROOT, queue.clone());
            let edits = WebviewEdits::new(
                shared.dom.borrow().runtime(),
                RenderTargetId::ROOT,
                queue,
                &shared,
            );
            Self { shared, edits }
        }

        fn draw(&self, page: Page) -> ElementId {
            assert_eq!(self.edits.wry_queue.page_arrived(page), Arrival::Drawn);
            let mut mutations = Mutations::default();
            self.shared
                .dom
                .borrow_mut()
                .rebuild_into_a_new_page(&mut mutations);
            let mut top = None;
            for mutation in &mutations.edits {
                match mutation {
                    Mutation::SetId { id } | Mutation::PushId { id } => top = Some(*id),
                    Mutation::NewEventListener { name } if name == "resize" => {
                        return top.expect("The element listening for its size has an id.");
                    }
                    _ => {}
                }
            }
            panic!("The page listens for its size.")
        }

        fn resize(&self, element: ElementId, page: Option<Page>) -> serde_json::Value {
            let event = serde_json::json!({
                "name": "resize",
                "element": element,
                "bubbles": false,
                "data": SerializedResizeData::new(
                    PixelsSize::new(320.0, 640.0),
                    PixelsSize::new(320.0, 640.0),
                ),
            });
            let mut request = wry::http::Request::builder()
                .uri(format!(
                    "{}__events",
                    crate::protocol::PageAddress::THIS_PLATFORM.index()
                ))
                .header(
                    "dioxus-data",
                    base64::Engine::encode(&BASE64_STANDARD, event.to_string()),
                );
            if let Some(page) = page {
                request = request.header(PAGE_HEADER, page.to_string());
            }
            let body = self
                .edits
                .try_handle_event(request.body(Vec::new()).unwrap())
                .unwrap();
            serde_json::from_slice(&body).unwrap()
        }
    }

    fn heard() -> usize {
        HEARD.with(Cell::get)
    }

    fn answered_with_edits(response: &serde_json::Value) -> bool {
        response
            .get("edits")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|edits| !edits.is_empty())
    }

    #[test]
    fn an_event_from_the_page_drawn_into_is_handled_and_answered_in_its_frame() {
        let scene = Scene::new();
        let page = scene.edits.wry_queue.serve_page();
        let element = scene.draw(page);
        let response = scene.resize(element, Some(page));
        assert_eq!(heard(), 1);
        assert!(answered_with_edits(&response), "{response}");
    }

    #[test]
    fn an_event_from_a_replaced_page_never_reaches_the_new_pages_tree_or_takes_its_edits() {
        let scene = Scene::new();
        let first = scene.edits.wry_queue.serve_page();
        let old_element = scene.draw(first);
        assert!(answered_with_edits(&scene.resize(old_element, Some(first))));
        assert_eq!(heard(), 1);

        let second = scene.edits.wry_queue.serve_page();
        let element = scene.draw(second);
        let response = scene.resize(element, Some(first));
        assert_eq!(
            heard(),
            1,
            "The replaced page's event ran a handler in the new page's tree."
        );
        assert_eq!(response, serde_json::json!({ "preventDefault": false }));

        let response = scene.resize(element, Some(second));
        assert_eq!(heard(), 2);
        assert!(answered_with_edits(&response), "{response}");
    }

    #[test]
    fn an_event_from_a_page_that_has_not_reported_in_is_dropped() {
        let scene = Scene::new();
        let first = scene.edits.wry_queue.serve_page();
        let element = scene.draw(first);
        let arriving = scene.edits.wry_queue.serve_page();
        let response = scene.resize(element, Some(arriving));
        assert_eq!(heard(), 0);
        assert_eq!(response, serde_json::json!({ "preventDefault": false }));
    }

    #[test]
    fn an_event_from_a_lost_page_is_dropped() {
        let scene = Scene::new();
        let page = scene.edits.wry_queue.serve_page();
        let element = scene.draw(page);
        scene.edits.wry_queue.forget_connection();
        let response = scene.resize(element, Some(page));
        assert_eq!(heard(), 0);
        assert_eq!(response, serde_json::json!({ "preventDefault": false }));
    }

    #[test]
    fn an_event_that_does_not_name_its_page_is_dropped() {
        let scene = Scene::new();
        let page = scene.edits.wry_queue.serve_page();
        let element = scene.draw(page);
        let response = scene.resize(element, None);
        assert_eq!(heard(), 0);
        assert_eq!(response, serde_json::json!({ "preventDefault": false }));
    }
}

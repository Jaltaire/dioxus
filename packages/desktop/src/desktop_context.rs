use crate::RequestAsyncResponder;
use crate::{
    AssetRequest, WindowCloseBehaviour, WindowConfig, WryEventHandler,
    assets::AssetHandlerRegistry,
    desktop_state::{DesktopAppContext, DesktopWindowContext, WindowCloseRequestResult},
    file_upload::NativeFileHover,
    head_record::{HeadInsertion, HeadRecord},
    ipc::UserWindowEvent,
    shortcut::{HotKey, HotKeyState, ShortcutHandle, ShortcutRegistryError},
};
use dioxus_vdom::{Callback, RenderTargetId};
use std::cell::RefCell;
use std::{
    cell::Cell,
    future::{Future, IntoFuture},
    pin::Pin,
    rc::{Rc, Weak},
    sync::Arc,
};
use tao::{
    event::Event,
    event_loop::EventLoopWindowTarget,
    window::{Fullscreen as WryFullscreen, Window, WindowId},
};
use wry::WebView;

#[cfg(target_os = "ios")]
use objc2::rc::Retained;
#[cfg(target_os = "ios")]
use objc2_ui_kit::UIView;
#[cfg(target_os = "ios")]
use tao::platform::ios::WindowExtIOS;

/// Get an imperative handle to the current window without using a hook
///
/// ## Panics
///
/// This function will panic if it is called outside of the context of a Dioxus App.
pub fn window() -> DesktopContext {
    dioxus_vdom::consume_context()
}

/// Get an imperative handle to the desktop application without using a hook.
///
/// This is available in the root scope, including apps launched through the raw `VirtualDom` API.
///
/// ## Panics
///
/// This function will panic if it is called outside of the context of a Dioxus App.
pub fn app() -> Rc<DesktopAppContext> {
    dioxus_vdom::consume_context()
}

/// A handle to the [`DesktopService`] that can be passed around.
pub type DesktopContext = Rc<DesktopService>;

/// A weak handle to the [`DesktopService`] to ensure safe passing.
/// The problem without this is that the tao window is never dropped and therefore cannot be closed.
/// This was due to the Rc that had still references because of multiple copies when creating a webview.
pub type WeakDesktopContext = Weak<DesktopService>;

/// Registration for a component-owned desktop window.
pub(crate) struct ComponentWindowRegistration {
    window: WeakDesktopContext,
}

impl Drop for ComponentWindowRegistration {
    fn drop(&mut self) {
        if let Some(window) = self.window.upgrade() {
            window.window_context.clear_component_window_callbacks();
        }
    }
}

/// Shared cancellation state for a queued desktop window.
#[derive(Clone, Default)]
pub(crate) struct PendingWindowCancellation {
    canceled: Rc<Cell<bool>>,
}

impl PendingWindowCancellation {
    /// Mark the queued window as canceled.
    pub(crate) fn cancel(&self) {
        self.canceled.set(true);
    }

    /// Returns true if the queued window was canceled before creation.
    pub(crate) fn is_canceled(&self) -> bool {
        self.canceled.get()
    }
}

/// An imperative interface to the current window.
///
/// To get a handle to the current window, use the [`window`] function.
///
///
/// # Example
///
/// you can use `cx.consume_context::<DesktopContext>` to get this context
///
/// ```rust, ignore
///     let desktop = cx.consume_context::<DesktopContext>().unwrap();
/// ```
pub struct DesktopService {
    pub(crate) app: Rc<DesktopAppContext>,
    window_context: DesktopWindowContext,

    /// Whether the window's page has been loaded once. The navigation guard
    /// refuses every load of the page after the first, so that a link in the
    /// page cannot navigate the window away; a page that is lost is loaded
    /// again by clearing this first.
    pub(crate) page_loaded: Arc<std::sync::atomic::AtomicBool>,

    /// Whether the window's page is gone and its replacement has yet to
    /// report in. While it is, every load of the page is allowed, not just
    /// the first: see the navigation guard.
    pub(crate) page_awaited: Arc<std::sync::atomic::AtomicBool>,

    /// Whether the page's document has been asked for since the page was
    /// last lost: a load that is under way, not merely asked for.
    pub(crate) page_begun: Arc<std::sync::atomic::AtomicBool>,

    /// The head elements this window's document has put into its page.
    ///
    /// Kept because a page can be replaced underneath a running application --
    /// the platform terminates the process drawing it and it is loaded again --
    /// and the replacement must receive these elements before remounted effects
    /// run. Without this record, the new page can initially have an empty head
    /// and render without styling. Every replacement receives all of them, not
    /// only the first: a remounted component that de-duplicates what it puts
    /// into the head, as a stylesheet link does, never puts it in again.
    pub(crate) head_elements: RefCell<HeadRecord>,
}

/// A smart pointer to the current window context.
impl std::ops::Deref for DesktopService {
    type Target = DesktopWindowContext;

    fn deref(&self) -> &Self::Target {
        &self.window_context
    }
}

impl DesktopService {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        webview: WebView,
        window: Arc<Window>,
        app: Rc<DesktopAppContext>,
        target_id: RenderTargetId,
        asset_handlers: AssetHandlerRegistry,
        file_hover: NativeFileHover,
        close_behaviour: WindowCloseBehaviour,
        page_loaded: Arc<std::sync::atomic::AtomicBool>,
        page_awaited: Arc<std::sync::atomic::AtomicBool>,
        page_begun: Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        Self {
            app,
            window_context: DesktopWindowContext::new(
                webview,
                window,
                target_id,
                asset_handlers,
                file_hover,
                close_behaviour,
            ),
            page_loaded,
            page_awaited,
            page_begun,
            head_elements: RefCell::new(HeadRecord::default()),
        }
    }

    pub(crate) fn app_context(&self) -> &Rc<DesktopAppContext> {
        &self.app
    }

    pub(crate) fn register_component_window(
        self: &Rc<Self>,
        on_close_requested: impl FnMut() + 'static,
        on_destroyed: impl FnMut() + 'static,
    ) -> ComponentWindowRegistration {
        self.window_context
            .register_component_window_callbacks(on_close_requested, on_destroyed);

        ComponentWindowRegistration {
            window: Rc::downgrade(self),
        }
    }

    pub(crate) fn request_window_close(&self) -> WindowCloseRequestResult {
        self.window_context.request_window_close()
    }

    pub(crate) fn notify_window_destroyed(&self) {
        self.window_context.notify_window_destroyed();
    }

    /// Loads the window's page again, from the virtual dom that never went
    /// anywhere.
    ///
    /// For a page the application has found lost: on screen but showing
    /// nothing of what the virtual dom holds, and not answering. The platform
    /// reclaims a page's process from an application in the background and
    /// says so, and that case is handled without being asked; this is for the
    /// cases it does not say so, which an application notices by asking the
    /// page a question and getting no answer. The page is loaded once more,
    /// the virtual dom is rebuilt into it, and the head is put back.
    pub fn reload_lost_page(&self) {
        self.page_awaited
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.page_loaded
            .store(false, std::sync::atomic::Ordering::SeqCst);
        _ = self
            .app
            .proxy
            .send_event(UserWindowEvent::PageLost(self.window.id()));
    }

    /// Start the creation of a new window using the props and window builder
    ///
    /// Returns a future that resolves to the webview handle for the new window. You can use this
    /// to control other windows from the current window once the new window is created.
    ///
    /// Be careful to not create a cycle of windows, or you might leak memory.
    ///
    /// # Example
    ///
    /// ```rust, no_run
    /// use dioxus::prelude::*;
    ///
    /// # async fn app() {
    /// // Create the window and keep its render target.
    /// let pending_window = dioxus::desktop::window().new_window(Default::default());
    /// let target = pending_window.target_id();
    ///
    /// // Wait for the desktop context to be created.
    /// let window = pending_window.await;
    /// window.set_fullscreen(true);
    ///
    /// // Render into `target` from the app's existing VDOM, for example with
    /// // the `Window` component or a `Portal`.
    /// let _ = target;
    /// # }
    /// ```
    // Note: This method is asynchronous because webview2 does not support creating a new window from
    // inside of an existing webview callback. Dioxus runs event handlers synchronously inside of a webview
    // callback. See [this page](https://learn.microsoft.com/en-us/microsoft-edge/webview2/concepts/threading-model#reentrancy) for more information.
    //
    // Related issues:
    // - https://github.com/tauri-apps/wry/issues/583
    // - https://github.com/DioxusLabs/dioxus/issues/3080
    pub fn new_window(&self, cfg: WindowConfig) -> PendingDesktopWindow {
        self.app.new_window(cfg)
    }

    /// trigger the drag-window event
    ///
    /// Moves the window with the left mouse button until the button is released.
    ///
    /// you need use it in `onmousedown` event:
    /// ```rust, ignore
    /// onmousedown: move |_| { desktop.drag_window(); }
    /// ```
    pub fn drag(&self) {
        if self.window.fullscreen().is_none() {
            _ = self.window.drag_window();
        }
    }

    /// Toggle whether the window is maximized or not
    pub fn toggle_maximized(&self) {
        self.window.set_maximized(!self.window.is_maximized())
    }

    /// Records a script that puts an element into the head, so that it can be
    /// put into a page that replaces this one.
    pub(crate) fn remember_head_element(&self, js: String) -> HeadInsertion {
        self.head_elements.borrow_mut().remember(js)
    }

    /// Puts every remembered head element into the page again.
    ///
    /// Called once a page that replaced another is ready, however many pages
    /// were replaced before it. On a page that replaced nothing there is
    /// nothing remembered yet, so this does nothing.
    pub(crate) fn replay_head_elements(&self) {
        let scripts = self.head_elements.borrow_mut().replay();
        if scripts.is_empty() {
            return;
        }
        tracing::debug!(
            "Putting {} head elements back into the new page.",
            scripts.len()
        );
        for js in scripts {
            self.webview.evaluate_script(&js).ok();
        }
    }

    /// Set the close behavior of this window
    ///
    /// By default, windows close when the user clicks the close button.
    /// If this is set to `WindowCloseBehaviour::WindowHides`, the window will hide instead of closing.
    pub fn set_close_behavior(&self, behaviour: WindowCloseBehaviour) {
        self.close_behaviour.set(behaviour);
    }

    /// Close this window
    pub fn close(&self) {
        let _ = self
            .app
            .proxy
            .send_event(UserWindowEvent::RequestWindowClose(self.id()));
    }

    /// Close a particular window, given its ID
    pub fn close_window(&self, id: WindowId) {
        let _ = self
            .app
            .proxy
            .send_event(UserWindowEvent::RequestWindowClose(id));
    }

    /// change window to fullscreen
    pub fn set_fullscreen(&self, fullscreen: bool) {
        if let Some(handle) = &self.window.current_monitor() {
            self.window.set_fullscreen(
                fullscreen.then_some(WryFullscreen::Borderless(Some(handle.clone()))),
            );
        }
    }

    /// launch print modal
    pub fn print(&self) {
        if let Err(e) = self.webview.print() {
            tracing::warn!("Open print modal failed: {e}");
        }
    }

    /// Set the zoom level of the webview
    pub fn set_zoom_level(&self, level: f64) {
        if let Err(e) = self.webview.zoom(level) {
            tracing::warn!("Set webview zoom failed: {e}");
        }
    }

    /// opens DevTool window
    pub fn devtool(&self) {
        #[cfg(debug_assertions)]
        self.webview.open_devtools();

        #[cfg(not(debug_assertions))]
        tracing::warn!("Devtools are disabled in release builds");
    }

    /// Create a wry event handler that listens for wry events.
    /// This event handler is scoped to the currently active window and will only receive events that are either global or related to the current window.
    ///
    /// The id this function returns can be used to remove the event handler with [`Self::remove_wry_event_handler`]
    pub fn create_wry_event_handler(
        &self,
        handler: impl FnMut(&Event<UserWindowEvent>, &EventLoopWindowTarget<UserWindowEvent>) + 'static,
    ) -> WryEventHandler {
        self.app.event_handlers.add(self.window.id(), handler)
    }

    /// Remove a wry event handler created with [`Self::create_wry_event_handler`]
    pub fn remove_wry_event_handler(&self, id: WryEventHandler) {
        self.app.event_handlers.remove(id)
    }

    /// Create a global shortcut
    ///
    /// Linux: Only works on x11. See [this issue](https://github.com/tauri-apps/tao/issues/331) for more information.
    pub fn create_shortcut(
        &self,
        hotkey: HotKey,
        callback: impl FnMut(HotKeyState) + 'static,
    ) -> Result<ShortcutHandle, ShortcutRegistryError> {
        self.app
            .shortcut_manager
            .add_shortcut(hotkey, Box::new(callback))
    }

    /// Remove a global shortcut
    pub fn remove_shortcut(&self, id: ShortcutHandle) {
        self.app.shortcut_manager.remove_shortcut(id)
    }

    /// Remove all global shortcuts
    pub fn remove_all_shortcuts(&self) {
        self.app.shortcut_manager.remove_all()
    }

    /// Provide a callback to handle asset loading yourself.
    /// If the ScopeId isn't provided, defaults to a global handler.
    /// Note that the handler is namespaced by name, not ScopeId.
    ///
    /// When the component is dropped, the handler is removed.
    ///
    /// See [`crate::use_asset_handler`] for a convenient hook.
    pub fn register_asset_handler(
        &self,
        name: String,
        handler: impl Fn(AssetRequest, RequestAsyncResponder) + 'static,
    ) {
        self.asset_handlers
            .register_handler(name, Callback::new(move |(req, resp)| handler(req, resp)))
    }

    /// Provide a thread-safe handler to answer asset requests yourself, on whichever thread the
    /// web view asks from, without waiting for the event loop or for any other request. It
    /// replaces any handler of either kind registered under the same name.
    ///
    /// See [`crate::use_thread_safe_asset_handler`] for a convenient hook.
    pub fn register_thread_safe_asset_handler(
        &self,
        name: String,
        handler: impl Fn(AssetRequest, RequestAsyncResponder) + Send + Sync + 'static,
    ) {
        self.asset_handlers
            .register_thread_safe_handler(name, std::sync::Arc::new(handler))
    }

    /// Removes an asset handler of either kind by its identifier.
    ///
    /// Returns `None` if the handler did not exist.
    pub fn remove_asset_handler(&self, name: &str) -> Option<()> {
        self.asset_handlers.remove_handler(name)
    }

    #[cfg(target_os = "ios")]
    /// Get a retained reference to the current UIView
    pub fn ui_view(&self) -> objc2::rc::Retained<objc2_ui_kit::UIView> {
        use objc2::rc::Retained;
        use objc2_ui_kit::UIView;
        let ui_view = self.window.ui_view().cast::<UIView>();
        unsafe { Retained::retain(ui_view) }.unwrap()
    }

    #[cfg(target_os = "ios")]
    /// Get a retained reference to the current UIViewController
    pub fn ui_view_controller(&self) -> objc2::rc::Retained<objc2_ui_kit::UIViewController> {
        use objc2::rc::Retained;
        use objc2_ui_kit::UIViewController;
        let ui_view_controller = self.window.ui_view_controller().cast::<UIViewController>();
        unsafe { Retained::retain(ui_view_controller) }.unwrap()
    }

    /// Push an objc view to the window
    #[cfg(target_os = "ios")]
    pub fn push_view(&self, new_view: Retained<UIView>) {
        use objc2_ui_kit::UIViewAutoresizing;

        assert!(is_main_thread());
        let current_ui_view = self.ui_view();
        let current_ui_view_frame = current_ui_view.frame();

        new_view.setFrame(current_ui_view_frame);
        new_view.setAutoresizingMask(UIViewAutoresizing::from_bits(31).unwrap());

        let ui_view_controller = self.ui_view_controller();
        ui_view_controller.setView(Some(&new_view));
        self.views.borrow_mut().push(new_view);
    }

    /// Pop an objc view from the window
    #[cfg(target_os = "ios")]
    pub fn pop_view(&self) {
        assert!(is_main_thread());
        if let Some(view) = self.views.borrow_mut().pop() {
            self.ui_view_controller().setView(Some(&view));
        }
    }
}

#[cfg(target_os = "ios")]
fn is_main_thread() -> bool {
    objc2_foundation::NSThread::isMainThread_class()
}

/// A desktop window that is pending creation.
///
/// # Example
/// ```rust, no_run
/// # use dioxus::prelude::*;
/// # async fn app() {
/// // Create a new window asynchronously
/// let pending_window = dioxus::desktop::window().new_window(Default::default());
/// let target = pending_window.target_id();
///
/// // Wait for the context to be created
/// let window = pending_window.await;
///
/// // Now control the window
/// window.set_fullscreen(true);
///
/// // Render into `target` from the app's existing VDOM.
/// # }
/// ```
pub struct PendingDesktopWindow {
    pub(crate) target_id: RenderTargetId,
    pub(crate) receiver: futures_channel::oneshot::Receiver<DesktopContext>,
    pub(crate) cancellation: PendingWindowCancellation,
}

impl PendingDesktopWindow {
    /// The render target associated with the pending window.
    pub fn target_id(&self) -> RenderTargetId {
        self.target_id
    }

    /// Get the cancellation handle for the queued window.
    pub(crate) fn cancellation(&self) -> PendingWindowCancellation {
        self.cancellation.clone()
    }

    /// Resolve the pending context into a [`DesktopContext`].
    pub async fn resolve(self) -> DesktopContext {
        self.try_resolve()
            .await
            .expect("Failed to resolve pending desktop context")
    }

    /// Try to resolve the pending context into a [`DesktopContext`].
    pub async fn try_resolve(self) -> Result<DesktopContext, futures_channel::oneshot::Canceled> {
        self.receiver.await
    }
}

impl IntoFuture for PendingDesktopWindow {
    type Output = DesktopContext;

    type IntoFuture = Pin<Box<dyn Future<Output = Self::Output>>>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(self.resolve())
    }
}

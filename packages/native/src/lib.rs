#![cfg_attr(docsrs, feature(doc_cfg))]

//! A native renderer for Dioxus.
//!
//! ## Feature flags
//!  - `default`: Enables the features listed below.
//!  - `accessibility`: Enables [`accesskit`](https://docs.rs/accesskit/latest/accesskit/) accessibility support.
//!  - `hot-reload`: Enables hot-reloading of Dioxus RSX.
//!  - `menu`: Enables the [`muda`](https://docs.rs/muda/latest/muda/) menubar.
//!  - `tracing`: Enables tracing support.

mod assets;
mod config;
mod contexts;
mod dioxus_application;
mod dioxus_renderer;
mod event_handlers;
mod hooks;
mod link_handler;

#[cfg(feature = "prelude")]
pub mod prelude;

#[cfg(all(feature = "net", not(target_arch = "wasm32")))]
use blitz_traits::net::NetProvider;
#[doc(inline)]
pub use dioxus_native_dom::*;

use assets::DioxusNativeNetProvider;
pub use dioxus_application::{DioxusNativeApplication, DioxusNativeEvent};
pub use dioxus_renderer::DioxusNativeWindowRenderer;

#[cfg(target_os = "android")]
#[cfg_attr(docsrs, doc(cfg(target_os = "android")))]
/// Set the current [`AndroidApp`](android_activity::AndroidApp).
pub fn set_android_app(app: android_activity::AndroidApp) {
    blitz_shell::set_android_app(app);
}

#[cfg(target_os = "android")]
#[cfg_attr(docsrs, doc(cfg(target_os = "android")))]
/// Get the current [`AndroidApp`](android_activity::AndroidApp).
/// This will panic if the android activity has not been setup with [`set_android_app`].
pub fn current_android_app() -> android_activity::AndroidApp {
    blitz_shell::current_android_app()
}

#[cfg(target_os = "android")]
#[cfg_attr(docsrs, doc(cfg(target_os = "android")))]
pub use android_activity::AndroidApp;

#[cfg(any(feature = "vello", feature = "vello-hybrid"))]
pub use {
    dioxus_renderer::{Features, Limits},
    wgpu_context::DeviceHandle,
};

pub use blitz_dom::{FontContext, Widget, build_single_font_ctx};
pub use config::Config;
pub use event_handlers::WinitEventHandlerId;
pub use hooks::{use_back_button, use_window_event};
pub use winit;
pub use winit::dpi::{LogicalSize, PhysicalSize};
pub use winit::window::WindowAttributes;

use blitz_shell::{BlitzShellEvent, BlitzShellProxy, WindowConfig, create_default_event_loop};
use dioxus_core::{ComponentFunction, Element, VirtualDom, consume_context, use_hook};
use link_handler::DioxusNativeNavigationProvider;
use std::any::Any;
use std::sync::Arc;
use winit::{
    raw_window_handle::{HasWindowHandle as _, RawWindowHandle},
    window::Window,
};

pub fn use_window() -> Arc<dyn Window> {
    use_hook(consume_context::<Arc<dyn Window>>)
}

pub fn use_raw_window_handle() -> RawWindowHandle {
    use_hook(|| {
        consume_context::<Arc<dyn Window>>()
            .window_handle()
            .unwrap()
            .as_raw()
    })
}

/// Launch an interactive HTML/CSS renderer driven by the Dioxus virtualdom
pub fn launch(app: fn() -> Element) {
    launch_cfg(app, vec![], vec![])
}

pub fn launch_cfg(
    app: fn() -> Element,
    contexts: Vec<Box<dyn Fn() -> Box<dyn Any> + Send + Sync>>,
    cfg: Vec<Box<dyn Any>>,
) {
    launch_cfg_with_props(app, (), contexts, cfg)
}

/// A window for [`launch_windows`] to open when the application starts: the
/// virtual dom that renders into it and the configuration it opens with.
pub struct NativeWindow {
    vdom: VirtualDom,
    config: Config,
}

impl NativeWindow {
    pub fn new(vdom: VirtualDom, config: Config) -> Self {
        Self { vdom, config }
    }
}

/// The renderer settings every window shares, read from a launch's configs.
struct RendererSettings {
    #[cfg(any(feature = "vello", feature = "vello-hybrid"))]
    features: Option<Features>,
    #[cfg(any(feature = "vello", feature = "vello-hybrid"))]
    limits: Option<Limits>,
}

// todo: props shouldn't have the clone bound - should try and match dioxus-desktop behavior
pub fn launch_cfg_with_props<P: Clone + 'static, M: 'static>(
    app: impl ComponentFunction<P, M>,
    props: P,
    contexts: Vec<Box<dyn Fn() -> Box<dyn Any> + Send + Sync>>,
    configs: Vec<Box<dyn Any>>,
) {
    // Macro to attempt to downcast a type out of a Box<dyn Any>
    macro_rules! try_read_config {
        ($input:ident, $store:ident, $kind:ty) => {
            // Try to downcast the Box<dyn Any> to type $kind
            match $input.downcast::<$kind>() {
                // If the type matches then write downcast value to variable $store
                Ok(value) => {
                    $store = Some(*value);
                    continue;
                }
                // Else extract the original Box<dyn Any> value out of the error type
                // and return it so that we can try again with a different type.
                Err(cfg) => cfg,
            }
        };
    }

    // Read config values
    #[cfg(any(feature = "vello", feature = "vello-hybrid"))]
    let (mut features, mut limits) = (None, None);
    let mut window_attributes = None;
    let mut config = None;
    for mut cfg in configs {
        #[cfg(any(feature = "vello", feature = "vello-hybrid"))]
        {
            cfg = try_read_config!(cfg, features, Features);
            cfg = try_read_config!(cfg, limits, Limits);
        }
        cfg = try_read_config!(cfg, window_attributes, WindowAttributes);
        cfg = try_read_config!(cfg, config, Config);
        let _ = cfg;
    }

    let mut config = config.unwrap_or_default();
    if let Some(window_attributes) = window_attributes {
        config.window_attributes = window_attributes;
    }

    // Build the vdom first; the net provider, document, and other window-bound
    // contexts are attached once the event-loop proxy exists.
    let mut vdom = VirtualDom::new_with_props(app, props);

    for context in contexts {
        vdom.insert_any_root_context(context());
    }

    run_windows(
        vec![NativeWindow::new(vdom, config)],
        RendererSettings {
            #[cfg(any(feature = "vello", feature = "vello-hybrid"))]
            features,
            #[cfg(any(feature = "vello", feature = "vello-hybrid"))]
            limits,
        },
    );
}

/// Launch several windows at once, each rendering its own virtual dom, in one
/// application and one event loop. Every window's components get the same
/// contexts a window opened by [`launch_cfg`] gets. The configs are read for
/// the renderer settings every window shares.
pub fn launch_windows(windows: Vec<NativeWindow>, configs: Vec<Box<dyn Any>>) {
    #[cfg(any(feature = "vello", feature = "vello-hybrid"))]
    let (mut features, mut limits) = (None, None);
    for cfg in configs {
        #[cfg(any(feature = "vello", feature = "vello-hybrid"))]
        let cfg = match cfg.downcast::<Features>() {
            Ok(value) => {
                features = Some(*value);
                continue;
            }
            Err(cfg) => cfg,
        };
        #[cfg(any(feature = "vello", feature = "vello-hybrid"))]
        let cfg = match cfg.downcast::<Limits>() {
            Ok(value) => {
                limits = Some(*value);
                continue;
            }
            Err(cfg) => cfg,
        };
        let _ = cfg;
    }
    run_windows(
        windows,
        RendererSettings {
            #[cfg(any(feature = "vello", feature = "vello-hybrid"))]
            features,
            #[cfg(any(feature = "vello", feature = "vello-hybrid"))]
            limits,
        },
    );
}

fn run_windows(windows: Vec<NativeWindow>, settings: RendererSettings) {
    assert!(
        !windows.is_empty(),
        "Dioxus Native needs at least one window to launch."
    );
    let event_loop = create_default_event_loop();
    let winit_proxy = event_loop.create_proxy();
    let (proxy, event_queue) = BlitzShellProxy::new(winit_proxy);

    // Turn on the runtime and enter it
    #[cfg(feature = "net")]
    #[cfg(not(target_arch = "wasm32"))]
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    #[cfg(feature = "net")]
    #[cfg(not(target_arch = "wasm32"))]
    let _guard = rt.enter();

    // Setup hot-reloading if enabled.
    #[cfg(all(feature = "hot-reload", debug_assertions))]
    #[cfg(not(target_arch = "wasm32"))]
    {
        let proxy = proxy.clone();
        dioxus_devtools::connect(move |event| {
            let dxn_event = DioxusNativeEvent::DevserverEvent(event);
            proxy.send_event(BlitzShellEvent::embedder_event(dxn_event));
        })
    }

    let configs = windows
        .into_iter()
        .map(|window| window_config(window, &proxy, &settings))
        .collect();

    // Create application
    let application = DioxusNativeApplication::with_windows(proxy, event_queue, configs);

    // Run event loop
    event_loop.run_app(application).unwrap();
}

/// Attach the net, html parser, and navigation providers to one window's
/// virtual dom, and pair its document with a renderer of its own.
fn window_config(
    window: NativeWindow,
    proxy: &BlitzShellProxy,
    settings: &RendererSettings,
) -> WindowConfig<DioxusNativeWindowRenderer> {
    let NativeWindow { vdom, config } = window;

    #[cfg(all(feature = "net", not(target_arch = "wasm32")))]
    let net_provider = {
        let net_waker = Some(Arc::new(proxy.clone()) as _);
        let inner_net_provider = Arc::new(blitz_net::Provider::new(net_waker));
        vdom.provide_root_context(Arc::clone(&inner_net_provider));

        Arc::new(DioxusNativeNetProvider::with_inner(
            proxy.clone(),
            inner_net_provider as _,
        )) as Arc<dyn NetProvider>
    };

    #[cfg(any(not(feature = "net"), target_arch = "wasm32"))]
    let net_provider = DioxusNativeNetProvider::shared(proxy.clone());

    vdom.provide_root_context(Arc::clone(&net_provider));

    #[cfg(feature = "html")]
    let html_parser_provider = {
        let html_parser = Arc::new(blitz_html::HtmlProvider) as _;
        vdom.provide_root_context(Arc::clone(&html_parser));
        Some(html_parser)
    };
    #[cfg(not(feature = "html"))]
    let html_parser_provider = None;

    let navigation_provider = Some(Arc::new(DioxusNativeNavigationProvider) as _);

    // Create document + window from the baked virtualdom
    let doc = DioxusDocument::new(
        vdom,
        DocumentConfig {
            net_provider: Some(net_provider),
            html_parser_provider,
            navigation_provider,
            font_ctx: config.font_ctx,
            ..Default::default()
        },
    );
    #[cfg(all(
        feature = "vello-hybrid",
        not(any(feature = "vello", feature = "vello-cpu-base", feature = "skia"))
    ))]
    let renderer = DioxusNativeWindowRenderer::for_background(
        settings.features,
        settings.limits.clone(),
        config.background,
    );
    #[cfg(all(
        any(feature = "vello", feature = "vello-hybrid"),
        not(all(
            feature = "vello-hybrid",
            not(any(feature = "vello", feature = "vello-cpu-base", feature = "skia"))
        ))
    ))]
    let renderer = {
        let _ = config.background;
        DioxusNativeWindowRenderer::with_features_and_limits(
            settings.features,
            settings.limits.clone(),
        )
    };
    #[cfg(not(any(feature = "vello", feature = "vello-hybrid")))]
    let renderer = {
        let _ = (config.background, settings);
        DioxusNativeWindowRenderer::new()
    };
    WindowConfig::with_attributes(Box::new(doc) as _, renderer, config.window_attributes)
}

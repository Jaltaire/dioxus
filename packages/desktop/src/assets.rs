use dioxus_core::Callback;
use rustc_hash::FxHashMap;
use std::{borrow::Cow, cell::RefCell, rc::Rc};
use wry::http::{Request, Response};

/// A request for an asset within dioxus-desktop.
pub type AssetRequest = Request<Vec<u8>>;

#[allow(missing_docs)]
pub struct RequestAsyncResponder {
    responder: Box<dyn FnOnce(Response<Cow<'static, [u8]>>) + Send>,
}

impl RequestAsyncResponder {
    pub(crate) fn new(
        responder: impl FnOnce(Response<Cow<'static, [u8]>>) + Send + 'static,
    ) -> Self {
        Self {
            responder: Box::new(responder),
        }
    }

    #[allow(missing_docs)]
    pub fn respond<T: Into<Cow<'static, [u8]>>>(self, response: Response<T>) {
        let (parts, body) = response.into_parts();
        (self.responder)(Response::from_parts(parts, body.into()));
    }
}

pub struct AssetHandler {
    f: Callback<(AssetRequest, RequestAsyncResponder)>,
}

#[derive(Clone)]
pub struct AssetHandlerRegistry {
    handlers: Rc<RefCell<FxHashMap<String, AssetHandler>>>,
}

impl AssetHandlerRegistry {
    pub fn new() -> Self {
        AssetHandlerRegistry {
            handlers: Default::default(),
        }
    }

    pub fn has_handler(&self, name: &str) -> bool {
        self.handlers.borrow().contains_key(name)
    }

    pub fn handle_request(
        &self,
        name: &str,
        request: AssetRequest,
        responder: RequestAsyncResponder,
    ) {
        if let Some(handler) = self.handlers.borrow().get(name) {
            // Avoid handler being already borrowed on android
            #[cfg(target_os = "android")]
            let _lock = crate::android_sync_lock::android_runtime_lock();

            // And run the handler in the scope of the component that created it
            handler.f.call((request, responder));
        }
    }

    pub fn register_handler(
        &self,
        name: String,
        f: Callback<(AssetRequest, RequestAsyncResponder)>,
    ) {
        self.handlers.borrow_mut().insert(name, AssetHandler { f });
    }

    pub fn remove_handler(&self, name: &str) -> Option<AssetHandler> {
        self.handlers.borrow_mut().remove(name)
    }
}

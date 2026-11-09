use dioxus_vdom::Callback;
use rustc_hash::FxHashMap;
use std::{
    borrow::Cow,
    cell::RefCell,
    rc::Rc,
    sync::{Arc, PoisonError, RwLock},
};
use wry::http::{Request, Response};

/// A request for an asset within dioxus-desktop.
pub type AssetRequest = Request<Vec<u8>>;

type Respond = Box<dyn FnOnce(Response<Cow<'static, [u8]>>) + Send>;

#[allow(missing_docs)]
pub struct RequestAsyncResponder {
    responder: Respond,
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

/// An asset handler that may be called on any thread, so a request for it is answered on the
/// thread that asked, without waiting for the event loop or for any other request.
pub(crate) type ThreadSafeAssetHandler =
    Arc<dyn Fn(AssetRequest, RequestAsyncResponder) + Send + Sync>;

/// The thread-safe asset handlers of one web view, which its request threads read without either
/// of the locks that guard the rest of what a request reaches.
#[derive(Clone, Default)]
pub(crate) struct ThreadSafeAssetHandlers {
    handlers: Arc<RwLock<FxHashMap<String, ThreadSafeAssetHandler>>>,
}

impl ThreadSafeAssetHandlers {
    /// The handler registered under `name`, held apart from the table, so that removing it while
    /// a request is being answered never waits for the answer.
    pub(crate) fn get(&self, name: &str) -> Option<ThreadSafeAssetHandler> {
        self.handlers
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(name)
            .cloned()
    }

    fn insert(&self, name: String, handler: ThreadSafeAssetHandler) {
        self.handlers
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(name, handler);
    }

    fn remove(&self, name: &str) -> Option<ThreadSafeAssetHandler> {
        self.handlers
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(name)
    }
}

#[derive(Clone)]
pub struct AssetHandlerRegistry {
    handlers: Rc<RefCell<FxHashMap<String, AssetHandler>>>,
    thread_safe: ThreadSafeAssetHandlers,
}

impl AssetHandlerRegistry {
    pub fn new() -> Self {
        AssetHandlerRegistry {
            handlers: Default::default(),
            thread_safe: Default::default(),
        }
    }

    /// The thread-safe handlers, for the request threads to read without either lock.
    pub(crate) fn thread_safe(&self) -> ThreadSafeAssetHandlers {
        self.thread_safe.clone()
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
            let _lock = crate::android_sync_lock::android_runtime_lock();

            // And run the handler in the scope of the component that created it
            handler.f.call((request, responder));
        }
    }

    /// Registers a handler that runs in the scope of the component that created it, replacing any
    /// handler of either kind registered under the same name.
    pub fn register_handler(
        &self,
        name: String,
        f: Callback<(AssetRequest, RequestAsyncResponder)>,
    ) {
        self.thread_safe.remove(&name);
        self.handlers.borrow_mut().insert(name, AssetHandler { f });
    }

    /// Registers a handler that answers on whichever thread asks, replacing any handler of either
    /// kind registered under the same name.
    pub(crate) fn register_thread_safe_handler(
        &self,
        name: String,
        handler: ThreadSafeAssetHandler,
    ) {
        self.handlers.borrow_mut().remove(&name);
        self.thread_safe.insert(name, handler);
    }

    /// Removes the handler registered under `name`, of either kind, and says whether there was one.
    pub fn remove_handler(&self, name: &str) -> Option<()> {
        let removed = self.handlers.borrow_mut().remove(name).is_some();
        let removed_thread_safe = self.thread_safe.remove(name).is_some();
        (removed || removed_thread_safe).then_some(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dioxus_vdom::{ScopeId, VirtualDom};
    use std::sync::{
        Barrier,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    };

    fn answering(body: &'static [u8]) -> ThreadSafeAssetHandler {
        Arc::new(move |_, responder: RequestAsyncResponder| {
            responder.respond(Response::new(body));
        })
    }

    fn ask(handler: &ThreadSafeAssetHandler) -> Vec<u8> {
        let (sender, receiver) = mpsc::channel();
        handler(
            Request::new(Vec::new()),
            RequestAsyncResponder::new(move |response| {
                sender.send(response.into_body().into_owned()).unwrap()
            }),
        );
        receiver.recv().unwrap()
    }

    fn scoped(dom: &VirtualDom, registry: &AssetHandlerRegistry, name: &str) {
        dom.in_scope(ScopeId::ROOT, || {
            registry.register_handler(name.to_string(), Callback::new(|_| {}));
        });
    }

    #[test]
    fn a_thread_safe_handler_is_found_by_its_name_and_by_no_other() {
        let registry = AssetHandlerRegistry::new();
        registry.register_thread_safe_handler("media".to_string(), answering(b"Media."));
        let handlers = registry.thread_safe();
        let found = handlers.get("media").expect("The handler was registered.");
        assert_eq!(ask(&found), b"Media.");
        assert!(handlers.get("assets").is_none());
        assert!(
            !registry.has_handler("media"),
            "A thread-safe handler is not one the locked path serves."
        );
    }

    #[test]
    fn a_name_belongs_to_whichever_kind_of_handler_was_registered_under_it_last() {
        let dom = VirtualDom::new(dioxus_vdom::VNode::empty);
        let registry = AssetHandlerRegistry::new();
        scoped(&dom, &registry, "media");
        registry.register_thread_safe_handler("media".to_string(), answering(b"Media."));
        assert!(!registry.has_handler("media"));
        assert!(registry.thread_safe().get("media").is_some());
        scoped(&dom, &registry, "media");
        assert!(registry.has_handler("media"));
        assert!(registry.thread_safe().get("media").is_none());
    }

    #[test]
    fn removing_a_name_removes_its_handler_of_either_kind_and_says_whether_there_was_one() {
        let dom = VirtualDom::new(dioxus_vdom::VNode::empty);
        let registry = AssetHandlerRegistry::new();
        scoped(&dom, &registry, "assets");
        registry.register_thread_safe_handler("media".to_string(), answering(b"Media."));
        assert_eq!(registry.remove_handler("assets"), Some(()));
        assert!(!registry.has_handler("assets"));
        assert_eq!(registry.remove_handler("media"), Some(()));
        assert!(registry.thread_safe().get("media").is_none());
        assert_eq!(registry.remove_handler("media"), None);
        assert_eq!(registry.remove_handler("never"), None);
    }

    #[test]
    fn a_handler_removed_while_it_answers_finishes_its_answer_and_answers_nothing_more() {
        let registry = AssetHandlerRegistry::new();
        let (entered, has_entered) = mpsc::channel();
        let (release, released) = mpsc::channel::<()>();
        let released = std::sync::Mutex::new(released);
        registry.register_thread_safe_handler(
            "media".to_string(),
            Arc::new(move |_, responder: RequestAsyncResponder| {
                entered.send(()).unwrap();
                released.lock().unwrap().recv().unwrap();
                responder.respond(Response::new(b"Media.".as_slice()));
            }),
        );
        let handlers = registry.thread_safe();
        let answer = std::thread::spawn(move || {
            let handler = handlers.get("media").unwrap();
            ask(&handler)
        });
        has_entered.recv().unwrap();
        assert_eq!(
            registry.remove_handler("media"),
            Some(()),
            "The handler is removed while it is still answering, without waiting for it."
        );
        assert!(registry.thread_safe().get("media").is_none());
        release.send(()).unwrap();
        assert_eq!(answer.join().unwrap(), b"Media.");
    }

    #[test]
    fn many_threads_find_and_call_handlers_while_they_are_registered_and_removed() {
        const THREADS: usize = 8;
        const ASKS: usize = 2_000;
        let registry = AssetHandlerRegistry::new();
        let handlers = registry.thread_safe();
        let answered = Arc::new(AtomicUsize::new(0));
        let missed = Arc::new(AtomicUsize::new(0));
        let start = Arc::new(Barrier::new(THREADS + 1));
        let askers: Vec<_> = (0..THREADS)
            .map(|_| {
                let handlers = handlers.clone();
                let (answered, missed, start) = (answered.clone(), missed.clone(), start.clone());
                std::thread::spawn(move || {
                    start.wait();
                    for _ in 0..ASKS {
                        match handlers.get("media") {
                            Some(handler) => {
                                assert_eq!(ask(&handler), b"Media.");
                                answered.fetch_add(1, Ordering::SeqCst);
                            }
                            None => {
                                missed.fetch_add(1, Ordering::SeqCst);
                            }
                        }
                    }
                })
            })
            .collect();
        start.wait();
        for _ in 0..ASKS {
            registry.register_thread_safe_handler("media".to_string(), answering(b"Media."));
            registry.remove_handler("media");
        }
        registry.register_thread_safe_handler("media".to_string(), answering(b"Media."));
        for asker in askers {
            asker.join().expect("No asker panicked.");
        }
        assert_eq!(
            answered.load(Ordering::SeqCst) + missed.load(Ordering::SeqCst),
            THREADS * ASKS,
            "Every ask either found a handler and was answered or found none."
        );
    }
}

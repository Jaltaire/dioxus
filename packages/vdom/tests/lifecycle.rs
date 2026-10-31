#![allow(unused, non_upper_case_globals)]
#![allow(non_snake_case)]

//! Tests for the lifecycle of components.
use dioxus::html::SerializedHtmlEventConverter;
use dioxus::prelude::*;
use dioxus_renderer_oracle::RendererOracle;
use std::any::Any;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

type Shared<T> = Arc<Mutex<T>>;

#[derive(Clone, Default)]
struct PageTracker {
    created: Arc<std::sync::atomic::AtomicUsize>,
    live: Arc<std::sync::atomic::AtomicUsize>,
}

struct PageLease(PageTracker);

impl PageLease {
    fn new(tracker: PageTracker) -> Self {
        tracker
            .created
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        tracker
            .live
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Self(tracker)
    }
}

impl Drop for PageLease {
    fn drop(&mut self) {
        self.0
            .live
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

fn new_page_app() -> Element {
    let tracker = consume_context::<PageTracker>();
    let _lease = use_hook(|| Rc::new(PageLease::new(tracker)));
    rsx! { div { "Recovered" } }
}

#[test]
fn a_new_page_replaces_the_mounted_component_tree() {
    let tracker = PageTracker::default();
    let mut dom = VirtualDom::new(new_page_app).with_root_context(tracker.clone());
    dom.rebuild(&mut dioxus_vdom::NoOpMutations);

    assert_eq!(tracker.created.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(tracker.live.load(std::sync::atomic::Ordering::SeqCst), 1);

    dom.rebuild_into_a_new_page(&mut dioxus_vdom::NoOpMutations);

    assert_eq!(tracker.created.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(tracker.live.load(std::sync::atomic::Ordering::SeqCst), 1);

    dom.rebuild_into_a_new_page(&mut dioxus_vdom::NoOpMutations);

    assert_eq!(tracker.created.load(std::sync::atomic::Ordering::SeqCst), 3);
    assert_eq!(tracker.live.load(std::sync::atomic::Ordering::SeqCst), 1);

    drop(dom);
    assert_eq!(tracker.live.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[test]
fn a_new_page_can_be_built_from_a_dom_never_mounted() {
    let tracker = PageTracker::default();
    let mut dom = VirtualDom::new(new_page_app).with_root_context(tracker.clone());

    dom.rebuild_into_a_new_page(&mut dioxus_vdom::NoOpMutations);

    assert_eq!(tracker.created.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(tracker.live.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[test]
fn manual_diffing() {
    #[derive(Clone)]
    struct AppProps {
        value: Shared<&'static str>,
    }

    fn app(cx: AppProps) -> Element {
        let val = cx.value.lock().unwrap();
        rsx! { div { "{val}" } }
    };

    let value = Arc::new(Mutex::new("Hello"));
    fn expected_goodbye() -> Element {
        rsx! { div { "goodbye" } }
    }

    let mut dom = VirtualDom::new_with_props(app, AppProps { value: value.clone() });
    let mut oracle = RendererOracle::new();
    oracle.rebuild(&mut dom);

    *value.lock().unwrap() = "goodbye";

    dom.mark_dirty(ScopeId::APP);
    oracle.render(&mut dom);
    oracle.assert_matches(expected_goodbye);
}

#[test]
fn events_generate() {
    set_event_converter(Box::new(SerializedHtmlEventConverter));
    fn app() -> Element {
        let mut count = use_signal(|| 0);

        match count() {
            0 => rsx! {
                div { id: "click-target", onclick: move |_| count += 1,
                    div { "nested" }
                    "Click me!"
                }
            },
            _ => VNode::empty(),
        }
    };

    let mut dom = VirtualDom::new(app);
    let mut oracle = RendererOracle::new();
    oracle.rebuild(&mut dom);

    let event = Event::new(
        Rc::new(PlatformEventData::new(Box::<SerializedMouseData>::default())) as Rc<dyn Any>,
        true,
    );
    let target = oracle.element_id_by_attr("id", "click-target");
    dom.runtime().handle_event("click", event, target);

    dom.mark_dirty(ScopeId::APP);
    let summary = oracle.render(&mut dom);
    // Anchor diff: the populated div is just removed when the component
    // returns empty. No placeholder swap is needed.
    assert_eq!(summary.removes, 1);
    assert_eq!(summary.replaces, 0);
}

#![allow(unused, non_upper_case_globals)]
#![allow(non_snake_case)]

//! Tests for the lifecycle of components.
use dioxus::dioxus_core::{ElementId, Mutation::*};
use dioxus::html::SerializedHtmlEventConverter;
use dioxus::prelude::*;
use std::any::Any;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

type Shared<T> = Arc<Mutex<T>>;

#[derive(Clone, Default)]
struct RendererLossTracker {
    created: Arc<std::sync::atomic::AtomicUsize>,
    live: Arc<std::sync::atomic::AtomicUsize>,
}

struct RendererLossLease(RendererLossTracker);

impl RendererLossLease {
    fn new(tracker: RendererLossTracker) -> Self {
        tracker
            .created
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        tracker
            .live
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Self(tracker)
    }
}

impl Drop for RendererLossLease {
    fn drop(&mut self) {
        self.0
            .live
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

fn renderer_loss_app() -> Element {
    let tracker = consume_context::<RendererLossTracker>();
    let _lease = use_hook(|| Rc::new(RendererLossLease::new(tracker)));
    rsx! { div { "Recovered" } }
}

#[test]
fn renderer_loss_replaces_the_mounted_component_tree() {
    let tracker = RendererLossTracker::default();
    let mut dom = VirtualDom::new(renderer_loss_app).with_root_context(tracker.clone());
    dom.rebuild(&mut dioxus_core::NoOpMutations);

    assert_eq!(tracker.created.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(tracker.live.load(std::sync::atomic::Ordering::SeqCst), 1);

    dom.rebuild_after_renderer_loss(&mut dioxus_core::NoOpMutations);

    assert_eq!(tracker.created.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(tracker.live.load(std::sync::atomic::Ordering::SeqCst), 1);

    dom.rebuild_after_renderer_loss(&mut dioxus_core::NoOpMutations);

    assert_eq!(tracker.created.load(std::sync::atomic::Ordering::SeqCst), 3);
    assert_eq!(tracker.live.load(std::sync::atomic::Ordering::SeqCst), 1);

    drop(dom);
    assert_eq!(tracker.live.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[test]
fn renderer_loss_can_build_an_unmounted_dom() {
    let tracker = RendererLossTracker::default();
    let mut dom = VirtualDom::new(renderer_loss_app).with_root_context(tracker.clone());

    dom.rebuild_after_renderer_loss(&mut dioxus_core::NoOpMutations);

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
    let mut dom = VirtualDom::new_with_props(app, AppProps { value: value.clone() });

    dom.rebuild(&mut dioxus_core::NoOpMutations);

    *value.lock().unwrap() = "goodbye";

    assert_eq!(
        dom.rebuild_to_vec().edits,
        [
            LoadTemplate { index: 0, id: ElementId(3) },
            CreateTextNode { value: "goodbye".to_string(), id: ElementId(4) },
            ReplacePlaceholder { path: &[0], m: 1 },
            AppendChildren { m: 1, id: ElementId(0) }
        ]
    );
}

#[test]
fn events_generate() {
    set_event_converter(Box::new(SerializedHtmlEventConverter));
    fn app() -> Element {
        let mut count = use_signal(|| 0);

        match count() {
            0 => rsx! {
                div { onclick: move |_| count += 1,
                    div { "nested" }
                    "Click me!"
                }
            },
            _ => VNode::empty(),
        }
    };

    let mut dom = VirtualDom::new(app);
    dom.rebuild(&mut dioxus_core::NoOpMutations);

    let event = Event::new(
        Rc::new(PlatformEventData::new(Box::<SerializedMouseData>::default())) as Rc<dyn Any>,
        true,
    );
    dom.runtime().handle_event("click", event, ElementId(1));

    dom.mark_dirty(ScopeId::APP);
    let edits = dom.render_immediate_to_vec();

    assert_eq!(
        edits.edits,
        [
            CreatePlaceholder { id: ElementId(2) },
            ReplaceWith { id: ElementId(1), m: 1 }
        ]
    )
}

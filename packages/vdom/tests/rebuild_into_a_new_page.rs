//! Tests rebuilding a dom into a page that replaced the one it was rendered into.
use dioxus::dioxus_vdom::{Mutations, NoOpMutations, use_drop};
use dioxus::prelude::*;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

static INSTANCES: AtomicUsize = AtomicUsize::new(0);
static DROPS: AtomicUsize = AtomicUsize::new(0);
static TICKS: Mutex<Vec<usize>> = Mutex::new(Vec::new());

fn app() -> Element {
    rsx! {
        div { Child {} }
    }
}

#[component]
fn Child() -> Element {
    use_hook(|| {
        let instance = INSTANCES.fetch_add(1, Ordering::SeqCst);
        spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(5)).await;
                TICKS.lock().unwrap().push(instance);
            }
        });
    });
    use_drop(|| {
        DROPS.fetch_add(1, Ordering::SeqCst);
    });
    rsx! {
        span { "a child" }
    }
}

async fn drive(dom: &mut VirtualDom, for_how_long: Duration) {
    let deadline = tokio::time::Instant::now() + for_how_long;
    loop {
        tokio::select! {
            _ = dom.wait_for_work() => dom.render_immediate(&mut NoOpMutations),
            _ = tokio::time::sleep_until(deadline) => break,
        }
    }
}

#[tokio::test(start_paused = true)]
async fn the_old_page_is_forgotten_and_the_new_one_built_as_a_first_one_is() {
    let mut dom = VirtualDom::new(app);
    let first_build = dom.rebuild_to_vec();
    assert_eq!(INSTANCES.load(Ordering::SeqCst), 1);
    assert_eq!(DROPS.load(Ordering::SeqCst), 0);

    drive(&mut dom, Duration::from_millis(60)).await;
    assert!(TICKS.lock().unwrap().iter().all(|instance| *instance == 0));
    assert!(!TICKS.lock().unwrap().is_empty());

    let mut new_page = Mutations::default();
    dom.rebuild_into_a_new_page(&mut new_page);
    assert_eq!(INSTANCES.load(Ordering::SeqCst), 2);
    assert_eq!(DROPS.load(Ordering::SeqCst), 1);
    assert_eq!(
        new_page.edits, first_build.edits,
        "A new page is built with the same edits, and the same element ids, as a first one."
    );

    TICKS.lock().unwrap().clear();
    drive(&mut dom, Duration::from_millis(60)).await;
    let ticks = TICKS.lock().unwrap().clone();
    assert!(!ticks.is_empty(), "The new page's task never ran.");
    assert!(
        ticks.iter().all(|instance| *instance == 1),
        "A task of the forgotten page ticked on: {ticks:?}"
    );
}

#[test]
fn a_dom_never_rendered_is_built_as_a_first_one_is() {
    fn plain() -> Element {
        rsx! {
            div { "hello" }
        }
    }
    let mut first = VirtualDom::new(plain);
    let mut second = VirtualDom::new(plain);
    let mut built_new = Mutations::default();
    second.rebuild_into_a_new_page(&mut built_new);
    assert_eq!(first.rebuild_to_vec().edits, built_new.edits);
}

#[test]
fn a_page_built_more_than_once_never_refers_to_a_template_the_page_was_not_given() {
    fn repeated() -> Element {
        rsx! {
            for row in 0..3 {
                div { key: "{row}", span { "row" } }
            }
        }
    }
    let mut dom = VirtualDom::new(repeated);
    let first_build = dom.rebuild_to_vec();
    for _ in 0..3 {
        let mut new_page = Mutations::default();
        dom.rebuild_into_a_new_page(&mut new_page);
        assert_eq!(
            new_page.edits, first_build.edits,
            "Every new page is given every template it is built from, as the first page was."
        );
    }
}

mod targets {
    use dioxus::dioxus_vdom::{MultiWriter, Mutations, Portal, RenderTargetId, use_drop};
    use dioxus::prelude::*;
    use std::cell::Cell;
    use std::collections::BTreeMap;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static PORTAL_INSTANCES: AtomicUsize = AtomicUsize::new(0);
    static PORTAL_DROPS: AtomicUsize = AtomicUsize::new(0);
    static ROOT_INSTANCES: AtomicUsize = AtomicUsize::new(0);

    #[derive(Default)]
    struct Targets(BTreeMap<RenderTargetId, Mutations>);

    impl MultiWriter for Targets {
        type Writer = Mutations;

        fn writer_for(&mut self, id: RenderTargetId) -> Option<&mut Mutations> {
            Some(self.0.entry(id).or_default())
        }
    }

    impl Targets {
        fn written(self) -> BTreeMap<RenderTargetId, Mutations> {
            self.0
                .into_iter()
                .filter(|(_, mutations)| !mutations.edits.is_empty())
                .collect()
        }
    }

    #[derive(Clone)]
    struct Slot(Rc<Cell<RenderTargetId>>);

    impl PartialEq for Slot {
        fn eq(&self, other: &Self) -> bool {
            Rc::ptr_eq(&self.0, &other.0)
        }
    }

    #[derive(Clone, PartialEq, Props)]
    struct WindowedProps {
        target: Slot,
    }

    fn windowed(props: WindowedProps) -> Element {
        use_hook(|| ROOT_INSTANCES.fetch_add(1, Ordering::SeqCst));
        rsx! {
            div { "root content" }
            Portal {
                target: props.target.0.get(),
                InWindow {}
            }
        }
    }

    #[component]
    fn InWindow() -> Element {
        use_hook(|| PORTAL_INSTANCES.fetch_add(1, Ordering::SeqCst));
        use_drop(|| {
            PORTAL_DROPS.fetch_add(1, Ordering::SeqCst);
        });
        rsx! {
            for row in 0..2 {
                p { key: "{row}", span { "in the window" } }
            }
        }
    }

    #[test]
    fn a_target_is_rebuilt_into_a_new_page_and_no_other_target_is_touched() {
        let slot = Slot(Rc::new(Cell::new(RenderTargetId::ROOT)));
        let mut dom = VirtualDom::new_with_props(windowed, WindowedProps { target: slot.clone() });
        let window = dom.runtime().create_render_target();
        slot.0.set(window);

        let mut first = Targets::default();
        dom.rebuild(&mut first);
        let first = first.written();
        assert!(first.contains_key(&RenderTargetId::ROOT));
        assert!(first.contains_key(&window));
        assert_eq!(PORTAL_INSTANCES.load(Ordering::SeqCst), 1);
        assert_eq!(ROOT_INSTANCES.load(Ordering::SeqCst), 1);

        for page in 2..=3 {
            let mut new_page = Targets::default();
            dom.rebuild_target_into_a_new_page(window, &mut new_page);
            let new_page = new_page.written();
            assert_eq!(
                new_page.keys().copied().collect::<Vec<_>>(),
                vec![window],
                "Only the target whose page was replaced is written to."
            );
            assert_eq!(
                new_page[&window].edits, first[&window].edits,
                "The new page is built with the same edits and element ids as the first."
            );
            assert_eq!(PORTAL_INSTANCES.load(Ordering::SeqCst), page);
            assert_eq!(PORTAL_DROPS.load(Ordering::SeqCst), page - 1);
            assert_eq!(
                ROOT_INSTANCES.load(Ordering::SeqCst),
                1,
                "The scope holding the portal keeps its state."
            );
        }
    }

    #[test]
    fn a_target_no_portal_renders_into_is_left_as_it_is() {
        fn plain() -> Element {
            rsx! { div { "only the root" } }
        }
        let mut dom = VirtualDom::new(plain);
        let unused = dom.runtime().create_render_target();
        dom.rebuild(&mut Targets::default());
        let mut new_page = Targets::default();
        dom.rebuild_target_into_a_new_page(unused, &mut new_page);
        assert!(new_page.written().is_empty());
    }
}

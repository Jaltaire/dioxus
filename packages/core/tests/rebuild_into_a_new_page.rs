//! Tests rebuilding a dom into a page that replaced the one it was rendered into.
use dioxus::dioxus_core::{NoOpMutations, use_drop};
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
    let instance = use_hook(|| {
        let instance = INSTANCES.fetch_add(1, Ordering::SeqCst);
        spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(5)).await;
                TICKS.lock().unwrap().push(instance);
            }
        });
        instance
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

#[tokio::test]
async fn the_old_page_is_forgotten_and_the_new_one_built_as_a_first_one_is() {
    let mut dom = VirtualDom::new(app);
    let first_build = dom.rebuild_to_vec();
    assert_eq!(INSTANCES.load(Ordering::SeqCst), 1);
    assert_eq!(DROPS.load(Ordering::SeqCst), 0);

    drive(&mut dom, Duration::from_millis(60)).await;
    assert!(TICKS.lock().unwrap().iter().all(|instance| *instance == 0));
    assert!(!TICKS.lock().unwrap().is_empty());

    let mut new_page = dioxus_core::Mutations::default();
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
    let mut built_new = dioxus_core::Mutations::default();
    second.rebuild_into_a_new_page(&mut built_new);
    assert_eq!(first.rebuild_to_vec().edits, built_new.edits);
}

use std::sync::atomic::{AtomicBool, Ordering};

use dioxus::prelude::*;
use dioxus_desktop::wry::http::{Response, StatusCode};
use dioxus_desktop::{AssetRequest, RequestAsyncResponder, window};

#[path = "./utils.rs"]
mod utils;

pub fn main() {
    #[cfg(not(windows))]
    utils::check_app_exits(app);
}

static PAGE_LOST: AtomicBool = AtomicBool::new(false);

const MODULES: &str = "modules";
const COUNTED: &str = "/modules/counted.mjs";
const COUNTED_SOURCE: &str = "window.moduleRuns = (window.moduleRuns ?? 0) + 1; window.dispatchEvent(new Event('module-ran'));";
const MISSING: &str = "/no-such-module.mjs";
const REFUSAL: &str = "the module refused to start";
const PAGE_BASE: &str = "/modules/based/";
const BASED: &str = "/modules/based/module.mjs";
const BASED_FROM_THE_PAGE_BASE: &str = "./module.mjs";
const BASED_SOURCE: &str = "window.basedRuns = (window.basedRuns ?? 0) + 1;";

const GATED_MODULE: &str = r#"
    window.moduleGate = new Promise((resolve) => { window.openModuleGate = resolve; });
    return URL.createObjectURL(new Blob(
        ["await window.moduleGate; window.moduleRan = (window.moduleRan ?? 0) + 1;"],
        { type: "text/javascript" },
    ));
"#;

const THROWING_MODULE: &str = r#"
    return URL.createObjectURL(new Blob(
        ["throw new Error('the module refused to start');"],
        { type: "text/javascript" },
    ));
"#;

fn serve_modules(request: AssetRequest, responder: RequestAsyncResponder) {
    let source = match request.uri().path() {
        COUNTED => Some(COUNTED_SOURCE),
        BASED => Some(BASED_SOURCE),
        _ => None,
    };
    let response = match source {
        Some(source) => Response::builder()
            .header("Content-Type", "text/javascript")
            .body(source.as_bytes().to_vec()),
        None => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Vec::new()),
    };
    responder.respond(response.expect("The response is well formed."));
}

async fn ran(variable: &str) -> u32 {
    document::eval(&format!("return window.{variable} ?? 0;"))
        .join()
        .await
        .expect("The page answers how often the module ran.")
}

async fn answered_only_after_running() {
    let gated: String = document::eval(GATED_MODULE)
        .join()
        .await
        .expect("The page makes the gated module.");
    let answered = std::cell::Cell::new(false);
    let starting = async {
        let started = document::start_module(&gated).await;
        answered.set(true);
        assert!(
            started.is_ok(),
            "A module that runs is started: {started:?}"
        );
        assert_eq!(
            ran("moduleRan").await,
            1,
            "The start is answered only once the module has run."
        );
    };
    let opening = async {
        assert_eq!(ran("moduleRan").await, 0);
        assert!(
            !answered.get(),
            "The start is not answered while the module waits to run."
        );
        document::eval("window.openModuleGate(); return null;")
            .await
            .expect("The page opens the gate.");
    };
    futures_util::future::join(starting, opening).await;
}

async fn a_missing_module_is_refused() {
    let started = document::start_module(MISSING).await;
    assert!(
        started.is_err(),
        "A module whose script cannot be found is not started: {started:?}"
    );
}

async fn a_module_that_throws_is_refused_with_its_error() {
    let throwing: String = document::eval(THROWING_MODULE)
        .join()
        .await
        .expect("The page makes the throwing module.");
    let started = document::start_module(&throwing).await;
    match started {
        Err(document::EvalError::Communication(refusal)) => assert!(
            refusal.contains(REFUSAL),
            "The refusal carries the module's own error: {refusal}"
        ),
        other => panic!("A module that throws as it runs is not started: {other:?}"),
    }
}

async fn a_module_started_again_does_not_run_again() {
    document::start_module(COUNTED)
        .await
        .expect("The counted module is started.");
    document::start_module(COUNTED)
        .await
        .expect("The counted module is started again.");
    assert_eq!(
        ran("moduleRuns").await,
        1,
        "A module started twice in one page runs once."
    );
}

async fn first_page() {
    answered_only_after_running().await;
    a_missing_module_is_refused().await;
    a_module_that_throws_is_refused_with_its_error().await;
    a_module_started_again_does_not_run_again().await;
    PAGE_LOST.store(true, Ordering::SeqCst);
    window().reload_lost_page();
}

async fn replacing_page() {
    let replayed: u32 = document::eval(
        "if (window.moduleRuns === undefined) { await new Promise((resolve) => window.addEventListener('module-ran', resolve, { once: true })); } return window.moduleRuns;",
    )
    .join()
    .await
    .expect("The replacing page answers how often the module ran.");
    assert_eq!(
        replayed, 1,
        "A page that replaces a lost one runs the modules the lost page started."
    );
    document::start_module(COUNTED)
        .await
        .expect("The counted module is started in the replacing page.");
    assert_eq!(
        ran("moduleRuns").await,
        1,
        "Starting a module the replacing page already runs does not run it again."
    );
    a_module_s_address_is_resolved_against_the_page_s_base().await;
    window().close();
}

async fn a_module_s_address_is_resolved_against_the_page_s_base() {
    document::eval(&format!(
        "const base = document.createElement('base'); base.href = '{PAGE_BASE}'; document.head.prepend(base); return null;"
    ))
    .await
    .expect("The page takes a base.");
    let started = document::start_module(BASED_FROM_THE_PAGE_BASE).await;
    document::eval("document.querySelector('base').remove(); return null;")
        .await
        .expect("The page drops its base.");
    assert!(
        started.is_ok(),
        "A module whose address is relative to the page's base is started: {started:?}"
    );
    assert_eq!(
        ran("basedRuns").await,
        1,
        "A module is found where the page's base says it is."
    );
}

fn app() -> Element {
    use_hook(|| window().register_asset_handler(MODULES.to_string(), serve_modules));
    use_future(|| async {
        if PAGE_LOST.load(Ordering::SeqCst) {
            replacing_page().await;
        } else {
            first_page().await;
        }
    });
    rsx! {
        div { "modules" }
    }
}

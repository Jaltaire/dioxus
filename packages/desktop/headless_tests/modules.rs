use dioxus::prelude::*;
use dioxus_desktop::window;

#[path = "./utils.rs"]
mod utils;

pub fn main() {
    #[cfg(not(windows))]
    utils::check_app_exits(app);
}

const MISSING: &str = "/no-such-module.mjs";
const REFUSAL: &str = "the module refused to start";

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

const COUNTED_MODULE: &str = r#"
    return URL.createObjectURL(new Blob(
        ["window.moduleRuns = (window.moduleRuns ?? 0) + 1;"],
        { type: "text/javascript" },
    ));
"#;

async fn made(module: &str) -> String {
    document::eval(module)
        .join()
        .await
        .expect("The page makes the module.")
}

async fn ran(variable: &str) -> u32 {
    document::eval(&format!("return window.{variable} ?? 0;"))
        .join()
        .await
        .expect("The page answers how often the module ran.")
}

async fn answered_only_after_running() {
    let gated = made(GATED_MODULE).await;
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
    let throwing = made(THROWING_MODULE).await;
    match document::start_module(&throwing).await {
        Err(document::EvalError::Communication(refusal)) => assert!(
            refusal.contains(REFUSAL),
            "The refusal carries the module's own error: {refusal}"
        ),
        other => panic!("A module that throws as it runs is not started: {other:?}"),
    }
}

async fn a_module_started_again_does_not_run_again() {
    let counted = made(COUNTED_MODULE).await;
    document::start_module(&counted)
        .await
        .expect("The counted module is started.");
    document::start_module(&counted)
        .await
        .expect("The counted module is started again.");
    assert_eq!(
        ran("moduleRuns").await,
        1,
        "A module started twice in one page runs once."
    );
}

fn app() -> Element {
    use_future(|| async {
        answered_only_after_running().await;
        a_missing_module_is_refused().await;
        a_module_that_throws_is_refused_with_its_error().await;
        a_module_started_again_does_not_run_again().await;
        window().close();
    });
    rsx! {
        div { "modules" }
    }
}

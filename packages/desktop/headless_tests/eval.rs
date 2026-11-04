use dioxus::prelude::*;
use dioxus_desktop::window;
use serde::Deserialize;

#[path = "./utils.rs"]
mod utils;

pub fn main() {
    #[cfg(not(windows))]
    utils::check_app_exits(app);
}

static EVALS_RECEIVED: GlobalSignal<usize> = Signal::global(|| 0);
static EVALS_RETURNED: GlobalSignal<usize> = Signal::global(|| 0);
static RELEASED_EVALS_RETURNED: GlobalSignal<usize> = Signal::global(|| 0);
const RELEASED_EVALS: usize = 50;
static FOCUS_ANSWERED: GlobalSignal<usize> = Signal::global(|| 0);

fn app() -> Element {
    // Double 100 values in the value
    use_future(|| async {
        let mut eval = document::eval(
            r#"for (let i = 0; i < 100; i++) {
            let value = await dioxus.recv();
            dioxus.send(value*2);
        }"#,
        );
        for i in 0..100 {
            eval.send(i).unwrap();
            let value: i32 = eval.recv().await.unwrap();
            assert_eq!(value, i * 2);
            EVALS_RECEIVED.with_mut(|x| *x += 1);
        }
    });

    // Make sure returning no value resolves the future
    use_future(|| async {
        let eval = document::eval(r#"return;"#);

        eval.await.unwrap();
        EVALS_RETURNED.with_mut(|x| *x += 1);
    });

    // Return a value from the future
    use_future(|| async {
        let eval = document::eval(
            r#"
        return [1, 2, 3];
        "#,
        );

        assert_eq!(
            Vec::<i32>::deserialize(&eval.await.unwrap()).unwrap(),
            vec![1, 2, 3]
        );
        EVALS_RETURNED.with_mut(|x| *x += 1);
    });

    use_future(|| async {
        let returns = (0..RELEASED_EVALS).map(|index| async move {
            document::eval(&format!(
                r#"queueMicrotask(() => queueMicrotask(() => window.ipc.postMessage(JSON.stringify({{
                    method: "query",
                    params: {{ id: dioxus.request_id, data: {{ method: "drop" }} }},
                }}))));
                return {index};"#
            ))
            .join::<usize>()
            .await
        });
        for (index, returned) in futures_util::future::join_all(returns)
            .await
            .into_iter()
            .enumerate()
        {
            assert_eq!(returned.unwrap(), index);
            RELEASED_EVALS_RETURNED.with_mut(|x| *x += 1);
        }
    });

    let mut field = use_signal(|| None::<std::rc::Rc<MountedData>>);
    use_effect(move || {
        let Some(field) = field() else {
            return;
        };
        spawn(async move {
            field.set_focus(true).await.unwrap();
            let focused: bool = document::eval(
                "return document.activeElement === document.getElementById('focused');",
            )
            .join()
            .await
            .unwrap();
            assert!(focused);
            field.set_focus(false).await.unwrap();
            let blurred: bool = document::eval(
                "return document.activeElement !== document.getElementById('focused');",
            )
            .join()
            .await
            .unwrap();
            assert!(blurred);
            FOCUS_ANSWERED.with_mut(|x| *x += 1);
        });
    });

    use_memo(|| {
        println!("expected 1 focus answered found {}", FOCUS_ANSWERED());
        println!("expected 100 evals received found {}", EVALS_RECEIVED());
        println!("expected 2 eval returned found {}", EVALS_RETURNED());
        println!(
            "expected {RELEASED_EVALS} released evals returned found {}",
            RELEASED_EVALS_RETURNED()
        );
        if EVALS_RECEIVED() == 100
            && EVALS_RETURNED() == 2
            && RELEASED_EVALS_RETURNED() == RELEASED_EVALS
            && FOCUS_ANSWERED() == 1
        {
            window().close();
        }
    });

    rsx! {
        input { id: "focused", onmounted: move |event| field.set(Some(event.data())) }
    }
}

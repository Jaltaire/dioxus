use dioxus::prelude::*;

fn main() {
    dioxus::launch(app);
}

fn app() -> Element {
    use_hook(dioxus::fullstack::commit_initial_chunk);

    rsx! {
        Hydrated {}
        EmptiedSibling {}
        FilledSibling {}
    }
}

#[component]
fn Hydrated() -> Element {
    #[cfg_attr(not(feature = "web"), allow(unused_mut))]
    let mut hydrated = use_signal(|| false);

    #[cfg(feature = "web")]
    use_effect(move || hydrated.set(true));

    rsx! {
        div { id: "hydrated", "data-hydrated": "{hydrated}" }
    }
}

#[component]
fn Row(on: bool) -> Element {
    rsx! {
        if on {
            b {}
        }
    }
}

#[component]
fn EmptiedSibling() -> Element {
    let mut on = use_signal(|| false);
    let on_now = on();

    rsx! {
        button { id: "flip-emptied", onclick: move |_| on.toggle(), "Flip" }
        div { id: "emptied",
            if on_now {
                span { class: "first" }
            }
            if !on_now {
                span { class: "second" }
            }
            for i in 0..1 {
                Row { key: "{i}", on: on_now }
            }
        }
    }
}

#[component]
fn FilledSibling() -> Element {
    let mut on = use_signal(|| false);
    let on_now = on();

    rsx! {
        button { id: "flip-filled", onclick: move |_| on.toggle(), "Flip" }
        div { id: "filled",
            i {}
            if on_now {
                span {}
            }
            for i in 0..1 {
                Row { key: "{i}", on: on_now }
            }
        }
    }
}

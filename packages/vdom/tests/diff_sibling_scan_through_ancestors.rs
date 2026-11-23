use dioxus::prelude::*;
use dioxus_renderer_oracle::RendererOracle;
use dioxus_vdom::{ScopeId, generation};

fn assert_every_flip_matches_a_fresh_build(app: fn() -> Element) {
    let mut dom = VirtualDom::new(app);
    let mut oracle = RendererOracle::new();
    oracle.rebuild(&mut dom);
    oracle.assert_matches_vdom(&dom);

    for _ in 0..4 {
        dom.mark_dirty(ScopeId::APP);
        oracle.render(&mut dom);
        oracle.assert_matches_vdom(&dom);
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

#[test]
fn adjacent_conditionals_appear_and_vanish_together() {
    fn app() -> Element {
        let on = generation() % 2 == 1;
        rsx! {
            div {
                if on {
                    span {}
                }
                if on {
                    span {}
                }
            }
        }
    }

    assert_every_flip_matches_a_fresh_build(app);
}

#[test]
fn adjacent_conditionals_swap_while_the_list_after_them_fills() {
    fn app() -> Element {
        let odd = generation() % 2 == 1;
        let count = if odd { 2 } else { 0 };
        rsx! {
            div {
                if odd {
                    span { class: "first" }
                }
                if !odd {
                    span { class: "second" }
                }
                for i in 0..count {
                    p { "{i}" }
                }
            }
        }
    }

    assert_every_flip_matches_a_fresh_build(app);
}

#[test]
fn keyed_row_inserts_after_an_earlier_sibling_empties() {
    fn app() -> Element {
        let odd = generation() % 2 == 1;
        rsx! {
            div {
                if odd {
                    span { class: "first" }
                }
                if !odd {
                    span { class: "second" }
                }
                for i in 0..1 {
                    Row { key: "{i}", on: odd }
                }
            }
        }
    }

    assert_every_flip_matches_a_fresh_build(app);
}

#[test]
fn unkeyed_row_inserts_after_an_earlier_sibling_empties() {
    fn app() -> Element {
        let odd = generation() % 2 == 1;
        rsx! {
            div {
                if !odd {
                    span {}
                }
                for _ in 0..1 {
                    Row { on: odd }
                }
            }
        }
    }

    assert_every_flip_matches_a_fresh_build(app);
}

#[test]
fn keyed_row_inserts_after_an_earlier_sibling_fills() {
    fn app() -> Element {
        let odd = generation() % 2 == 1;
        rsx! {
            div {
                i {}
                if odd {
                    span {}
                }
                for i in 0..1 {
                    Row { key: "{i}", on: odd }
                }
            }
        }
    }

    assert_every_flip_matches_a_fresh_build(app);
}

#[test]
fn keyed_rows_grow_and_insert_after_adjacent_siblings_swap() {
    fn app() -> Element {
        let odd = generation() % 2 == 1;
        let count = if odd { 3 } else { 1 };
        rsx! {
            div {
                if odd {
                    span { class: "first" }
                }
                if !odd {
                    span { class: "second" }
                }
                for i in 0..count {
                    Row { key: "{i}", on: odd }
                }
            }
        }
    }

    assert_every_flip_matches_a_fresh_build(app);
}

use dioxus_interpreter_js::NATIVE_JS;

mod common;

fn handled(event: &str, listener_bubbles: bool, liveview: bool) -> String {
    let program = format!(
        r#"
        var told = [];
        var posted = [];
        var applied = [];
        var prevented = [];
        class Node {{}}
        class Element extends Node {{}}
        class HTMLInputElement extends Element {{}}
        class Event {{
            constructor(type, init = {{}}) {{
                this.type = type;
                this.bubbles = !!init.bubbles;
                this.cancelable = !!init.cancelable;
                this.isTrusted = !!init.trusted;
                this.target = new Element();
            }}
            preventDefault() {{ prevented.push(this.type); }}
            stopPropagation() {{}}
        }}
        class CustomEvent extends Event {{
            constructor(type, init = {{}}) {{
                super(type, init);
                this.detail = init.detail === undefined ? null : init.detail;
            }}
        }}
        function getTargetId(target) {{ return 5; }}
        function serializeEvent(event, target) {{
            return {{ detail: event.detail === undefined ? null : event.detail }};
        }}
        function {answers_nothing}
        class Page {{
            constructor() {{
                this.liveview = {liveview};
                this.page = 7;
                this.ipc = {{
                    postMessage(message) {{
                        if (typeof message !== "string") {{
                            throw new TypeError("Error processing argument at index 0, conversion failure from " + typeof message);
                        }}
                        posted.push(JSON.parse(message));
                    }}
                }};
            }}
            listensOnPath() {{ return true; }}
            handleClickNavigate() {{}}
            runEditsInFrame(edits) {{ applied.push(edits); }}
            sendSerializedEvent(body) {{
                told.push(body);
                return {{ preventDefault: true, stopPropagation: false, edits: "drawn" }};
            }}
            {handle_event}
            {send_without_answer}
            {send_ipc}
        }}
        new Page().handleEvent({event}, "heard", {listener_bubbles});
        JSON.stringify({{ told, posted, applied, prevented }})
        "#,
        answers_nothing = common::method(NATIVE_JS, "answersNothing"),
        handle_event = common::method(NATIVE_JS, "handleEvent"),
        send_without_answer = common::method(NATIVE_JS, "sendEventWithoutAnswer"),
        send_ipc = common::method(NATIVE_JS, "sendIpcMessage"),
    );
    let mut context = boa_engine::Context::default();
    let result = context
        .eval(boa_engine::Source::from_bytes(program.as_bytes()))
        .unwrap_or_else(|error| panic!("The event is handled without an error: {error}"));
    result
        .as_string()
        .expect("What happened is written as JSON.")
        .to_std_string_escaped()
}

const ASKED: &str = r#"{"told":[{"name":"heard","data":{"detail":null},"element":5,"bubbles":false}],"posted":[],"applied":["drawn"],"prevented":["heard"]}"#;

#[test]
fn an_event_the_page_dispatched_that_can_neither_be_cancelled_nor_travel_is_posted_with_its_page_and_holds_nothing()
{
    assert_eq!(
        handled(r#"new CustomEvent("heard", { detail: { pressed: 1 } })"#, false, false),
        r#"{"told":[],"posted":[{"method":"user_event","params":{"name":"heard","data":{"detail":{"pressed":1}},"element":5,"bubbles":false,"page":7}}],"applied":[],"prevented":[]}"#
    );
}

#[test]
fn a_liveview_page_sends_such_an_event_as_it_sends_a_mounted_one_and_reads_no_answer() {
    assert_eq!(
        handled(r#"new CustomEvent("heard")"#, false, true),
        r#"{"told":[{"name":"heard","data":{"detail":null},"element":5,"bubbles":false}],"posted":[],"applied":[],"prevented":[]}"#
    );
}

#[test]
fn an_event_the_page_dispatched_that_may_be_cancelled_waits_for_its_answer() {
    assert_eq!(
        handled(r#"new CustomEvent("heard", { cancelable: true })"#, false, false),
        ASKED
    );
}

#[test]
fn an_event_the_page_dispatched_that_travels_waits_for_its_answer() {
    assert_eq!(
        handled(r#"new CustomEvent("heard", { bubbles: true })"#, false, false),
        ASKED
    );
    assert_eq!(
        handled(r#"new CustomEvent("heard", { bubbles: true })"#, true, false),
        ASKED.replace(r#""bubbles":false"#, r#""bubbles":true"#)
    );
}

#[test]
fn an_event_the_browser_dispatched_waits_for_its_answer_however_it_was_made() {
    assert_eq!(
        handled(r#"new CustomEvent("heard", { trusted: true })"#, false, false),
        ASKED
    );
    assert_eq!(handled(r#"new Event("heard", { trusted: true })"#, false, false), ASKED);
}

#[test]
fn an_event_that_is_not_a_custom_one_waits_for_its_answer_and_draws_what_it_brings_in_frame() {
    assert_eq!(handled(r#"new Event("heard")"#, false, false), ASKED);
}

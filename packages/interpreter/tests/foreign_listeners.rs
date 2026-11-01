use dioxus_interpreter_js::{INTERPRETER_JS, NATIVE_JS};

mod common;

fn foreign_listener(event: &str) -> String {
    let program = format!(
        r#"
        var told = [];
        var mounted = [];
        var posted = [];
        var window = {{
            ipc: {{
                postMessage(message) {{
                    if (typeof message !== "string") {{
                        throw new TypeError("Error processing argument at index 0, conversion failure from " + typeof message);
                    }}
                    posted.push(message);
                }}
            }}
        }};
        class Element {{
            setAttribute(name, value) {{ this[name] = value; }}
        }}
        class Page {{
            constructor() {{ this.stack = [[new Element(), 5]]; }}
            currentTopId() {{ return this.stack[this.stack.length - 1][1]; }}
            sendSerializedEvent(body) {{
                told.push(body);
                return {{ preventDefault: false, stopPropagation: false }};
            }}
            sendMountedEvent(body) {{ mounted.push(body); }}
            createListener(name, node, bubbles) {{ told.push({{ listened: name, bubbles }}); }}
            {method}
        }}
        new Page().addTopForeignEventListener("{event}", false);
        JSON.stringify({{ told, mounted, posted }})
        "#,
        method = common::method(INTERPRETER_JS, "addTopForeignEventListener"),
    );
    let mut context = boa_engine::Context::default();
    let result = context
        .eval(boa_engine::Source::from_bytes(program.as_bytes()))
        .unwrap_or_else(|error| panic!("The listener is added without an error: {error}"));
    result
        .as_string()
        .expect("What happened is written as JSON.")
        .to_std_string_escaped()
}

#[test]
fn a_mounted_listener_hands_its_event_to_the_mounted_sender_at_once_and_waits_for_nothing() {
    assert_eq!(
        foreign_listener("mounted"),
        r#"{"told":[],"mounted":[{"name":"mounted","element":5,"data":null,"bubbles":false}],"posted":[]}"#
    );
}

#[test]
fn any_other_foreign_listener_is_listened_for() {
    assert_eq!(
        foreign_listener("visible"),
        r#"{"told":[{"listened":"visible","bubbles":false}],"mounted":[],"posted":[]}"#
    );
}

fn mounted_sent(liveview: bool) -> String {
    let program = format!(
        r#"
        var told = [];
        var posted = [];
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
            sendSerializedEvent(body) {{
                told.push(body);
                return {{ preventDefault: false, stopPropagation: false }};
            }}
            {send_mounted}
            {send_ipc}
        }}
        new Page().sendMountedEvent({{ name: "mounted", element: 5, data: null, bubbles: false }});
        JSON.stringify({{ told, posted }})
        "#,
        send_mounted = common::method(NATIVE_JS, "sendMountedEvent"),
        send_ipc = common::method(NATIVE_JS, "sendIpcMessage"),
    );
    let mut context = boa_engine::Context::default();
    let result = context
        .eval(boa_engine::Source::from_bytes(program.as_bytes()))
        .unwrap_or_else(|error| panic!("The mounted event is sent without an error: {error}"));
    result
        .as_string()
        .expect("What happened is written as JSON.")
        .to_std_string_escaped()
}

#[test]
fn a_native_page_posts_a_mounted_event_with_its_page_as_text_and_sends_no_request() {
    assert_eq!(
        mounted_sent(false),
        r#"{"told":[],"posted":[{"method":"user_event","params":{"name":"mounted","element":5,"data":null,"bubbles":false,"page":7}}]}"#
    );
}

#[test]
fn a_liveview_page_sends_a_mounted_event_as_it_sends_every_other_event() {
    assert_eq!(
        mounted_sent(true),
        r#"{"told":[{"name":"mounted","element":5,"data":null,"bubbles":false}],"posted":[]}"#
    );
}

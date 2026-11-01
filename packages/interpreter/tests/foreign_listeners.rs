use dioxus_interpreter_js::INTERPRETER_JS;

mod common;

fn foreign_listener(event: &str) -> String {
    let program = format!(
        r#"
        var told = [];
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
            createListener(name, node, bubbles) {{ told.push({{ listened: name, bubbles }}); }}
            {method}
        }}
        new Page().addTopForeignEventListener("{event}", false);
        JSON.stringify({{ told, posted }})
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
fn a_mounted_listener_is_told_at_once_and_posts_nothing_to_the_page_s_channel() {
    assert_eq!(
        foreign_listener("mounted"),
        r#"{"told":[{"name":"mounted","element":5,"data":null,"bubbles":false}],"posted":[]}"#
    );
}

#[test]
fn any_other_foreign_listener_is_listened_for() {
    assert_eq!(
        foreign_listener("visible"),
        r#"{"told":[{"listened":"visible","bubbles":false}],"posted":[]}"#
    );
}

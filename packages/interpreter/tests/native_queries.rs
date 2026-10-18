//! The native interpreter answers questions about an element - where it
//! stands, how far it scrolls, whether it takes focus - for every kind of
//! element the page can hold, an SVG one as much as an HTML one. Each
//! method is read from the shipped script and run against a page of
//! stand-in elements.

use dioxus_interpreter_js::NATIVE_JS;

/// The text of a method of the shipped native interpreter, from its name to
/// its closing brace.
fn method(name: &str) -> String {
    let opening = format!("{name}(");
    let start = NATIVE_JS
        .match_indices(&opening)
        .map(|(at, _)| at)
        .find(|at| {
            let before = NATIVE_JS[..*at].chars().next_back();
            let rest = &NATIVE_JS[*at..];
            let close = rest.find(')').expect("A method's parameters close.");
            !before.is_some_and(|character| character == '.' || character.is_alphanumeric())
                && rest[close + 1..].starts_with('{')
        })
        .unwrap_or_else(|| panic!("The shipped interpreter has {name}."));
    let mut depth = 0usize;
    for (offset, character) in NATIVE_JS[start..].char_indices() {
        match character {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return NATIVE_JS[start..start + offset + 1].to_string();
                }
            }
            _ => {}
        }
    }
    panic!("{name} never closes.");
}

/// Runs `call` against a page holding one element of `kind` under the id 7,
/// with every element query of the shipped interpreter defined on it, and
/// answers what `call` evaluates to, written as JSON.
fn asked(kind: &str, call: &str) -> String {
    let methods: Vec<String> = [
        "scrollTo",
        "scroll",
        "getScrollHeight",
        "getScrollLeft",
        "getScrollTop",
        "getScrollWidth",
        "getClientRect",
        "setFocus",
    ]
    .into_iter()
    .map(method)
    .collect();
    let program = format!(
        r#"
        class Node {{}}
        class Text extends Node {{}}
        class Element extends Node {{
            getBoundingClientRect() {{ return {{ x: 1, y: 2, width: 30, height: 40 }}; }}
            scrollIntoView() {{ this.told = "scrolled into view"; }}
            scroll(options) {{ this.told = "scrolled to " + options.left + "," + options.top; }}
            get scrollHeight() {{ return 400; }}
            get scrollLeft() {{ return 5; }}
            get scrollTop() {{ return 6; }}
            get scrollWidth() {{ return 300; }}
        }}
        class HTMLElement extends Element {{
            focus() {{ this.told = "focused"; }}
            blur() {{ this.told = "blurred"; }}
        }}
        class SVGElement extends Element {{
            focus() {{ this.told = "focused"; }}
            blur() {{ this.told = "blurred"; }}
        }}
        class Page {{
            constructor(node) {{ this.nodes = {{ 7: node }}; }}
            {methods}
        }}
        const node = new {kind}();
        const page = new Page(node);
        const answer = {call};
        answer === undefined ? "undefined" : JSON.stringify(answer)
        "#,
        methods = methods.join("\n"),
    );
    let mut context = boa_engine::Context::default();
    let result = context
        .eval(boa_engine::Source::from_bytes(program.as_bytes()))
        .unwrap_or_else(|error| panic!("The interpreter's methods run: {error}"));
    result
        .as_string()
        .expect("The answer is written as JSON.")
        .to_std_string_escaped()
}

const MEASURED: &str = r#"{"type":"GetClientRect","origin":[1,2],"size":[30,40]}"#;

#[test]
fn an_svg_element_is_measured_as_an_html_one_is() {
    for kind in ["HTMLElement", "SVGElement", "Element"] {
        assert_eq!(asked(kind, "page.getClientRect(7)"), MEASURED, "{kind}");
    }
    assert_eq!(
        asked("Text", "page.getClientRect(7)"),
        "undefined",
        "A text node is no element, and has no box to tell."
    );
    assert_eq!(asked("HTMLElement", "page.getClientRect(8)"), "undefined");
}

#[test]
fn an_svg_element_scrolls_and_tells_its_scroll_as_an_html_one_does() {
    for kind in ["HTMLElement", "SVGElement", "Element"] {
        assert_eq!(
            asked(
                kind,
                "[page.getScrollHeight(7), page.getScrollLeft(7), page.getScrollTop(7), page.getScrollWidth(7)]"
            ),
            "[400,5,6,300]",
            "{kind}"
        );
        assert_eq!(
            asked(kind, "[page.scrollTo(7, {}), node.told]"),
            r#"[true,"scrolled into view"]"#,
            "{kind}"
        );
        assert_eq!(
            asked(kind, r#"[page.scroll(7, 10, 20, "instant"), node.told]"#),
            r#"[true,"scrolled to 10,20"]"#,
            "{kind}"
        );
    }
    assert_eq!(
        asked(
            "Text",
            r#"[page.scrollTo(7, {}), page.scroll(7, 1, 2, "instant")]"#
        ),
        "[false,false]"
    );
    assert_eq!(asked("Text", "page.getScrollTop(7)"), "undefined");
}

#[test]
fn an_svg_element_takes_focus_as_an_html_one_does() {
    for kind in ["HTMLElement", "SVGElement"] {
        assert_eq!(
            asked(kind, "(page.setFocus(7, true), node.told)"),
            r#""focused""#,
            "{kind}"
        );
        assert_eq!(
            asked(kind, "(page.setFocus(7, false), node.told)"),
            r#""blurred""#,
            "{kind}"
        );
    }
    assert_eq!(
        asked("Element", "(page.setFocus(7, true), node.told === undefined)"),
        "true",
        "An element that cannot take focus is left alone."
    );
}

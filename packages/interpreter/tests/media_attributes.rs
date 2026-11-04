//! A media element's `muted` attribute only sets `defaultMuted` once the
//! element exists, while whether it plays without a gesture is decided by its
//! `muted` property. The shipped interpreters set and clear the property along
//! with the attribute, which a clone of a template's element is muted by, and
//! every other attribute keeps its old path. Each script is run against a
//! stand-in media element that keeps the two apart as a browser does.

use dioxus_interpreter_js::INTERPRETER_JS;

const SET_ATTRIBUTE_JS: &str = include_str!("../src/js/set_attribute.js");

const MEDIA_ELEMENT: &str = r#"
    class MediaElement {
        constructor() {
            this.attributes = {};
            this.muted = false;
            this.tagName = "VIDEO";
        }
        get defaultMuted() { return "muted" in this.attributes; }
        setAttribute(name, value) { this.attributes[name] = String(value); }
        removeAttribute(name) { delete this.attributes[name]; }
    }
    const told = (video) => JSON.stringify({
        muted: video.muted,
        defaultMuted: video.defaultMuted,
        attributes: video.attributes,
    });
"#;

fn without_export(script: &str) -> &str {
    let export = script
        .rfind("export{")
        .expect("The shipped script exports its interface.");
    &script[..export]
}

fn evaluated(program: &str) -> String {
    let mut context = boa_engine::Context::default();
    let result = context
        .eval(boa_engine::Source::from_bytes(program.as_bytes()))
        .unwrap_or_else(|error| panic!("The interpreter's script runs: {error}"));
    result
        .as_string()
        .expect("What the element holds is written as JSON.")
        .to_std_string_escaped()
}

fn through_the_interpreter(steps: &str) -> String {
    evaluated(&format!(
        r#"
        {script}
        {MEDIA_ELEMENT}
        const video = new MediaElement();
        const interpreter = Object.create(BaseInterpreter.prototype);
        interpreter.stack = [[video, 1]];
        {steps}
        told(video)
        "#,
        script = without_export(INTERPRETER_JS),
    ))
}

fn through_the_standalone_setter(steps: &str) -> String {
    evaluated(&format!(
        r#"
        {script}
        {MEDIA_ELEMENT}
        const video = new MediaElement();
        {steps}
        told(video)
        "#,
        script = without_export(SET_ATTRIBUTE_JS),
    ))
}

const MUTED: &str = r#"{"muted":true,"defaultMuted":true,"attributes":{"muted":"true"}}"#;
const UNMUTED: &str = r#"{"muted":false,"defaultMuted":false,"attributes":{}}"#;

#[test]
fn muted_is_set_through_the_property_the_autoplay_policy_reads() {
    assert_eq!(
        through_the_interpreter(r#"interpreter.setTopAttribute("muted", "true", null);"#),
        MUTED
    );
    assert_eq!(
        through_the_standalone_setter(r#"setAttributeInner(video, "muted", "true", "");"#),
        MUTED
    );
}

#[test]
fn muted_set_to_false_unmutes_the_element() {
    assert_eq!(
        through_the_interpreter(
            r#"
            video.muted = true;
            interpreter.setTopAttribute("muted", "true", null);
            interpreter.setTopAttribute("muted", "false", null);
            "#
        ),
        UNMUTED
    );
    assert_eq!(
        through_the_standalone_setter(
            r#"
            video.muted = true;
            setAttributeInner(video, "muted", "true", "");
            setAttributeInner(video, "muted", "false", "");
            "#
        ),
        UNMUTED
    );
}

#[test]
fn muted_removed_unmutes_the_element() {
    assert_eq!(
        through_the_interpreter(
            r#"
            video.muted = true;
            interpreter.setTopAttribute("muted", "true", null);
            interpreter.removeTopAttribute("muted", null);
            "#
        ),
        UNMUTED
    );
}

#[test]
fn muted_in_a_namespace_is_still_an_attribute() {
    assert_eq!(
        through_the_interpreter(
            r#"
            video.setAttributeNS = (ns, name, value) => video.setAttribute(ns + ":" + name, value);
            interpreter.setTopAttribute("muted", "true", "custom");
            "#
        ),
        r#"{"muted":false,"defaultMuted":false,"attributes":{"custom:muted":"true"}}"#
    );
}

#[test]
fn other_boolean_attributes_are_still_written_as_attributes() {
    assert_eq!(
        through_the_interpreter(
            r#"
            interpreter.setTopAttribute("autoplay", "true", null);
            interpreter.setTopAttribute("playsinline", "true", null);
            interpreter.setTopAttribute("loop", "false", null);
            "#
        ),
        r#"{"muted":false,"defaultMuted":false,"attributes":{"autoplay":"true","playsinline":"true"}}"#
    );
    assert_eq!(
        through_the_interpreter(
            r#"
            interpreter.setTopAttribute("autoplay", "true", null);
            interpreter.removeTopAttribute("autoplay", null);
            "#
        ),
        UNMUTED
    );
}

use std::collections::BTreeSet;

use dioxus_interpreter_js::{INTERPRETER_JS, NATIVE_JS};

fn declared(script: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for keyword in ["function ", "class "] {
        for (at, _) in script.match_indices(keyword) {
            let name: String = script[at + keyword.len()..]
                .chars()
                .take_while(|character| {
                    character.is_alphanumeric() || *character == '_' || *character == '$'
                })
                .collect();
            if !name.is_empty() {
                names.insert(name);
            }
        }
    }
    names
}

#[test]
fn the_native_interpreter_declares_nothing_the_base_interpreter_also_declares() {
    let base = declared(INTERPRETER_JS);
    let shared: Vec<String> = declared(NATIVE_JS)
        .into_iter()
        .filter(|name| base.contains(name))
        .collect();
    assert!(
        shared.is_empty(),
        "The desktop page loads both scripts into one module, where a name declared twice is a syntax error: {shared:?}"
    );
}

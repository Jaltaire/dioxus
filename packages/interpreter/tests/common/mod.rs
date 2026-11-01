/// The text of a method of a shipped interpreter script, from its name to
/// its closing brace.
pub fn method(script: &str, name: &str) -> String {
    let opening = format!("{name}(");
    let start = script
        .match_indices(&opening)
        .map(|(at, _)| at)
        .find(|at| {
            let before = script[..*at].chars().next_back();
            let rest = &script[*at..];
            let close = rest.find(')').expect("A method's parameters close.");
            !before.is_some_and(|character| character == '.' || character.is_alphanumeric())
                && rest[close + 1..].starts_with('{')
        })
        .unwrap_or_else(|| panic!("The shipped interpreter has {name}."));
    let body = start
        + script[start..]
            .find("){")
            .expect("A method's parameters close before its body.")
        + 1;
    let mut depth = 0usize;
    for (offset, character) in script[body..].char_indices() {
        match character {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return script[start..body + offset + 1].to_string();
                }
            }
            _ => {}
        }
    }
    panic!("{name} never closes.");
}

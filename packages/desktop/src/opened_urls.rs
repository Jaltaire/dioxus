//! The URLs the application has been asked to open.
//!
//! A URL reaches an application by two routes, and they arrive at very
//! different times. One follows a link while the application is already
//! running, and the window that should answer it exists. The other is what
//! launches the application in the first place, and arrives before the
//! first component has mounted - before there is anything to hand it to.
//!
//! So the URLs are kept here rather than only announced. Whoever is ready
//! to answer for them takes them, whether that is a moment after launch or
//! well into a session.

use std::sync::Mutex;

/// The URLs that have arrived and that nobody has taken yet.
static OPENED: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Keeps URLs until somebody takes them.
pub(crate) fn remember(urls: impl IntoIterator<Item = String>) {
    let mut opened = match OPENED.lock() {
        Ok(opened) => opened,
        Err(poisoned) => poisoned.into_inner(),
    };
    opened.extend(urls);
}

/// Takes the URLs the application has been asked to open and has not
/// answered for yet, leaving none behind.
///
/// Call this wherever the application is ready to act on a URL. It is
/// safe to call before anything has arrived, and safe to call repeatedly:
/// each URL is handed out exactly once.
pub fn take_opened_urls() -> Vec<String> {
    let mut opened = match OPENED.lock() {
        Ok(opened) => opened,
        Err(poisoned) => poisoned.into_inner(),
    };
    std::mem::take(&mut opened)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One test rather than several, because they all share the one place
    /// the URLs are kept and tests run alongside each other: split up,
    /// each would be taking the URLs the others were about to look for.
    #[test]
    fn urls_wait_to_be_taken_and_are_handed_out_once() {
        let _ = take_opened_urls();
        assert!(
            take_opened_urls().is_empty(),
            "Taking when nothing has arrived should be no answer rather than a panic."
        );

        remember(["dioxus://early".to_string()]);
        remember(["dioxus://later".to_string()]);
        assert_eq!(
            take_opened_urls(),
            vec!["dioxus://early".to_string(), "dioxus://later".to_string()],
            "A URL that arrived before anyone was listening should still be there, in \
             the order it arrived."
        );

        assert!(
            take_opened_urls().is_empty(),
            "Taking the URLs should leave none behind."
        );
    }
}

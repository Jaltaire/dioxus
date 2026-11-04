use crate::{
    DesktopContext, WeakDesktopContext,
    head_record::HeadInsertion,
    query::{Query, QueryEngine},
};
use dioxus_document::{
    Document, Eval, EvalError, Evaluator, LinkProps, MetaProps, ScriptProps, StyleProps,
    create_element_in_head,
};
use dioxus_vdom::queue_effect;

use generational_box::{AnyStorage, GenerationalBox, UnsyncStorage};

/// Code for the Dioxus channel used to communicate between the dioxus and javascript code
pub const NATIVE_EVAL_JS: &str = include_str!("./js/native_eval.js");

/// Represents the desktop-target's provider of evaluators.
#[derive(Clone)]
pub struct DesktopDocument {
    pub(crate) desktop_ctx: WeakDesktopContext,
}

impl DesktopDocument {
    pub fn new(desktop_ctx: DesktopContext) -> Self {
        let desktop_ctx = std::rc::Rc::downgrade(&desktop_ctx);
        Self { desktop_ctx }
    }
}

impl DesktopDocument {
    fn put_in_head(&self, js: String) {
        let insertion = match self.desktop_ctx.upgrade() {
            Some(ctx) => ctx.remember_head_element(js.clone()),
            None => HeadInsertion::PutIn,
        };
        match insertion {
            HeadInsertion::PutIn => {
                self.eval(js);
            }
            HeadInsertion::AlreadyInPage => {}
        }
    }
}

impl Document for DesktopDocument {
    fn eval(&self, js: String) -> Eval {
        Eval::new(DesktopEvaluator::create(
            self.desktop_ctx
                .upgrade()
                .expect("Window to exist when document is alive"),
            js,
        ))
    }

    fn set_title(&self, title: String) {
        if let Some(ctx) = self.desktop_ctx.upgrade() {
            ctx.set_title(&title);
        }
    }

    /// Create a new meta tag in the head
    fn create_meta(&self, props: MetaProps) {
        let myself = self.clone();
        queue_effect(move || {
            let js = create_element_in_head("meta", &props.attributes(), None);
            myself.put_in_head(js);
        });
    }

    /// Create a new script tag in the head
    fn create_script(&self, props: ScriptProps) {
        let myself = self.clone();
        queue_effect(move || {
            let js =
                create_element_in_head("script", &props.attributes(), props.script_contents().ok());
            myself.put_in_head(js);
        });
    }

    /// Create a new style tag in the head
    fn create_style(&self, props: StyleProps) {
        let myself = self.clone();
        queue_effect(move || {
            let js =
                create_element_in_head("style", &props.attributes(), props.style_contents().ok());
            myself.put_in_head(js);
        });
    }

    /// Create a new link tag in the head
    fn create_link(&self, props: LinkProps) {
        let myself = self.clone();
        queue_effect(move || {
            let js = create_element_in_head("link", &props.attributes(), None);
            myself.put_in_head(js);
        });
    }
}

/// Represents a desktop-target's JavaScript evaluator.
pub(crate) struct DesktopEvaluator {
    query: Query<serde_json::Value>,
}

impl DesktopEvaluator {
    /// Creates a new evaluator for desktop-based targets.
    pub fn create(desktop_ctx: DesktopContext, js: String) -> GenerationalBox<Box<dyn Evaluator>> {
        let query = desktop_ctx.query.new_query(&js, desktop_ctx.clone());
        Self::owned_by(&desktop_ctx.query, query)
    }

    fn owned_by(
        engine: &QueryEngine,
        query: Query<serde_json::Value>,
    ) -> GenerationalBox<Box<dyn Evaluator>> {
        // We create a generational box that is owned by the query slot so that when we drop the query slot, the generational box is also dropped.
        let owner = UnsyncStorage::owner();
        let query_id = query.id;
        let query = owner.insert(Box::new(DesktopEvaluator { query }) as Box<dyn Evaluator>);
        engine
            .active_requests
            .requests
            .borrow_mut()
            .get_mut(query_id)
            .expect("A query is registered as it is created.")
            .owner = Some(owner);

        query
    }
}

impl Evaluator for DesktopEvaluator {
    fn poll_join(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<serde_json::Value, EvalError>> {
        self.query
            .poll_result(cx)
            .map_err(|e| EvalError::Communication(e.to_string()))
    }

    /// Sends a message to the evaluated JavaScript.
    fn send(&self, data: serde_json::Value) -> Result<(), EvalError> {
        if let Err(e) = self.query.send(data) {
            return Err(EvalError::Communication(e.to_string()));
        }
        Ok(())
    }

    /// Gets an UnboundedReceiver to receive messages from the evaluated JavaScript.
    fn poll_recv(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<serde_json::Value, EvalError>> {
        self.query
            .poll_recv(cx)
            .map_err(|e| EvalError::Communication(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::{QueryPage, QueryResult};
    use futures_util::FutureExt;
    use serde_json::{Value, json};
    use std::task::{Context, Poll};

    fn evaluated(engine: &QueryEngine) -> (usize, Eval) {
        let query =
            engine.start::<Value>("return 7;", QueryPage::Drawn, std::rc::Weak::new(), |_| {
                Ok(())
            });
        let id = query.id;
        (id, Eval::new(DesktopEvaluator::owned_by(engine, query)))
    }

    fn told(engine: &QueryEngine, id: usize, data: Value) {
        engine.send(
            serde_json::from_value::<QueryResult>(json!({ "id": id, "data": data }))
                .expect("The page's report is well formed."),
        );
    }

    #[test]
    fn a_result_the_page_lets_go_of_before_the_waiting_task_runs_again_is_still_read() {
        let engine = QueryEngine::default();
        let (id, eval) = evaluated(&engine);
        let mut joining = eval.join::<i32>().boxed_local();
        let waker = futures_util::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        assert!(joining.poll_unpin(&mut cx).is_pending());
        told(&engine, id, json!({ "method": "return", "data": 7 }));
        told(&engine, id, json!({ "method": "drop" }));
        let joined = joining.poll_unpin(&mut cx);
        assert!(matches!(joined, Poll::Ready(Ok(7))), "{joined:?}");
    }

    #[test]
    fn an_evaluator_the_page_let_go_of_is_freed_once_the_pass_after_it_has_run() {
        let engine = QueryEngine::default();
        let (id, eval) = evaluated(&engine);
        told(&engine, id, json!({ "method": "return", "data": 7 }));
        told(&engine, id, json!({ "method": "drop" }));
        assert!(matches!(eval.send(1), Err(EvalError::Communication(_))));
        drop(engine.released_by_their_pages());
        assert!(matches!(eval.send(1), Err(EvalError::Finished)));
        let joined = eval.join::<i32>().now_or_never();
        assert!(
            matches!(joined, Some(Err(EvalError::Finished))),
            "{joined:?}"
        );
    }

    #[test]
    fn a_pass_that_ends_before_any_task_runs_keeps_what_the_page_let_go_of_for_the_next() {
        let engine = QueryEngine::default();
        let (id, eval) = evaluated(&engine);
        told(&engine, id, json!({ "method": "return", "data": 7 }));
        told(&engine, id, json!({ "method": "drop" }));
        engine.released_by_their_pages().wait_for_the_next_pass();
        let joined = eval.join::<i32>().now_or_never();
        assert!(matches!(joined, Some(Ok(7))), "{joined:?}");
        drop(engine.released_by_their_pages());
        assert!(matches!(eval.send(1), Err(EvalError::Finished)));
    }

    #[test]
    fn every_message_sent_before_the_page_let_go_is_received_and_then_the_channel_ends() {
        let engine = QueryEngine::default();
        let (id, mut eval) = evaluated(&engine);
        told(&engine, id, json!({ "method": "send", "data": 1 }));
        told(&engine, id, json!({ "method": "send", "data": 2 }));
        told(&engine, id, json!({ "method": "drop" }));
        assert!(matches!(eval.recv::<i32>().now_or_never(), Some(Ok(1))));
        assert!(matches!(eval.recv::<i32>().now_or_never(), Some(Ok(2))));
        let ended = eval.recv::<i32>().now_or_never();
        assert!(
            matches!(ended, Some(Err(EvalError::Communication(_)))),
            "{ended:?}"
        );
    }

    #[test]
    fn an_evaluator_the_page_still_holds_lives_through_every_pass() {
        let engine = QueryEngine::default();
        let (id, eval) = evaluated(&engine);
        drop(engine.released_by_their_pages());
        assert!(eval.join::<i32>().now_or_never().is_none());
        told(&engine, id, json!({ "method": "return", "data": 7 }));
        drop(engine.released_by_their_pages());
        assert!(matches!(eval.join::<i32>().now_or_never(), Some(Ok(7))));
    }

    #[test]
    fn a_report_after_the_page_let_go_is_ignored() {
        let engine = QueryEngine::default();
        let (id, mut eval) = evaluated(&engine);
        told(&engine, id, json!({ "method": "drop" }));
        told(&engine, id, json!({ "method": "send", "data": 1 }));
        told(&engine, id, json!({ "method": "return", "data": 7 }));
        assert!(matches!(
            eval.recv::<i32>().now_or_never(),
            Some(Err(EvalError::Communication(_)))
        ));
        assert!(matches!(
            eval.join::<i32>().now_or_never(),
            Some(Err(EvalError::Communication(_)))
        ));
        drop(engine.released_by_their_pages());
        assert!(matches!(
            eval.join::<i32>().now_or_never(),
            Some(Err(EvalError::Finished))
        ));
    }

    #[test]
    fn a_page_that_lets_go_without_returning_ends_the_join_rather_than_leaving_it_waiting() {
        let engine = QueryEngine::default();
        let (id, eval) = evaluated(&engine);
        told(&engine, id, json!({ "method": "drop" }));
        let joined = eval.join::<i32>().now_or_never();
        assert!(
            matches!(joined, Some(Err(EvalError::Communication(_)))),
            "{joined:?}"
        );
    }

    #[test]
    fn an_error_the_page_returned_before_it_let_go_is_still_read() {
        let engine = QueryEngine::default();
        let (id, eval) = evaluated(&engine);
        told(
            &engine,
            id,
            json!({ "method": "return_error", "data": "Error running JS: thrown" }),
        );
        told(&engine, id, json!({ "method": "drop" }));
        let joined = eval.join::<i32>().now_or_never();
        assert!(
            matches!(&joined, Some(Err(EvalError::Communication(reason))) if reason.contains("thrown")),
            "{joined:?}"
        );
    }
}

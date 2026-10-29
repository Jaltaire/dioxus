use crate::{DesktopContext, WeakDesktopContext};
use futures_util::{FutureExt, StreamExt};
use generational_box::Owner;
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::Value;
use std::{cell::RefCell, collections::HashMap, rc::Rc};
use thiserror::Error;

pub(crate) struct Requests<T> {
    next: usize,
    entries: HashMap<usize, T>,
}

impl<T> Default for Requests<T> {
    fn default() -> Self {
        Self {
            next: 0,
            entries: HashMap::new(),
        }
    }
}

impl<T> Requests<T> {
    pub fn insert(&mut self, entry: T) -> usize {
        let id = self.next;
        self.next += 1;
        self.entries.insert(id, entry);
        id
    }

    pub fn get_mut(&mut self, id: usize) -> Option<&mut T> {
        self.entries.get_mut(&id)
    }

    pub fn remove(&mut self, id: usize) -> Option<T> {
        self.entries.remove(&id)
    }
}

/// Tracks what query ids are currently active
pub(crate) struct SharedRequests<T> {
    pub requests: Rc<RefCell<Requests<T>>>,
}

impl<T> Clone for SharedRequests<T> {
    fn clone(&self) -> Self {
        Self {
            requests: self.requests.clone(),
        }
    }
}

impl<T> Default for SharedRequests<T> {
    fn default() -> Self {
        Self {
            requests: Rc::new(RefCell::new(Requests::default())),
        }
    }
}

pub(crate) struct QueryEntry {
    channel_sender: futures_channel::mpsc::UnboundedSender<Value>,
    return_sender: Option<futures_channel::oneshot::Sender<Result<Value, String>>>,
    pub owner: Option<Owner>,
}

/// Handles sending and receiving arbitrary queries from the webview. Queries can be resolved non-sequentially, so we use ids to track them.
#[derive(Clone, Default)]
pub(crate) struct QueryEngine {
    pub active_requests: SharedRequests<QueryEntry>,
}

/// Whether a query has a page to run in: one that has reported in, or none,
/// because the page was lost and its replacement has not reported in yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueryPage {
    Drawn,
    Awaited,
}

impl QueryPage {
    fn of(context: &DesktopContext) -> Self {
        if context
            .page_awaited
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            QueryPage::Awaited
        } else {
            QueryPage::Drawn
        }
    }
}

impl QueryEngine {
    /// Creates a new query and returns a handle to it. The query will be resolved when the webview returns a result with the same id.
    ///
    /// A query asked for while the page is lost and its replacement has not
    /// reported in is never sent. Its script was written for the page that is
    /// gone, and the page loading in its place has none of the scripts a
    /// query runs on yet, so it would only throw there. The query waits, as
    /// one that threw would have, and goes with the task that asked for it
    /// when the tree is rebuilt into the new page.
    pub fn new_query<V: DeserializeOwned>(
        &self,
        script: &str,
        context: DesktopContext,
    ) -> Query<V> {
        self.start(
            script,
            QueryPage::of(&context),
            Rc::downgrade(&context),
            |script| {
                context
                    .webview
                    .evaluate_script(script)
                    .map_err(|err| err.to_string())
            },
        )
    }

    fn start<V: DeserializeOwned>(
        &self,
        script: &str,
        page: QueryPage,
        desktop: WeakDesktopContext,
        evaluate: impl FnOnce(&str) -> Result<(), String>,
    ) -> Query<V> {
        let (tx, rx) = futures_channel::mpsc::unbounded();
        let (return_tx, return_rx) = futures_channel::oneshot::channel();
        let request_id = self
            .active_requests
            .requests
            .borrow_mut()
            .insert(QueryEntry {
                channel_sender: tx,
                return_sender: Some(return_tx),
                owner: None,
            });

        // start the query
        // We embed the return of the eval in a function so we can send it back to the main thread
        let script = format!(
            r#"(function(){{
                let dioxus = window.createQuery({request_id});
                let post_error = function(err) {{
                    let returned_value = {{
                        "method": "query",
                        "params": {{
                            "id": {request_id},
                            "data": {{
                                "data": err,
                                "method": "return_error"
                            }}
                        }}
                    }};
                    window.ipc.postMessage(
                        JSON.stringify(returned_value)
                    );
                }};
                try {{
                    const AsyncFunction = async function () {{}}.constructor;
                    let promise = (new AsyncFunction("dioxus", {script:?}))(dioxus);
                    promise
                        .then((result)=>{{
                            dioxus.close();
                            let returned_value = {{
                                "method": "query",
                                "params": {{
                                    "id": {request_id},
                                    "data": {{
                                        "data": result,
                                        "method": "return"
                                    }}
                                }}
                            }};
                            window.ipc.postMessage(
                                JSON.stringify(returned_value)
                            );
                        }})
                        .catch(err => post_error(`Error running JS: ${{err}}`));
                }} catch (error) {{
                    dioxus.close();
                    post_error(`Invalid JS: ${{error}}`);
                }}
            }})();"#
        );
        match page {
            QueryPage::Drawn => {
                if let Err(err) = evaluate(&script) {
                    tracing::warn!("Query error: {err}");
                }
            }
            QueryPage::Awaited => tracing::debug!(
                "Query {request_id} was asked for while the page is awaited, and is not sent to it."
            ),
        }

        Query {
            id: request_id,
            receiver: rx,
            return_receiver: Some(return_rx),
            desktop,
            phantom: std::marker::PhantomData,
        }
    }

    /// Send a query channel message to the correct query
    pub fn send(&self, data: QueryResult) {
        let QueryResult { id, data } = data;
        let mut requests = self.active_requests.requests.borrow_mut();
        if let Some(entry) = requests.get_mut(id) {
            match data {
                QueryResultData::Return { data } => {
                    if let Some(sender) = entry.return_sender.take() {
                        let _ = sender.send(Ok(data.unwrap_or_default()));
                    }
                }
                QueryResultData::ReturnError { data } => {
                    if let Some(sender) = entry.return_sender.take() {
                        let _ = sender.send(Err(data.to_string()));
                    }
                }
                QueryResultData::Drop => {
                    requests.remove(id);
                }
                QueryResultData::Send { data } => {
                    let _ = entry.channel_sender.unbounded_send(data);
                }
            }
        }
    }
}

pub(crate) struct Query<V: DeserializeOwned> {
    desktop: WeakDesktopContext,
    receiver: futures_channel::mpsc::UnboundedReceiver<Value>,
    return_receiver: Option<futures_channel::oneshot::Receiver<Result<Value, String>>>,
    pub id: usize,
    phantom: std::marker::PhantomData<V>,
}

impl<V: DeserializeOwned> Query<V> {
    /// Resolve the query
    pub async fn resolve(mut self) -> Result<V, QueryError> {
        let result = self.result().await?;
        V::deserialize(result).map_err(QueryError::Deserialize)
    }

    /// Send a message to the query
    pub fn send<S: ToString>(&self, message: S) -> Result<(), QueryError> {
        let desktop = self.desktop.upgrade().ok_or(QueryError::Finished)?;
        self.send_to(QueryPage::of(&desktop), message, |script| {
            desktop
                .webview
                .evaluate_script(script)
                .map_err(|e| e.to_string())
        })
    }

    /// Sends a message to the query in the page it runs in. While the page
    /// is awaited there is no such page: the one the query ran in is gone,
    /// and the one loading has never heard of it.
    fn send_to<S: ToString>(
        &self,
        page: QueryPage,
        message: S,
        evaluate: impl FnOnce(&str) -> Result<(), String>,
    ) -> Result<(), QueryError> {
        if page == QueryPage::Awaited {
            return Err(QueryError::Send(String::from(
                "The page the query ran in has been lost, and the page replacing it has not reported in.",
            )));
        }
        let queue_id = self.id;

        let data = message.to_string();
        let script = format!(r#"window.getQuery({queue_id}).rustSend({data});"#);

        evaluate(&script).map_err(QueryError::Send)?;

        Ok(())
    }

    /// Poll the query for a message
    pub fn poll_recv(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<Value, QueryError>> {
        self.receiver
            .poll_next_unpin(cx)
            .map(|result| result.ok_or(QueryError::Recv(String::from("Receive channel closed"))))
    }

    /// Receive the result of the query
    pub async fn result(&mut self) -> Result<Value, QueryError> {
        match self.return_receiver.take() {
            Some(receiver) => match receiver.await {
                Ok(Ok(data)) => Ok(data),
                Ok(Err(err)) => Err(QueryError::Recv(err)),
                Err(err) => Err(QueryError::Recv(err.to_string())),
            },
            None => Err(QueryError::Finished),
        }
    }

    /// Poll the query for a result
    pub fn poll_result(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<Value, QueryError>> {
        match self.return_receiver.as_mut() {
            Some(receiver) => receiver.poll_unpin(cx).map(|result| match result {
                Ok(Ok(data)) => Ok(data),
                Ok(Err(err)) => Err(QueryError::Recv(err)),
                Err(err) => Err(QueryError::Recv(err.to_string())),
            }),
            None => std::task::Poll::Ready(Err(QueryError::Finished)),
        }
    }
}

#[derive(Error, Debug)]
#[non_exhaustive]
pub enum QueryError {
    #[error("Error receiving query result: {0}")]
    Recv(String),
    #[error("Error sending message to query: {0}")]
    Send(String),
    #[error("Error deserializing query result: {0}")]
    Deserialize(serde_json::Error),
    #[error("Query has already been resolved")]
    Finished,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct QueryResult {
    id: usize,
    data: QueryResultData,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "method")]
enum QueryResultData {
    #[serde(rename = "return")]
    Return { data: Option<Value> },
    #[serde(rename = "return_error")]
    ReturnError { data: Value },
    #[serde(rename = "send")]
    Send { data: Value },
    #[serde(rename = "drop")]
    Drop,
}

#[cfg(test)]
mod page_tests {
    use super::*;
    use std::cell::Cell;

    fn started(page: QueryPage, evaluated: &RefCell<Vec<String>>) -> (QueryEngine, Query<Value>) {
        let engine = QueryEngine::default();
        let query = engine.start::<Value>("return 1;", page, std::rc::Weak::new(), |script| {
            evaluated.borrow_mut().push(script.to_owned());
            Ok(())
        });
        (engine, query)
    }

    #[test]
    fn a_query_asked_for_while_a_page_is_drawn_into_is_sent_to_it() {
        let evaluated = RefCell::new(Vec::new());
        let (engine, mut query) = started(QueryPage::Drawn, &evaluated);
        let evaluated = evaluated.into_inner();
        assert_eq!(evaluated.len(), 1);
        assert!(evaluated[0].contains(&format!("window.createQuery({})", query.id)));
        assert!(
            engine
                .active_requests
                .requests
                .borrow_mut()
                .get_mut(query.id)
                .is_some()
        );
        assert!(
            query.result().now_or_never().is_none(),
            "The query waits for the page."
        );
    }

    #[test]
    fn a_query_asked_for_while_the_page_is_awaited_is_never_sent_and_waits() {
        let evaluated = RefCell::new(Vec::new());
        let (engine, mut query) = started(QueryPage::Awaited, &evaluated);
        assert!(evaluated.into_inner().is_empty());
        assert!(
            engine
                .active_requests
                .requests
                .borrow_mut()
                .get_mut(query.id)
                .is_some(),
            "The query is kept for its evaluator, as one sent would be."
        );
        assert!(query.result().now_or_never().is_none());
    }

    #[test]
    fn a_message_for_a_query_is_sent_to_the_page_drawn_into() {
        let evaluated = RefCell::new(Vec::new());
        let (_engine, query) = started(QueryPage::Drawn, &evaluated);
        let sent = Cell::new(None);
        assert!(
            query
                .send_to(QueryPage::Drawn, "7", |script| {
                    sent.set(Some(script.to_owned()));
                    Ok(())
                })
                .is_ok()
        );
        assert_eq!(
            sent.into_inner(),
            Some(format!("window.getQuery({}).rustSend(7);", query.id))
        );
    }

    #[test]
    fn a_message_for_a_query_while_the_page_is_awaited_is_refused_and_never_sent() {
        let evaluated = RefCell::new(Vec::new());
        let (_engine, query) = started(QueryPage::Drawn, &evaluated);
        let sent = Cell::new(false);
        let refused = query.send_to(QueryPage::Awaited, "7", |_| {
            sent.set(true);
            Ok(())
        });
        assert!(matches!(refused, Err(QueryError::Send(_))));
        assert!(!sent.get());
    }

    #[test]
    fn a_message_the_page_cannot_take_is_reported() {
        let evaluated = RefCell::new(Vec::new());
        let (_engine, query) = started(QueryPage::Drawn, &evaluated);
        let failed = query.send_to(QueryPage::Drawn, "7", |_| Err(String::from("gone")));
        assert!(matches!(failed, Err(QueryError::Send(reason)) if reason == "gone"));
    }

    #[test]
    fn a_query_whose_script_cannot_be_evaluated_still_waits() {
        let engine = QueryEngine::default();
        let mut query =
            engine.start::<Value>("return 1;", QueryPage::Drawn, std::rc::Weak::new(), |_| {
                Err(String::from("gone"))
            });
        assert!(query.result().now_or_never().is_none());
    }
}

#[cfg(test)]
mod tests {
    use super::Requests;

    #[test]
    fn an_id_is_never_handed_out_twice() {
        let mut requests = Requests::default();
        let first = requests.insert("first");
        assert_eq!(requests.remove(first), Some("first"));
        let second = requests.insert("second");
        assert_ne!(first, second);
    }

    #[test]
    fn a_late_report_for_a_finished_query_leaves_the_next_one_running() {
        let mut requests = Requests::default();
        let finished = requests.insert("finished");
        requests.remove(finished);
        let running = requests.insert("running");
        assert_eq!(requests.remove(finished), None);
        assert_eq!(requests.get_mut(running), Some(&mut "running"));
    }

    #[test]
    fn an_entry_is_found_until_it_is_removed() {
        let mut requests = Requests::default();
        let id = requests.insert(1);
        *requests.get_mut(id).expect("The entry was just inserted.") += 1;
        assert_eq!(requests.remove(id), Some(2));
        assert_eq!(requests.get_mut(id), None);
    }
}

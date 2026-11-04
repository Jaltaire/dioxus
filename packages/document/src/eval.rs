#![doc = include_str!("../docs/eval.md")]

use crate::error::EvalError;
use generational_box::{BorrowError, BorrowMutError, GenerationalBox};
use std::future::{Future, IntoFuture, poll_fn};
use std::pin::Pin;
use std::task::{Context, Poll};

#[doc = include_str!("../docs/eval.md")]
pub struct Eval {
    evaluator: GenerationalBox<Box<dyn Evaluator>>,
}

impl Eval {
    /// Create this eval from a dynamic evaluator
    pub fn new(evaluator: GenerationalBox<Box<dyn Evaluator + 'static>>) -> Self {
        Self { evaluator }
    }

    /// Wait until the javascript task is finished and return the result
    pub async fn join<T: serde::de::DeserializeOwned>(self) -> Result<T, EvalError> {
        let json_value = poll_fn(|cx| match self.evaluator.try_write() {
            Ok(mut evaluator) => evaluator.poll_join(cx),
            Err(error) => unavailable(error, cx),
        })
        .await?;
        serde_json::from_value(json_value).map_err(EvalError::Serialization)
    }

    /// Send a message to the javascript task
    pub fn send(&self, data: impl serde::Serialize) -> Result<(), EvalError> {
        match self.evaluator.try_read() {
            Ok(evaluator) => {
                evaluator.send(serde_json::to_value(data).map_err(EvalError::Serialization)?)
            }
            Err(BorrowError::Dropped(_)) => Err(EvalError::Finished),
            Err(BorrowError::AlreadyBorrowedMut(_)) => Err(EvalError::Communication(String::from(
                "The evaluator is in use by a join or a receive that is being polled.",
            ))),
        }
    }

    /// Receive a message from the javascript task
    pub async fn recv<T: serde::de::DeserializeOwned>(&mut self) -> Result<T, EvalError> {
        let json_value = poll_fn(|cx| match self.evaluator.try_write() {
            Ok(mut evaluator) => evaluator.poll_recv(cx),
            Err(error) => unavailable(error, cx),
        })
        .await?;
        serde_json::from_value(json_value).map_err(EvalError::Serialization)
    }
}

fn unavailable(
    error: BorrowMutError,
    cx: &mut Context<'_>,
) -> Poll<Result<serde_json::Value, EvalError>> {
    match error {
        BorrowMutError::Dropped(_) => Poll::Ready(Err(EvalError::Finished)),
        BorrowMutError::AlreadyBorrowed(_) | BorrowMutError::AlreadyBorrowedMut(_) => {
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
}

impl IntoFuture for Eval {
    type Output = Result<serde_json::Value, EvalError>;
    type IntoFuture = Pin<Box<dyn Future<Output = Self::Output>>>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(self.join().into_future())
    }
}

impl Clone for Eval {
    fn clone(&self) -> Self {
        *self
    }
}

impl Copy for Eval {}

/// The platform's evaluator.
pub trait Evaluator {
    /// Sends a message to the evaluated JavaScript.
    fn send(&self, data: serde_json::Value) -> Result<(), EvalError>;
    /// Receive any queued messages from the evaluated JavaScript.
    fn poll_recv(
        &mut self,
        context: &mut Context<'_>,
    ) -> Poll<Result<serde_json::Value, EvalError>>;
    /// Gets the return value of the JavaScript
    fn poll_join(
        &mut self,
        context: &mut Context<'_>,
    ) -> Poll<Result<serde_json::Value, EvalError>>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::FutureExt;
    use generational_box::{AnyStorage, Owner, UnsyncStorage};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Wake, Waker};

    struct Answering;

    impl Evaluator for Answering {
        fn send(&self, _data: serde_json::Value) -> Result<(), EvalError> {
            Ok(())
        }

        fn poll_recv(
            &mut self,
            _context: &mut Context<'_>,
        ) -> Poll<Result<serde_json::Value, EvalError>> {
            Poll::Ready(Ok(serde_json::json!(2)))
        }

        fn poll_join(
            &mut self,
            _context: &mut Context<'_>,
        ) -> Poll<Result<serde_json::Value, EvalError>> {
            Poll::Ready(Ok(serde_json::json!(1)))
        }
    }

    #[derive(Default)]
    struct Wakes(AtomicUsize);

    impl Wake for Wakes {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn evaluator() -> (Owner<UnsyncStorage>, GenerationalBox<Box<dyn Evaluator>>) {
        let owner = UnsyncStorage::owner();
        let evaluator = owner.insert(Box::new(Answering) as Box<dyn Evaluator>);
        (owner, evaluator)
    }

    fn polled<T>(
        future: impl std::future::Future<Output = Result<T, EvalError>>,
    ) -> (Poll<Result<T, EvalError>>, usize) {
        let wakes = Arc::new(Wakes::default());
        let waker = Waker::from(wakes.clone());
        let mut context = Context::from_waker(&waker);
        let polled = Box::pin(future).poll_unpin(&mut context);
        (polled, wakes.0.load(Ordering::SeqCst))
    }

    #[test]
    fn a_living_evaluator_is_joined_received_from_and_sent_to() {
        let (_owner, evaluator) = evaluator();
        let mut eval = Eval::new(evaluator);
        assert!(matches!(
            polled(eval.join::<i32>()),
            (Poll::Ready(Ok(1)), 0)
        ));
        assert!(matches!(
            polled(eval.recv::<i32>()),
            (Poll::Ready(Ok(2)), 0)
        ));
        assert!(eval.send(3).is_ok());
    }

    #[test]
    fn a_busy_evaluator_is_waited_for_rather_than_called_finished() {
        let (_owner, evaluator) = evaluator();
        let mut eval = Eval::new(evaluator);
        {
            let _reading = evaluator.try_read().expect("The evaluator is alive.");
            assert!(matches!(polled(eval.join::<i32>()), (Poll::Pending, 1)));
            assert!(matches!(polled(eval.recv::<i32>()), (Poll::Pending, 1)));
        }
        {
            let _writing = evaluator.try_write().expect("The evaluator is alive.");
            assert!(matches!(polled(eval.join::<i32>()), (Poll::Pending, 1)));
            assert!(matches!(polled(eval.recv::<i32>()), (Poll::Pending, 1)));
            assert!(matches!(eval.send(3), Err(EvalError::Communication(_))));
        }
        assert!(matches!(
            polled(eval.join::<i32>()),
            (Poll::Ready(Ok(1)), 0)
        ));
    }

    #[test]
    fn a_dropped_evaluator_is_finished() {
        let (owner, evaluator) = evaluator();
        let mut eval = Eval::new(evaluator);
        drop(owner);
        assert!(matches!(
            polled(eval.join::<i32>()),
            (Poll::Ready(Err(EvalError::Finished)), 0)
        ));
        assert!(matches!(
            polled(eval.recv::<i32>()),
            (Poll::Ready(Err(EvalError::Finished)), 0)
        ));
        assert!(matches!(eval.send(3), Err(EvalError::Finished)));
    }
}

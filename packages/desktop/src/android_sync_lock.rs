/// This is a hack to get around the fact that wry is currently not thread safe on android
///
/// We want to acquire this mutex before doing anything with the virtualdom directly, and before
/// touching anything else the event loop and the webview's request threads both reach. It is
/// held by the event loop for the whole of every event, and by a request thread for the whole of
/// every request, so the two never run at once. A thread that holds it may take it again.
pub fn android_runtime_lock() -> RuntimeLock {
    #[cfg(any(target_os = "android", test))]
    {
        static RUNTIME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let depth = HELD.get();
        let held = (depth == 0).then(|| {
            #[cfg(test)]
            let _waiting = Waiting::start();
            RUNTIME_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        });
        HELD.set(depth + 1);
        RuntimeLock { _held: held }
    }

    #[cfg(not(any(target_os = "android", test)))]
    RuntimeLock {}
}

pub(crate) struct RuntimeLock {
    #[cfg(any(target_os = "android", test))]
    _held: Option<std::sync::MutexGuard<'static, ()>>,
}

impl Drop for RuntimeLock {
    fn drop(&mut self) {
        #[cfg(any(target_os = "android", test))]
        HELD.set(HELD.get() - 1);
    }
}

#[cfg(any(target_os = "android", test))]
thread_local! {
    static HELD: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn held_by_this_thread() -> bool {
    HELD.get() > 0
}

#[cfg(test)]
static WAITING: std::sync::Mutex<Vec<std::thread::ThreadId>> = std::sync::Mutex::new(Vec::new());

#[cfg(test)]
struct Waiting(std::thread::ThreadId);

#[cfg(test)]
impl Waiting {
    fn start() -> Self {
        let thread = std::thread::current().id();
        WAITING
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(thread);
        Self(thread)
    }
}

#[cfg(test)]
impl Drop for Waiting {
    fn drop(&mut self) {
        WAITING
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|waiting| *waiting != self.0);
    }
}

#[cfg(test)]
pub(crate) fn is_waiting(thread: std::thread::ThreadId) -> bool {
    WAITING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .contains(&thread)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;

    #[test]
    fn a_thread_that_holds_the_lock_takes_it_again_and_keeps_it_until_the_outermost_lets_go() {
        assert!(!held_by_this_thread());
        let outer = android_runtime_lock();
        let inner = android_runtime_lock();
        assert!(held_by_this_thread());
        drop(inner);
        assert!(held_by_this_thread(), "The outer hold still stands.");

        let taken = Arc::new(AtomicBool::new(false));
        let (asking, asked) = mpsc::channel();
        let other = std::thread::spawn({
            let taken = taken.clone();
            move || {
                asking.send(()).unwrap();
                let _lock = android_runtime_lock();
                taken.store(true, Ordering::SeqCst);
            }
        });
        asked.recv().unwrap();
        let asker = other.thread().id();
        while !is_waiting(asker) {
            std::thread::yield_now();
        }
        assert!(
            !taken.load(Ordering::SeqCst),
            "Another thread cannot take the lock while this one holds it."
        );
        drop(outer);
        assert!(!held_by_this_thread());
        other.join().unwrap();
        assert!(taken.load(Ordering::SeqCst));
    }

    #[test]
    fn a_thread_that_panicked_holding_the_lock_does_not_take_the_next_holder_down_with_it() {
        let panicked = std::thread::spawn(|| {
            let _lock = android_runtime_lock();
            panic!("A request failed while holding the lock.");
        })
        .join();
        assert!(panicked.is_err());
        let _lock = android_runtime_lock();
        assert!(held_by_this_thread());
    }
}

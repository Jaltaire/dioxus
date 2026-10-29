use std::ffi::c_void;
use std::sync::OnceLock;

/// The context `ndk_context` hands every consumer: the application's own, published once and
/// held for as long as the process lives.
///
/// Android destroys an activity and creates another in the same process whenever it is asked to
/// show a configuration change the activity does not handle itself, and the reference to the old
/// activity is deleted with it. `ndk_context` keeps its context in an unsynchronized static that
/// can only be cleared and set again, never replaced, so a context published for each activity
/// leaves a moment in which a consumer on another thread finds none and panics, and a consumer
/// still holding the previous activity's reference aborts once it is deleted. The application
/// outlives every activity, so its context is published once, never cleared, and never deleted.
/// A consumer that needs an activity rather than a context has to find the current one itself.
pub(crate) struct ApplicationContext<H> {
    held: OnceLock<H>,
}

impl<H> ApplicationContext<H> {
    /// Nothing published yet.
    pub(crate) const fn new() -> Self {
        Self {
            held: OnceLock::new(),
        }
    }

    /// Publishes the application's context as the one every `ndk_context` consumer is handed, the
    /// first time it is called, from the reference `application` makes, through `object`, and
    /// holds that reference for good. Every later call leaves `ndk_context` as it is and makes no
    /// reference.
    ///
    /// # Safety
    ///
    /// `java_vm` is the process's `JavaVM`, `object(&held)` is a JNI reference to the application's
    /// context that stays valid for as long as `held` lives, and nothing but this publishes an
    /// `ndk_context`.
    pub(crate) unsafe fn publish(
        &self,
        java_vm: *mut c_void,
        application: impl FnOnce() -> H,
        object: impl FnOnce(&H) -> *mut c_void,
    ) -> &H {
        self.held.get_or_init(|| {
            let held = application();
            unsafe { ndk_context::initialize_android_context(java_vm, object(&held)) };
            held
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    struct Application {
        object: Box<u8>,
        let_go: Arc<AtomicUsize>,
    }

    impl Application {
        fn new(let_go: &Arc<AtomicUsize>) -> Self {
            Self {
                object: Box::new(0),
                let_go: Arc::clone(let_go),
            }
        }

        fn object(&self) -> *mut c_void {
            std::ptr::from_ref::<u8>(&self.object).cast_mut().cast()
        }
    }

    impl Drop for Application {
        fn drop(&mut self) {
            self.let_go.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn the_application_is_published_once_and_every_consumer_finds_it_for_good() {
        let context: &'static ApplicationContext<Application> =
            Box::leak(Box::new(ApplicationContext::new()));
        let java_vm = std::ptr::without_provenance_mut::<c_void>(0x1000);
        let let_go = Arc::new(AtomicUsize::new(0));
        let made = Arc::new(AtomicUsize::new(0));

        let make = |made: &Arc<AtomicUsize>| {
            made.fetch_add(1, Ordering::SeqCst);
            Application::new(&let_go)
        };
        let published = unsafe { context.publish(java_vm, || make(&made), Application::object) };
        let object = published.object();
        assert_eq!(ndk_context::android_context().context(), object);
        assert_eq!(ndk_context::android_context().vm(), java_vm);

        let reading = Arc::new(AtomicBool::new(true));
        let readers: Vec<_> = (0..4)
            .map(|_| {
                let reading = Arc::clone(&reading);
                let object = object as usize;
                std::thread::spawn(move || {
                    let mut reads = 0usize;
                    while reading.load(Ordering::SeqCst) || reads == 0 {
                        assert_eq!(ndk_context::android_context().context() as usize, object);
                        reads += 1;
                    }
                    reads
                })
            })
            .collect();
        for _ in 0..1000 {
            let again = unsafe { context.publish(java_vm, || make(&made), Application::object) };
            assert_eq!(
                again.object(),
                object,
                "An activity created again finds the application already published."
            );
        }
        reading.store(false, Ordering::SeqCst);
        for reader in readers {
            assert!(
                reader
                    .join()
                    .expect("No consumer ever found the context missing.")
                    > 0,
                "Every consumer read the context while activities were created again."
            );
        }

        assert_eq!(
            made.load(Ordering::SeqCst),
            1,
            "Only the first creation makes a reference."
        );
        assert_eq!(
            let_go.load(Ordering::SeqCst),
            0,
            "The application's reference is never let go."
        );
        assert_eq!(ndk_context::android_context().context(), object);
    }
}

use std::ffi::c_void;
use std::sync::{Mutex, PoisonError};

/// The activity `ndk_context` hands every consumer, kept current across the activity being
/// created again.
///
/// Android destroys an activity and creates another in the same process whenever it is asked to
/// show a configuration change the activity does not handle itself. Each creation runs the
/// application's setup again with a new activity, and the reference to the old one is deleted
/// with it. A context published once, at the first creation, would then name a deleted global
/// reference, and the next JNI call through it aborts the process. Each creation therefore
/// publishes its own activity, held here until the next one replaces it.
pub(crate) struct CurrentContext<H> {
    held: Mutex<Option<H>>,
}

impl<H> CurrentContext<H> {
    /// Nothing published yet.
    pub(crate) const fn new() -> Self {
        Self {
            held: Mutex::new(None),
        }
    }

    /// Publishes the activity `held` refers to, through `object`, as the context every
    /// `ndk_context` consumer is handed, and keeps `held` alive until the next activity takes its
    /// place. The activity published before is let go only once `ndk_context` names the new one.
    ///
    /// # Safety
    ///
    /// `java_vm` is the process's `JavaVM`, `object(&held)` is a JNI reference that stays valid
    /// for as long as `held` lives, and nothing but this publishes an `ndk_context`.
    pub(crate) unsafe fn publish(
        &self,
        java_vm: *mut c_void,
        held: H,
        object: impl FnOnce(&H) -> *mut c_void,
    ) {
        let mut current = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        let context = object(&held);
        unsafe {
            if current.is_some() {
                ndk_context::release_android_context();
            }
            ndk_context::initialize_android_context(java_vm, context);
        }
        *current = Some(held);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Activity {
        object: Box<u8>,
        let_go: Arc<AtomicUsize>,
    }

    impl Activity {
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

    impl Drop for Activity {
        fn drop(&mut self) {
            self.let_go.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn every_activity_created_is_the_one_consumers_are_handed_until_the_next() {
        let current = CurrentContext::new();
        let java_vm = std::ptr::without_provenance_mut::<c_void>(0x1000);
        let let_go = Arc::new(AtomicUsize::new(0));

        let first = Activity::new(&let_go);
        let first_object = first.object();
        unsafe { current.publish(java_vm, first, Activity::object) };
        assert_eq!(ndk_context::android_context().context(), first_object);
        assert_eq!(ndk_context::android_context().vm(), java_vm);
        assert_eq!(let_go.load(Ordering::SeqCst), 0, "The first is still held.");

        let second = Activity::new(&let_go);
        let second_object = second.object();
        assert_ne!(first_object, second_object);
        unsafe { current.publish(java_vm, second, Activity::object) };
        assert_eq!(
            ndk_context::android_context().context(),
            second_object,
            "The activity created again is the one handed out, not the deleted one."
        );
        assert_eq!(
            let_go.load(Ordering::SeqCst),
            1,
            "The first is let go once the second has taken its place."
        );

        let third = Activity::new(&let_go);
        let third_object = third.object();
        unsafe { current.publish(java_vm, third, Activity::object) };
        assert_eq!(ndk_context::android_context().context(), third_object);
        assert_eq!(let_go.load(Ordering::SeqCst), 2);
    }
}

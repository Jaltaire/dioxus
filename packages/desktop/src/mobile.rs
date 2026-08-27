#[cfg(any(target_os = "android", test))]
#[derive(Debug)]
struct StartupBarrier {
    ready: std::sync::Mutex<bool>,
    condition: std::sync::Condvar,
}

#[cfg(any(target_os = "android", test))]
impl StartupBarrier {
    const fn new() -> Self {
        Self {
            ready: std::sync::Mutex::new(false),
            condition: std::sync::Condvar::new(),
        }
    }

    fn mark_ready(&self) {
        let mut ready = self.ready.lock().unwrap();
        *ready = true;
        self.condition.notify_all();
    }

    fn wait(&self) {
        let ready = self.ready.lock().unwrap();
        drop(self.condition.wait_while(ready, |ready| !*ready).unwrap());
    }
}

#[cfg(target_os = "android")]
static ANDROID_CONTEXT_READY: StartupBarrier = StartupBarrier::new();

/// Expose the `Java_dev_dioxus_main_Rust_*` JNI trampolines that wry's Kotlin layer calls into.
/// We hardcode the package to `dev.dioxus.main` so host Java/Kotlin always has a single set of
/// symbols to bind against, without having to plumb the top-level package name down into this crate.
///
/// As of wry 0.55 the Kotlin lifecycle methods (create/start/stop/...) live on a `Rust` object
/// rather than `WryActivity`'s companion object, so the third arg to `tao::android_binding!` is
/// `Rust` — passing `WryActivity` would emit the wrong JNI symbol names and crash at startup with
/// `UnsatisfiedLinkError`.
///
/// The CLI is expecting to find `dev.dioxus.main` in the final library. If you find a need to
/// change this, you'll need to change the CLI as well.
#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn start_app() {
    use crate::Config;
    use dioxus_core::{Element, VirtualDom};
    use std::any::Any;

    // tao 0.35 dropped its automatic `ndk_context::initialize_android_context` call
    // (see https://github.com/tauri-apps/tao/issues/1220). Many android-aware crates —
    // including parts of wry itself — call `ndk_context::android_context()` and panic if
    // it's uninitialized, which then poisons wry's static mutexes and turns the original
    // panic into a confusing `PoisonError` at the next JNI callback. Initialize it here
    // before handing off to wry's own setup.
    //
    // Guarded by `Once` because `WryActivity.onCreate` (and therefore this setup) runs
    // again on activity re-creation — rotation, theme changes, back/foreground cycles —
    // and `ndk_context::initialize_android_context` asserts `previous.is_none()`, which
    // would abort the process on every re-entry. The global only needs the JavaVM + an
    // activity-like Context pointer for consumers to attach a JNI thread; we don't need
    // to refresh it per-activity.
    unsafe fn android_setup(
        package: &str,
        env: ::wry::prelude::JNIEnv<'_>,
        looper: &::ndk::looper::ThreadLooper,
        activity: ::wry::prelude::GlobalRef,
    ) {
        static NDK_CONTEXT_INIT: std::sync::Once = std::sync::Once::new();
        NDK_CONTEXT_INIT.call_once(|| {
            let vm = env.get_java_vm().unwrap();
            unsafe {
                ::ndk_context::initialize_android_context(
                    vm.get_java_vm_pointer() as *mut _,
                    activity.as_obj().as_raw() as *mut _,
                );
            }
        });
        ANDROID_CONTEXT_READY.mark_ready();
        unsafe {
            wry::android_setup(package, env, looper, activity);
        }
    }

    tao::android_binding!(dev_dioxus, main, Rust, android_setup, root, tao);
    wry::android_binding!(dev_dioxus, main, wry);

    #[cfg(target_os = "android")]
    fn root() {
        fn stop_unwind<F: FnOnce() -> T, T>(f: F) -> T {
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
                Ok(t) => t,
                Err(err) => {
                    eprintln!("attempt to unwind out of `rust` with err: {:?}", err);
                    std::process::abort()
                }
            }
        }

        stop_unwind(|| unsafe {
            ANDROID_CONTEXT_READY.wait();
            let mut main_fn_ptr = libc::dlsym(libc::RTLD_DEFAULT, b"main\0".as_ptr() as _);

            if main_fn_ptr.is_null() {
                main_fn_ptr = libc::dlsym(libc::RTLD_DEFAULT, b"_main\0".as_ptr() as _);
            }

            if main_fn_ptr.is_null() {
                panic!("Failed to find main symbol");
            }

            // Set the env vars that rust code might expect, passed off to us by the android app
            // Doing this before main emulates the behavior of a regular executable
            if cfg!(target_os = "android") && cfg!(debug_assertions) {
                // Load the env file from the session cache if we're in debug mode and on android
                //
                // This is a slightly hacky way of being able to use std::env::var code in android apps without
                // going through their custom java-based system.
                let env_file = dioxus_cli_config::android_session_cache_dir().join(".env");
                if let Ok(env_file) = std::fs::read_to_string(&env_file) {
                    for line in env_file.lines() {
                        if let Some((key, value)) = line.trim().split_once('=') {
                            std::env::set_var(key, value);
                        }
                    }
                }
            }

            let main_fn: extern "C" fn() = std::mem::transmute(main_fn_ptr);
            main_fn();
        });
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::StartupBarrier;

    #[test]
    fn startup_waits_for_platform_context_and_remains_ready() {
        let barrier = Arc::new(StartupBarrier::new());
        let waiter_barrier = barrier.clone();
        let (send, receive) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || {
            waiter_barrier.wait();
            send.send(()).unwrap();
        });

        assert!(receive.recv_timeout(Duration::from_millis(25)).is_err());
        barrier.mark_ready();
        receive.recv_timeout(Duration::from_secs(1)).unwrap();
        waiter.join().unwrap();
        barrier.wait();
    }

    #[test]
    fn marking_platform_context_ready_is_idempotent() {
        let barrier = StartupBarrier::new();
        barrier.mark_ready();
        barrier.mark_ready();
        barrier.wait();
    }
}

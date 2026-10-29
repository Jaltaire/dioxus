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
    // `WryActivity.onCreate` (and therefore this setup) runs again on activity re-creation —
    // rotation, theme changes, back/foreground cycles — and the previous activity's reference is
    // deleted with it. The context published is therefore the application's, which outlives every
    // activity: it is published once, from a reference held for the life of the process, and never
    // cleared, so no consumer can find it missing or deleted. Each activity created is recorded
    // with manganis, which holds it weakly and hands it out only while it has not been destroyed.
    unsafe fn android_setup(
        package: &str,
        mut env: ::wry::prelude::JNIEnv<'_>,
        looper: &::ndk::looper::ThreadLooper,
        activity: ::wry::prelude::GlobalRef,
    ) {
        static APPLICATION: crate::android_context::ApplicationContext<::wry::prelude::GlobalRef> =
            crate::android_context::ApplicationContext::new();
        let vm = env.get_java_vm().unwrap();
        unsafe {
            APPLICATION.publish(
                vm.get_java_vm_pointer() as *mut _,
                || {
                    let application = env
                        .call_method(
                            activity.as_obj(),
                            "getApplicationContext",
                            "()Landroid/content/Context;",
                            &[],
                        )
                        .unwrap()
                        .l()
                        .unwrap();
                    env.new_global_ref(application).unwrap()
                },
                |held| held.as_obj().as_raw() as *mut _,
            );
        }
        if let Err(error) = manganis::android::activity_created(&env, activity.as_obj()) {
            tracing::warn!("The activity created could not be recorded for manganis: {error}");
        }
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

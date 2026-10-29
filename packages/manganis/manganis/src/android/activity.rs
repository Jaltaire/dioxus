use jni::{
    JNIEnv, JavaVM,
    objects::{JObject, WeakRef},
};
use std::sync::{Mutex, OnceLock, PoisonError};

static JAVA_VM: OnceLock<JavaVM> = OnceLock::new();

/// How many local references a call is expected to make, in the frame
/// that frees every one of them when the call returns.
const LOCAL_REFERENCES: i32 = 16;

/// The activity created last, held weakly, so that it is never kept alive
/// past its destruction and never referred to once it has been collected.
static CREATED: Mutex<Option<WeakRef>> = Mutex::new(None);

/// Records `activity` as the activity created last, in place of the one
/// before it.
///
/// A renderer whose `ndk_context` holds a context that is not an activity
/// (`dioxus-desktop` publishes the application's own) calls this each time
/// Android creates an activity, before anything is asked of it, so that
/// [`with_activity`] hands out the activity alive at the time.
pub fn activity_created(env: &JNIEnv<'_>, activity: &JObject<'_>) -> jni::errors::Result<()> {
    let held = env.new_weak_ref(activity)?;
    *CREATED.lock().unwrap_or_else(PoisonError::into_inner) = held;
    Ok(())
}

/// Execute a JNI operation with the activity alive at the time.
///
/// The activity is the one a renderer recorded as created last (see
/// [`activity_created`]), or, where none was ever recorded, the context
/// `ndk_context` holds if that is an activity. It is handed to `f` as a
/// reference of the call's own, and never once it has been destroyed.
/// Every local reference the call makes is freed when it returns, so a
/// thread that stays attached does not gather them.
///
/// # Arguments
///
/// * `f` - A closure that receives a mutable JNIEnv and the Activity JObject
///
/// # Returns
///
/// Returns `Some(R)` if the operation succeeds, `None` if there's an error
/// getting the VM or Activity references, or if no activity is alive.
///
/// # Example
///
/// ```rust,no_run
/// use dioxus_platform_bridge::android::with_activity;
///
/// let result = with_activity(|env, activity| {
///     // Your JNI operations here
///     Some(42)
/// });
/// ```
pub fn with_activity<F, R>(f: F) -> Option<R>
where
    F: FnOnce(&mut JNIEnv<'_>, &JObject<'_>) -> Option<R>,
{
    let ctx = ndk_context::android_context();
    let mut env = java_vm(ctx)?.attach_current_thread().ok()?;
    env.with_local_frame(LOCAL_REFERENCES, |env| -> jni::errors::Result<Option<R>> {
        Ok(alive(env, ctx).and_then(|activity| f(env, &activity)))
    })
    .ok()
    .flatten()
}

/// Execute a JNI operation with the context `ndk_context` holds, which is
/// the application's own under `dioxus-desktop` and the activity's under
/// the `android-activity` glue, for work that needs a context and not an
/// activity, and can be done while no activity is alive. Every local
/// reference the call makes is freed when it returns.
///
/// Returns `None` if there's an error getting the VM, or if no context has
/// been published.
pub fn with_context<F, R>(f: F) -> Option<R>
where
    F: FnOnce(&mut JNIEnv<'_>, &JObject<'_>) -> Option<R>,
{
    let ctx = ndk_context::android_context();
    let mut env = java_vm(ctx)?.attach_current_thread().ok()?;
    let context = unsafe { JObject::from_raw(ctx.context() as jni::sys::jobject) };
    if context.is_null() {
        return None;
    }
    env.with_local_frame(LOCAL_REFERENCES, |env| -> jni::errors::Result<Option<R>> {
        Ok(f(env, &context))
    })
    .ok()
    .flatten()
}

fn java_vm(ctx: ndk_context::AndroidContext) -> Option<&'static JavaVM> {
    if let Some(vm) = JAVA_VM.get() {
        return Some(vm);
    }
    let raw_vm = unsafe { JavaVM::from_raw(ctx.vm().cast()) }.ok()?;
    let _ = JAVA_VM.set(raw_vm);
    JAVA_VM.get()
}

/// `result`'s value, or nothing where it failed, with any exception the
/// failure left pending cleared.
fn cleared<T>(env: &mut JNIEnv<'_>, result: jni::errors::Result<T>) -> Option<T> {
    if result.is_err() && env.exception_check().unwrap_or(false) {
        let _ = env.exception_clear();
    }
    result.ok()
}

/// The activity alive now, as a reference of the caller's own.
fn alive<'local>(
    env: &mut JNIEnv<'local>,
    ctx: ndk_context::AndroidContext,
) -> Option<JObject<'local>> {
    let created = CREATED.lock().unwrap_or_else(PoisonError::into_inner);
    let activity = match created.as_ref() {
        Some(created) => {
            let upgraded = created.upgrade_local(env);
            cleared(env, upgraded)??
        }
        None => {
            let published = unsafe { JObject::from_raw(ctx.context() as jni::sys::jobject) };
            if published.is_null() {
                return None;
            }
            let is_activity = env.is_instance_of(&published, "android/app/Activity");
            if !cleared(env, is_activity)? {
                return None;
            }
            let local = env.new_local_ref(&published);
            cleared(env, local)?
        }
    };
    drop(created);
    let destroyed = env
        .call_method(&activity, "isDestroyed", "()Z", &[])
        .and_then(|destroyed| destroyed.z());
    match cleared(env, destroyed)? {
        true => None,
        false => Some(activity),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jni::{InitArgsBuilder, JNIVersion, objects::GlobalRef};
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::MutexGuard;

    /// Stand-ins for the Android classes these calls reach, each as its
    /// package path and its source.
    const STAND_INS: [(&str, &str); 3] = [
        (
            "android/content/Context",
            "package android.content;\n\npublic class Context {}\n",
        ),
        (
            "android/app/Application",
            "package android.app;\n\npublic class Application extends android.content.Context {}\n",
        ),
        (
            "android/app/Activity",
            "package android.app;\n\npublic class Activity extends android.content.Context {\n    private boolean destroyed;\n\n    public final boolean isDestroyed() {\n        return destroyed;\n    }\n\n    public final void destroy() {\n        destroyed = true;\n    }\n}\n",
        ),
    ];

    struct Machine {
        vm: JavaVM,
        application: GlobalRef,
    }

    fn javac() -> PathBuf {
        match std::env::var_os("JAVA_HOME") {
            Some(home) => PathBuf::from(home).join("bin/javac"),
            None => PathBuf::from("javac"),
        }
    }

    fn compile(directory: &Path) -> Result<(), String> {
        let sources = STAND_INS
            .iter()
            .map(|(class, source)| {
                let path = directory.join(format!("{class}.java"));
                std::fs::create_dir_all(path.parent().expect("A stand-in lives in a package."))
                    .and_then(|()| std::fs::write(&path, source))
                    .map(|()| path)
                    .map_err(|error| format!("A stand-in could not be written: {error}."))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let compiled = Command::new(javac())
            .arg("-d")
            .arg(directory)
            .args(&sources)
            .output()
            .map_err(|error| {
                format!(
                    "No Java compiler could be run ({error}). These tests compile stand-ins for \
                     Android's classes and run them, so they need JDK 17 or later on PATH or at \
                     JAVA_HOME."
                )
            })?;
        if !compiled.status.success() {
            return Err(format!(
                "The stand-ins did not compile: {}",
                String::from_utf8_lossy(&compiled.stderr)
            ));
        }
        Ok(())
    }

    fn start() -> Result<Machine, String> {
        let directory = std::env::temp_dir().join(format!(
            "manganis-activity-stand-ins-{}",
            std::process::id()
        ));
        std::fs::create_dir(&directory)
            .map_err(|error| format!("{} could not be made: {error}.", directory.display()))?;
        let started = compile(&directory).and_then(|()| {
            let arguments = InitArgsBuilder::new()
                .version(JNIVersion::V8)
                .option(format!("-Djava.class.path={}", directory.display()))
                .build()
                .map_err(|error| {
                    format!("The virtual machine's arguments were refused: {error}.")
                })?;
            let vm = JavaVM::new(arguments)
                .map_err(|error| format!("No virtual machine could be started: {error}."))?;
            let application = {
                let mut env = vm
                    .attach_current_thread_permanently()
                    .map_err(|error| format!("The test could not attach: {error}."))?;
                for (class, _) in STAND_INS {
                    env.find_class(class)
                        .map_err(|error| format!("{class} could not be loaded: {error}."))?;
                }
                env.new_object("android/app/Application", "()V", &[])
                    .and_then(|application| env.new_global_ref(application))
                    .map_err(|error| format!("No application could be made: {error}."))?
            };
            Ok(Machine { vm, application })
        });
        std::fs::remove_dir_all(&directory)
            .map_err(|error| format!("{} could not be removed: {error}.", directory.display()))?;
        let machine = started?;
        unsafe {
            ndk_context::initialize_android_context(
                machine.vm.get_java_vm_pointer().cast(),
                machine.application.as_obj().as_raw().cast(),
            );
        }
        Ok(machine)
    }

    /// The device a test runs on, taken for as long as the test holds it,
    /// with no activity recorded and the application's context published.
    struct Device {
        machine: &'static Machine,
        _taken: MutexGuard<'static, ()>,
    }

    fn device() -> Device {
        static MACHINE: OnceLock<Result<Machine, String>> = OnceLock::new();
        static TAKEN: Mutex<()> = Mutex::new(());
        let taken = TAKEN.lock().unwrap_or_else(PoisonError::into_inner);
        let machine = match MACHINE.get_or_init(start) {
            Ok(machine) => machine,
            Err(error) => panic!("{error}"),
        };
        let device = Device {
            machine,
            _taken: taken,
        };
        *CREATED.lock().unwrap_or_else(PoisonError::into_inner) = None;
        device.publish(machine.application.as_obj());
        device
    }

    impl Device {
        fn env(&self) -> JNIEnv<'static> {
            self.machine
                .vm
                .attach_current_thread_permanently()
                .expect("The test thread attaches.")
        }

        fn publish(&self, context: &JObject<'_>) {
            unsafe {
                ndk_context::release_android_context();
                ndk_context::initialize_android_context(
                    self.machine.vm.get_java_vm_pointer().cast(),
                    context.as_raw().cast(),
                );
            }
        }

        fn activity(&self) -> GlobalRef {
            self.env()
                .with_local_frame(LOCAL_REFERENCES, |env| -> jni::errors::Result<GlobalRef> {
                    let activity = env.new_object("android/app/Activity", "()V", &[])?;
                    env.new_global_ref(activity)
                })
                .expect("An activity is made and held.")
        }

        /// Whether the activity recorded last can still be reached.
        fn reachable(&self) -> bool {
            self.env()
                .with_local_frame(LOCAL_REFERENCES, |env| -> jni::errors::Result<bool> {
                    Ok(CREATED
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .as_ref()
                        .expect("An activity was recorded.")
                        .upgrade_local(env)?
                        .is_some())
                })
                .expect("The reference can be read.")
        }

        fn created(&self, activity: &JObject<'_>) {
            activity_created(&self.env(), activity).expect("It is recorded.");
        }

        fn destroy(&self, activity: &JObject<'_>) {
            self.env()
                .call_method(activity, "destroy", "()V", &[])
                .expect("It is destroyed.");
        }

        fn is(&self, handed: &JObject<'_>, expected: &JObject<'_>) -> bool {
            self.env()
                .is_same_object(handed, expected)
                .expect("References compare.")
        }
    }

    fn handed() -> Option<GlobalRef> {
        with_activity(|env, activity| env.new_global_ref(activity).ok())
    }

    #[test]
    fn with_an_application_published_and_no_activity_created_nothing_is_handed_out() {
        let _device = device();
        let mut asked = false;
        assert_eq!(
            with_activity(|_, _| {
                asked = true;
                Some(())
            }),
            None
        );
        assert!(!asked);
    }

    #[test]
    fn the_activity_created_last_is_handed_out() {
        let device = device();
        let first = device.activity();
        device.created(first.as_obj());
        let handed_first = handed().expect("An activity is alive.");
        assert!(device.is(handed_first.as_obj(), first.as_obj()));
        let second = device.activity();
        device.created(second.as_obj());
        let handed_second = handed().expect("An activity is alive.");
        assert!(device.is(handed_second.as_obj(), second.as_obj()));
    }

    #[test]
    fn a_destroyed_activity_is_never_handed_out_nor_one_before_it() {
        let device = device();
        let first = device.activity();
        device.created(first.as_obj());
        let second = device.activity();
        device.created(second.as_obj());
        device.destroy(second.as_obj());
        assert!(handed().is_none());
        let third = device.activity();
        device.created(third.as_obj());
        let handed_third = handed().expect("The activity created since is alive.");
        assert!(device.is(handed_third.as_obj(), third.as_obj()));
    }

    #[test]
    fn an_activity_collected_since_it_was_handed_out_is_never_handed_out_again() {
        let device = device();
        let collected = device.activity();
        device.created(collected.as_obj());
        for _ in 0..3 {
            drop(handed().expect("The activity is alive."));
            drop(with_context(|env, context| {
                env.new_global_ref(context).ok()
            }));
        }
        drop(collected);
        let mut env = device.env();
        let mut collections = 0;
        while device.reachable() {
            assert!(
                collections < 10_000,
                "The virtual machine never collected an activity nothing refers to."
            );
            env.call_static_method("java/lang/System", "gc", "()V", &[])
                .expect("The virtual machine collects.");
            collections += 1;
        }
        assert!(handed().is_none());
    }

    #[test]
    fn a_published_activity_is_handed_out_where_none_was_created() {
        let device = device();
        let published = device.activity();
        device.publish(published.as_obj());
        let handed_published = handed().expect("The published activity is alive.");
        assert!(device.is(handed_published.as_obj(), published.as_obj()));
        device.destroy(published.as_obj());
        assert!(handed().is_none());
    }

    #[test]
    fn an_activity_created_is_handed_out_before_one_published() {
        let device = device();
        let published = device.activity();
        device.publish(published.as_obj());
        let created = device.activity();
        device.created(created.as_obj());
        let handed_created = handed().expect("The created activity is alive.");
        assert!(device.is(handed_created.as_obj(), created.as_obj()));
    }

    #[test]
    fn the_published_context_is_handed_out_whether_or_not_an_activity_is_alive() {
        let device = device();
        let context = || with_context(|env, context| env.new_global_ref(context).ok());
        let application = context().expect("The application is published.");
        assert!(device.is(application.as_obj(), device.machine.application.as_obj()));
        let activity = device.activity();
        device.created(activity.as_obj());
        device.destroy(activity.as_obj());
        assert!(handed().is_none());
        let still = context().expect("The application outlives the activity.");
        assert!(device.is(still.as_obj(), device.machine.application.as_obj()));
        device.publish(&JObject::null());
        assert!(context().is_none(), "Nothing published is handed out.");
        assert!(handed().is_none());
    }

    #[test]
    fn no_exception_is_left_pending_by_anything_handed_out() {
        let device = device();
        let activity = device.activity();
        device.created(activity.as_obj());
        assert!(handed().is_some());
        device.destroy(activity.as_obj());
        assert!(handed().is_none());
        assert!(
            !device
                .env()
                .exception_check()
                .expect("The thread can be asked.")
        );
    }
}

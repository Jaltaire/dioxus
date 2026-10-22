//! Native specific utilities for resolving assets in a bundle. This module is intended for use in renderers that
//! need to resolve asset bundles for resources like images, and fonts.

use http::{Response, status::StatusCode};
use std::path::{Component, Path, PathBuf};

use crate::{AssetPathError, NativeAssetResolveError};

/// An error that can occur when serving an asset.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum AssetServeError {
    /// The asset path could not be resolved.
    #[error("Failed to resolve asset: {0}")]
    ResolveError(#[from] NativeAssetResolveError),

    /// An error occurred while constructing the HTTP response.
    #[error("Failed to construct response: {0}")]
    ResponseError(#[from] http::Error),
}

/// Try to resolve the path of an asset from a given URI path.
pub(crate) fn resolve_native_asset_path(path: &str) -> Result<PathBuf, AssetPathError> {
    #[allow(clippy::unnecessary_lazy_evaluations)]
    resolve_asset_path_from_filesystem(path).ok_or_else(|| {
        #[cfg(target_os = "android")]
        {
            // If the asset exists in the Android asset manager, return the can't be represented as a path
            // error instead
            if to_java_load_asset(path).is_some() {
                return AssetPathError::CannotRepresentAsPath;
            }
        }

        AssetPathError::NotFound
    })
}

/// Try to resolve the path of an asset from a given URI path.
fn resolve_asset_path_from_filesystem(path: &str) -> Option<PathBuf> {
    if cfg!(target_os = "android") {
        return None;
    }
    resolve_asset_path_from_root(path, &get_asset_root())
}

fn resolve_asset_path_from_root(path: &str, bundle_root: &Path) -> Option<PathBuf> {
    // The path is the first part of the URI, so we need to trim the leading slash.
    let relative_path = relative_asset_path(path)?;
    canonical_asset_path(bundle_root, &relative_path)
}

fn relative_asset_path(path: &str) -> Option<PathBuf> {
    let decoded = percent_encoding::percent_decode_str(path)
        .decode_utf8()
        .ok()?;
    let relative = match decoded.strip_prefix('/') {
        Some(relative) => relative,
        None => decoded.as_ref(),
    };
    if relative.contains(['\\', ':', '\0'])
        || relative
            .split('/')
            .any(|component| matches!(component, "" | "." | ".."))
    {
        return None;
    }
    let relative = PathBuf::from(relative);
    relative
        .components()
        .all(|component| matches!(component, Component::Normal(_)))
        .then_some(relative)
}

fn canonical_asset_path(root: &Path, relative: &Path) -> Option<PathBuf> {
    let root = root.canonicalize().ok()?;
    let asset = root.join(relative).canonicalize().ok()?;
    // If the asset exists, return it.
    (asset.starts_with(root) && asset.is_file()).then_some(asset)
}

struct ResolvedAsset {
    mime_type: &'static str,
    body: Vec<u8>,
}

impl ResolvedAsset {
    fn new(mime_type: &'static str, body: Vec<u8>) -> Self {
        Self { mime_type, body }
    }

    fn into_response(self) -> Result<Response<Vec<u8>>, AssetServeError> {
        Ok(Response::builder()
            .header("Content-Type", self.mime_type)
            .body(self.body)?)
    }
}

/// Read the bytes for an asset
pub(crate) fn resolve_native_asset(path: &str) -> Result<Vec<u8>, NativeAssetResolveError> {
    // Attempt to serve from the asset dir on android using its loader
    #[cfg(target_os = "android")]
    {
        if let Some(asset) = to_java_load_asset(path) {
            return Ok(asset);
        }
    }

    let Some(uri_path) = resolve_asset_path_from_filesystem(path) else {
        return Err(NativeAssetResolveError::IoError(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "The asset was not found.",
        )));
    };
    Ok(std::fs::read(uri_path)?)
}

/// Resolve the asset and its mime type
fn resolve_asset(path: &str) -> Result<Option<ResolvedAsset>, NativeAssetResolveError> {
    // Attempt to serve from the asset dir on android using its loader
    #[cfg(target_os = "android")]
    {
        if let Some(asset) = to_java_load_asset(path) {
            let extension = path.rsplit_once('.').and_then(|(_, ext)| Some(ext));
            let mime_type = get_mime_from_ext(extension);
            return Ok(Some(ResolvedAsset::new(mime_type, asset)));
        }
    }

    let Some(uri_path) = resolve_asset_path_from_filesystem(path) else {
        return Ok(None);
    };
    let mime_type = get_mime_from_path(&uri_path)?;
    let body = std::fs::read(uri_path)?;
    Ok(Some(ResolvedAsset::new(mime_type, body)))
}

/// Serve an asset from the filesystem or a custom asset handler.
///
/// This method properly accesses the asset directory based on the platform and serves the asset
/// wrapped in an HTTP response.
///
/// Platform specifics:
/// - On the web, this returns AssetServerError since there's no filesystem access. Use `fetch` instead.
/// - On Android, it attempts to load assets using the Android AssetManager.
/// - On other platforms, it serves assets from the filesystem.
pub fn serve_asset(path: &str) -> Result<Response<Vec<u8>>, AssetServeError> {
    match resolve_asset(path)? {
        Some(asset) => asset.into_response(),
        None => Ok(Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(String::from("Not Found").into_bytes())?),
    }
}

/// Get the asset directory, following tauri/cargo-bundles directory discovery approach
///
/// Currently supports:
/// - [x] macOS
/// - [x] iOS
/// - [x] Windows
/// - [x] Linux (appimage)
/// - [x] Linux (rpm)
/// - [x] Linux (deb)
/// - [x] Android (returns current exe.)
#[allow(unreachable_code)]
fn get_asset_root() -> PathBuf {
    let cur_exe = std::env::current_exe().unwrap();

    // In macOS bundles, the assets are in the `Resources` directory
    if cfg!(target_os = "macos") {
        return cur_exe
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("Resources");
    }

    // In linux bundles, the assets are placed in the lib/$product_name directory
    // bin/
    //   main
    // lib/
    //   $product_name/
    //     assets/
    if cfg!(target_os = "linux") {
        if let Some(lib_dir) = cur_exe
            .parent()
            .and_then(|parent| parent.parent())
            .map(|root| root.join("lib"))
        {
            // Try the product name from the compile-time env var first
            if let Some(product_name) = dioxus_cli_config::product_name() {
                let path = lib_dir.join(&product_name);
                if path.exists() {
                    return path;
                }
            }

            // Fall back to scanning lib/ for a directory containing an `assets/` subfolder.
            // This handles debug builds where DIOXUS_PRODUCT_NAME isn't baked in.
            if let Ok(entries) = std::fs::read_dir(&lib_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() && path.join("assets").is_dir() {
                        return path;
                    }
                }
            }
        }
    }

    // For all others, the structure looks like this:
    // app.(exe/appimage)
    //   main.exe
    //   assets/
    cur_exe.parent().unwrap().to_path_buf()
}

/// Get the mime type from a path-like string
fn get_mime_from_path(asset: &Path) -> std::io::Result<&'static str> {
    if asset.extension().is_some_and(|ext| ext == "svg") {
        return Ok("image/svg+xml");
    }

    match infer::get_from_path(asset)?.map(|f| f.mime_type()) {
        Some(f) if f != "text/plain" => Ok(f),
        _other => Ok(get_mime_by_ext(asset)),
    }
}

/// Get the mime type from a URI using its extension
fn get_mime_by_ext(trimmed: &Path) -> &'static str {
    let ext = trimmed.extension().as_ref().and_then(|ext| ext.to_str());
    get_mime_from_ext(ext)
}

/// Get the mime type from a URI using its extension
pub fn get_mime_from_ext(ext: Option<&str>) -> &'static str {
    match ext {
        // The common assets are all utf-8 encoded
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json; charset=utf-8",
        Some("svg") => "image/svg+xml; charset=utf-8",
        Some("html") => "text/html; charset=utf-8",

        // the rest... idk? probably not
        Some("mjs") => "text/javascript; charset=utf-8",
        Some("bin") => "application/octet-stream",
        Some("csv") => "text/csv",
        Some("ico") => "image/vnd.microsoft.icon",
        Some("jsonld") => "application/ld+json",
        Some("rtf") => "application/rtf",
        Some("mp4") => "video/mp4",
        // Assume HTML when a TLD is found for eg. `dioxus:://dioxuslabs.app` | `dioxus://hello.com`
        Some(_) => "text/html; charset=utf-8",
        // https://developer.mozilla.org/en-US/docs/Web/HTTP/Basics_of_HTTP/MIME_types/Common_types
        // using octet stream according to this:
        None => "application/octet-stream",
    }
}

#[cfg(any(target_os = "android", test))]
fn android_asset_relative_path(filepath: &str) -> Option<PathBuf> {
    let relative = relative_asset_path(filepath)?;
    match relative.strip_prefix("assets") {
        Ok(normalized) if normalized.as_os_str().is_empty() => None,
        Ok(normalized) => Some(normalized.to_path_buf()),
        Err(_) => Some(relative),
    }
}

#[cfg(target_os = "android")]
pub(crate) fn to_java_load_asset(filepath: &str) -> Option<Vec<u8>> {
    let normalized = android_asset_relative_path(filepath)?;

    // In debug mode, attempt to read the asset from the session cache if it exists.
    #[cfg(debug_assertions)]
    {
        if let Some(path) =
            canonical_asset_path(&dioxus_cli_config::android_session_cache_dir(), &normalized)
        {
            return std::fs::read(path).ok();
        }
    }

    use std::ptr::NonNull;

    let ctx = ndk_context::android_context();
    let vm = unsafe { jni::JavaVM::from_raw(ctx.vm().cast()) }.unwrap();
    let mut env = vm.attach_current_thread().unwrap();

    // Query the Asset Manager
    let asset_manager_ptr = env
        .call_method(
            unsafe { jni::objects::JObject::from_raw(ctx.context().cast()) },
            "getAssets",
            "()Landroid/content/res/AssetManager;",
            &[],
        )
        .expect("Failed to get asset manager")
        .l()
        .expect("Failed to get asset manager as object");

    unsafe {
        let asset_manager =
            ndk_sys::AAssetManager_fromJava(env.get_native_interface(), *asset_manager_ptr);

        let asset_manager = ndk::asset::AssetManager::from_ptr(
            NonNull::new(asset_manager).expect("Invalid asset manager"),
        );

        let cstr = std::ffi::CString::new(normalized.to_str()?).ok()?;

        let mut asset = asset_manager.open(&cstr)?;
        Some(asset.buffer().unwrap().to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Assets {
        directory: tempfile::TempDir,
        root: PathBuf,
    }

    impl Assets {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().join("bundle");
            std::fs::create_dir_all(root.join("assets")).unwrap();
            Self { directory, root }
        }

        fn file(&self, name: &str) -> PathBuf {
            let path = self.root.join(name);
            std::fs::write(&path, "An asset.").unwrap();
            path.canonicalize().unwrap()
        }

        fn outside_file(&self) -> PathBuf {
            let path = self.directory.path().join("outside.txt");
            std::fs::write(&path, "Private data.").unwrap();
            path.canonicalize().unwrap()
        }

        fn resolve(&self, path: &str) -> Option<PathBuf> {
            resolve_asset_path_from_root(path, &self.root)
        }
    }

    #[test]
    fn absolute_filesystem_paths_are_not_served() {
        let assets = Assets::new();
        let path = assets.outside_file();
        let response = serve_asset(path.to_str().unwrap()).unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(assets.resolve(path.to_str().unwrap()).is_none());
    }

    #[test]
    fn traversal_cannot_escape_the_asset_root() {
        let assets = Assets::new();
        assets.outside_file();
        for path in [
            "/assets/../../outside.txt",
            "/assets/%2e%2e/%2e%2e/outside.txt",
            "/assets/%2E%2E%2F%2e%2e%2foutside.txt",
            "/assets/..%2f..%2foutside.txt",
            "/assets/..%5c..%5coutside.txt",
            "../outside.txt",
            "assets/../../outside.txt",
        ] {
            assert!(
                assets.resolve(path).is_none(),
                "The resolver allowed {path}."
            );
        }
    }

    #[test]
    fn paths_are_relative_to_the_bundle_instead_of_the_working_directory() {
        let assets = Assets::new();
        let file = assets.file("Cargo.toml");
        assert_eq!(assets.resolve("Cargo.toml"), Some(file.clone()));
        assert_eq!(assets.resolve("/Cargo.toml"), Some(file));
    }

    #[test]
    fn bundled_assets_are_canonicalized_after_decoding_once() {
        let assets = Assets::new();
        let file = assets.file("assets/an asset-音.svg");
        for path in [
            "/assets/an%20asset-%E9%9F%B3.svg",
            "assets/an asset-音.svg",
            "/assets%2fan%20asset-音.svg",
        ] {
            assert_eq!(assets.resolve(path), Some(file.clone()));
        }
        let literal = assets.file("assets/%2e%2e.txt");
        assert_eq!(assets.resolve("/assets/%252e%252e.txt"), Some(literal));
    }

    #[test]
    fn ambiguous_and_platform_specific_paths_are_rejected() {
        for path in [
            "",
            "/",
            "//assets/file.txt",
            "/%2fassets/file.txt",
            "assets//file.txt",
            "assets/",
            "./assets/file.txt",
            "/assets/./file.txt",
            "/assets/%2e/file.txt",
            "/assets/../file.txt",
            "C:/file.txt",
            "/C:/file.txt",
            "C:file.txt",
            "C%3a/file.txt",
            r"C:\file.txt",
            r"\\server\share\file.txt",
            r"/assets\file.txt",
            "/assets/file.txt:stream",
            "/assets/file%00.txt",
            "/assets/%ff.txt",
            "/assets/%c0%af.txt",
            "file:///assets/file.txt",
            "/assets/.%2e/file.txt",
        ] {
            assert!(
                relative_asset_path(path).is_none(),
                "The parser allowed {path}."
            );
        }
    }

    #[test]
    fn missing_assets_roots_and_directories_are_not_files() {
        let assets = Assets::new();
        assert!(assets.resolve("/assets/missing.txt").is_none());
        assert!(assets.resolve("/assets").is_none());
        assert!(resolve_asset_path_from_root("/file.txt", &assets.root.join("missing")).is_none());
        let file = assets.file("file.txt");
        assert!(resolve_asset_path_from_root("/file.txt", &file).is_none());
    }

    #[test]
    fn traversal_is_rejected_even_when_it_would_stay_inside_the_root() {
        let assets = Assets::new();
        assets.file("file.txt");
        assert!(assets.resolve("/assets/../file.txt").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn file_and_directory_symlinks_cannot_escape_the_asset_root() {
        let assets = Assets::new();
        let outside = assets.outside_file();
        std::os::unix::fs::symlink(outside, assets.root.join("assets/link.txt")).unwrap();
        std::os::unix::fs::symlink(
            assets.directory.path(),
            assets.root.join("assets/directory"),
        )
        .unwrap();
        assert!(assets.resolve("/assets/link.txt").is_none());
        assert!(assets.resolve("/assets/directory/outside.txt").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn sibling_directories_with_the_same_prefix_are_outside_the_root() {
        let assets = Assets::new();
        let sibling = assets.directory.path().join("bundle-extra");
        std::fs::create_dir(&sibling).unwrap();
        std::fs::write(sibling.join("outside.txt"), "Private data.").unwrap();
        std::os::unix::fs::symlink(sibling, assets.root.join("assets/sibling")).unwrap();
        assert!(assets.resolve("/assets/sibling/outside.txt").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_within_the_root_and_an_aliased_root_are_supported() {
        let assets = Assets::new();
        let file = assets.file("assets/file.txt");
        std::os::unix::fs::symlink(&file, assets.root.join("assets/link.txt")).unwrap();
        std::os::unix::fs::symlink(assets.root.join("assets"), assets.root.join("alias")).unwrap();
        let root_alias = assets.directory.path().join("root-alias");
        std::os::unix::fs::symlink(&assets.root, &root_alias).unwrap();
        assert_eq!(assets.resolve("/assets/link.txt"), Some(file.clone()));
        assert_eq!(assets.resolve("/alias/file.txt"), Some(file.clone()));
        assert_eq!(
            resolve_asset_path_from_root("/assets/file.txt", &root_alias),
            Some(file)
        );
    }

    #[cfg(unix)]
    #[test]
    fn broken_and_looping_symlinks_are_not_served() {
        let assets = Assets::new();
        std::os::unix::fs::symlink("missing.txt", assets.root.join("assets/broken.txt")).unwrap();
        std::os::unix::fs::symlink("loop.txt", assets.root.join("assets/loop.txt")).unwrap();
        assert!(assets.resolve("/assets/broken.txt").is_none());
        assert!(assets.resolve("/assets/loop.txt").is_none());
    }

    #[test]
    fn android_asset_paths_are_validated_before_removing_one_asset_prefix() {
        for path in [
            "/assets/file.txt",
            "assets/file.txt",
            "/file.txt",
            "file.txt",
        ] {
            assert_eq!(
                android_asset_relative_path(path),
                Some(PathBuf::from("file.txt"))
            );
        }
        assert_eq!(
            android_asset_relative_path("/assets/assets/file.txt"),
            Some(PathBuf::from("assets/file.txt"))
        );
        for path in [
            "assets",
            "/assets",
            "/assets/../file.txt",
            "/assets/%2e%2e/file.txt",
            "//outside.txt",
            "C:/outside.txt",
            "/assets/file%00.txt",
            "/assets/%ff.txt",
        ] {
            assert!(
                android_asset_relative_path(path).is_none(),
                "The Android resolver allowed {path}."
            );
        }
    }

    #[cfg(target_os = "android")]
    #[test]
    fn android_does_not_serve_files_from_the_process_executable_directory() {
        assert!(resolve_asset_path_from_filesystem("app_process64").is_none());
    }

    #[test]
    fn malformed_utf8_is_not_served() {
        let response = serve_asset("/assets/%ff.txt").unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn asset_responses_preserve_the_body_and_mime_type_without_granting_cross_origin_access() {
        let response = ResolvedAsset::new("text/plain", b"An asset.".to_vec())
            .into_response()
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["content-type"], "text/plain");
        assert_eq!(response.body(), b"An asset.");
        assert!(
            !response
                .headers()
                .contains_key("access-control-allow-origin")
        );
    }

    #[test]
    fn missing_assets_do_not_grant_cross_origin_access() {
        let response = serve_asset("/assets/missing.txt").unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(
            !response
                .headers()
                .contains_key("access-control-allow-origin")
        );
    }
}

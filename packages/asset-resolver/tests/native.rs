#![cfg(feature = "native")]

use dioxus_asset_resolver::{AssetPathError, AssetResolveError, NativeAssetResolveError};
use http::StatusCode;

fn rejected_paths() -> (tempfile::TempDir, Vec<String>) {
    let directory = tempfile::tempdir().unwrap();
    let outside = directory.path().join("outside.txt");
    std::fs::write(&outside, "Private data.").unwrap();
    let paths = vec![
        outside.to_str().unwrap().to_string(),
        format!(
            "/assets/../../{}",
            outside.to_str().unwrap().trim_start_matches('/')
        ),
        "/assets/%2e%2e/%2e%2e/outside.txt".to_string(),
        "/assets/%ff.txt".to_string(),
        "/assets/file%00.txt".to_string(),
        "C:/outside.txt".to_string(),
    ];
    (directory, paths)
}

#[test]
fn public_asset_paths_reject_files_outside_the_bundle() {
    let (_directory, paths) = rejected_paths();
    for path in paths {
        assert!(
            matches!(
                dioxus_asset_resolver::asset_path(&path),
                Err(AssetPathError::NotFound)
            ),
            "The resolver allowed {path}."
        );
    }
}

#[test]
fn public_asset_reads_reject_files_outside_the_bundle() {
    let (_directory, paths) = rejected_paths();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    for path in paths {
        let result = runtime.block_on(dioxus_asset_resolver::read_asset_bytes(&path));
        match result {
            Err(AssetResolveError::Native(NativeAssetResolveError::IoError(error))) => {
                assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
            }
            other => panic!("The resolver returned an unexpected result for {path}: {other:?}."),
        }
    }
}

#[test]
fn public_asset_responses_reject_files_outside_the_bundle_without_cors_headers() {
    let (_directory, paths) = rejected_paths();
    for path in paths {
        let response = dioxus_asset_resolver::native::serve_asset(&path).unwrap();
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "The resolver allowed {path}."
        );
        assert!(
            !response
                .headers()
                .contains_key("access-control-allow-origin")
        );
        assert!(
            !response
                .body()
                .windows(b"Private data.".len())
                .any(|bytes| bytes == b"Private data.")
        );
    }
}

#[test]
fn bundled_assets_resolve_through_all_public_apis() {
    const ROOT_VARIABLE: &str = "DIOXUS_RESOLVER_TEST_ASSET_ROOT";
    if let Some(root) = std::env::var_os(ROOT_VARIABLE) {
        let root = std::path::PathBuf::from(root);
        let expected = root.join("assets/file.css").canonicalize().unwrap();
        let body = b"body { color: red; }";
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        for path in ["/assets/file.css", "assets/file.css", "/assets/file%2ecss"] {
            assert_eq!(dioxus_asset_resolver::asset_path(path).unwrap(), expected);
            assert_eq!(
                runtime
                    .block_on(dioxus_asset_resolver::read_asset_bytes(path))
                    .unwrap(),
                body
            );
            let response = dioxus_asset_resolver::native::serve_asset(path).unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                response.headers()["content-type"],
                "text/css; charset=utf-8"
            );
            assert_eq!(response.body(), body);
            assert!(
                !response
                    .headers()
                    .contains_key("access-control-allow-origin")
            );
        }
        #[cfg(unix)]
        for path in ["/assets/leak.txt", "/assets/directory/outside.txt"] {
            assert!(matches!(
                dioxus_asset_resolver::asset_path(path),
                Err(AssetPathError::NotFound)
            ));
            assert!(
                runtime
                    .block_on(dioxus_asset_resolver::read_asset_bytes(path))
                    .is_err()
            );
            assert_eq!(
                dioxus_asset_resolver::native::serve_asset(path)
                    .unwrap()
                    .status(),
                StatusCode::NOT_FOUND
            );
        }
        return;
    }

    let directory = tempfile::tempdir().unwrap();
    let bundle = directory.path().join("bundle");
    let executable_directory = if cfg!(target_os = "macos") {
        bundle.join("Contents/MacOS")
    } else {
        bundle.clone()
    };
    let asset_root = if cfg!(target_os = "macos") {
        bundle.join("Contents/Resources")
    } else {
        bundle
    };
    std::fs::create_dir_all(&executable_directory).unwrap();
    std::fs::create_dir_all(asset_root.join("assets")).unwrap();
    std::fs::write(asset_root.join("assets/file.css"), b"body { color: red; }").unwrap();
    #[cfg(unix)]
    {
        let outside = directory.path().join("outside.txt");
        std::fs::write(&outside, "Private data.").unwrap();
        std::os::unix::fs::symlink(outside, asset_root.join("assets/leak.txt")).unwrap();
        std::os::unix::fs::symlink(directory.path(), asset_root.join("assets/directory")).unwrap();
    }
    let executable =
        executable_directory.join(format!("asset-tests{}", std::env::consts::EXE_SUFFIX));
    std::fs::copy(std::env::current_exe().unwrap(), &executable).unwrap();
    let output = std::process::Command::new(executable)
        .args([
            "--exact",
            "bundled_assets_resolve_through_all_public_apis",
            "--nocapture",
        ])
        .env(ROOT_VARIABLE, asset_root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "The bundled asset checks failed.\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

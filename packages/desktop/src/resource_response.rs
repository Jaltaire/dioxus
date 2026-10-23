use std::{borrow::Cow, sync::Arc};
use wry::http::{
    HeaderValue, Response, StatusCode,
    header::{CACHE_CONTROL, CONTENT_TYPE, X_CONTENT_TYPE_OPTIONS},
};

type ResourceResponseVerifier = Arc<dyn Fn(&str, Option<&str>, &[u8]) -> bool + Send + Sync>;

#[derive(Clone, Default)]
pub(crate) struct ResourceResponsePolicy {
    pub(crate) verifier: Option<ResourceResponseVerifier>,
    pub(crate) required: bool,
}

impl ResourceResponsePolicy {
    pub(crate) fn response(
        &self,
        path: &str,
        mut response: Response<Cow<'static, [u8]>>,
    ) -> Response<Cow<'static, [u8]>> {
        if self.verifier.is_some() || self.required {
            let allowed = resource_path(path).is_some_and(|decoded| {
                let mut content_types = response.headers().get_all(CONTENT_TYPE).iter();
                let mime = match (content_types.next(), content_types.next()) {
                    (None, None) => None,
                    (Some(value), None) => match value.to_str() {
                        Ok(value) => Some(value),
                        Err(_) => return false,
                    },
                    _ => return false,
                };
                match &self.verifier {
                    Some(verifier) => verifier(path, mime, response.body()),
                    None => !executable_resource(&decoded, mime, response.body()),
                }
            });
            if !allowed {
                response = Response::new(Cow::Borrowed(&[]));
                *response.status_mut() = StatusCode::FORBIDDEN;
            }
        }
        response
            .headers_mut()
            .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
        response
            .headers_mut()
            .insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
        response
    }

    pub(crate) fn responder(
        self,
        path: String,
        respond: impl FnOnce(Response<Cow<'static, [u8]>>) + Send + 'static,
    ) -> crate::RequestAsyncResponder {
        crate::RequestAsyncResponder::new(move |response| respond(self.response(&path, response)))
    }
}

fn resource_path(path: &str) -> Option<Cow<'_, str>> {
    let mut bytes = path.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%' && !(bytes.next()?.is_ascii_hexdigit() && bytes.next()?.is_ascii_hexdigit())
        {
            return None;
        }
    }
    let decoded = percent_encoding::percent_decode_str(path)
        .decode_utf8()
        .ok()?;
    let relative = decoded.strip_prefix('/').unwrap_or(&decoded);
    if relative.chars().any(char::is_control)
        || relative.contains(['\\', ':', '?', '#'])
        || relative
            .split('/')
            .any(|component| matches!(component, "" | "." | ".."))
    {
        return None;
    }
    Some(decoded)
}

fn executable_resource(path: &str, mime: Option<&str>, body: &[u8]) -> bool {
    if body.starts_with(b"\0asm") {
        return true;
    }
    if let Some((_, extension)) = path.rsplit_once('.') {
        if matches!(
            extension.to_ascii_lowercase().as_str(),
            "js" | "mjs" | "cjs" | "wasm" | "html" | "htm" | "xhtml" | "xht" | "svg" | "css"
        ) {
            return true;
        }
    }
    mime.is_some_and(|mime| {
        matches!(
            mime.split(';')
                .next()
                .expect("A MIME type always has a first component.")
                .trim()
                .to_ascii_lowercase()
                .as_str(),
            "application/javascript"
                | "application/ecmascript"
                | "application/x-javascript"
                | "application/x-ecmascript"
                | "text/javascript"
                | "text/ecmascript"
                | "text/jscript"
                | "text/livescript"
                | "text/x-javascript"
                | "text/x-ecmascript"
                | "text/javascript1.0"
                | "text/javascript1.1"
                | "text/javascript1.2"
                | "text/javascript1.3"
                | "text/javascript1.4"
                | "text/javascript1.5"
                | "application/wasm"
                | "text/html"
                | "application/xhtml+xml"
                | "image/svg+xml"
                | "text/css"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, mpsc};

    fn response(
        status: StatusCode,
        mime: Option<&str>,
        body: &'static [u8],
    ) -> Response<Cow<'static, [u8]>> {
        let mut response = Response::builder().status(status);
        if let Some(mime) = mime {
            response = response.header(CONTENT_TYPE, mime);
        }
        response.body(Cow::Borrowed(body)).unwrap()
    }

    fn required() -> ResourceResponsePolicy {
        ResourceResponsePolicy {
            required: true,
            ..Default::default()
        }
    }

    fn assert_denied(response: Response<Cow<'static, [u8]>>) {
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(response.body().is_empty());
        assert_eq!(response.headers()[CACHE_CONTROL], "no-store");
        assert_eq!(response.headers()[X_CONTENT_TYPE_OPTIONS], "nosniff");
        assert!(!response.headers().contains_key(CONTENT_TYPE));
        assert!(!response.headers().contains_key("content-length"));
        assert!(!response.headers().contains_key("location"));
    }

    #[test]
    fn default_optional_mode_preserves_unverified_executable_responses() {
        for status in [
            StatusCode::OK,
            StatusCode::NOT_FOUND,
            StatusCode::INTERNAL_SERVER_ERROR,
        ] {
            let response = ResourceResponsePolicy::default().response(
                "/assets/code.js",
                response(status, Some("text/javascript"), b"A response."),
            );
            assert_eq!(response.status(), status);
            assert_eq!(response.body().as_ref(), b"A response.");
            assert_eq!(response.headers()[CONTENT_TYPE], "text/javascript");
            assert_eq!(response.headers()[CACHE_CONTROL], "no-store");
            assert_eq!(response.headers()[X_CONTENT_TYPE_OPTIONS], "nosniff");
        }
    }

    #[test]
    fn required_mode_rejects_each_executable_extension_without_a_verifier() {
        for extension in [
            "js", "mjs", "cjs", "wasm", "html", "htm", "xhtml", "xht", "svg", "css", "JS", "WASM",
        ] {
            let path = format!("/assets/code.{extension}");
            for mime in [None, Some("application/octet-stream")] {
                assert_denied(
                    required().response(&path, response(StatusCode::OK, mime, b"A response.")),
                );
            }
        }
        for path in [
            "/assets/code%2ejs",
            "/assets/code.%6djs",
            "/assets%2fcode.wasm",
            "assets/code.js",
        ] {
            assert_denied(
                required().response(path, response(StatusCode::OK, None, b"A response.")),
            );
        }
    }

    #[test]
    fn required_mode_rejects_executable_mime_types_on_passive_paths() {
        for mime in [
            "application/javascript",
            "application/ecmascript",
            "application/x-javascript",
            "application/x-ecmascript",
            "text/javascript",
            "text/ecmascript",
            "text/jscript",
            "text/livescript",
            "text/x-javascript",
            "text/x-ecmascript",
            "text/javascript1.0",
            "text/javascript1.1",
            "text/javascript1.2",
            "text/javascript1.3",
            "text/javascript1.4",
            "text/javascript1.5",
            "application/wasm",
            "text/html",
            "application/xhtml+xml",
            "image/svg+xml",
            "text/css",
            "Text/JavaScript; charset=UTF-8",
            " text/css ; charset=utf-8",
        ] {
            assert_denied(required().response(
                "/assets/code.bin",
                response(StatusCode::OK, Some(mime), b"A response."),
            ));
        }
    }

    #[test]
    fn resolver_mime_mappings_preserve_passive_assets_and_executable_denials() {
        for extension in [
            "png", "jpg", "jpeg", "gif", "webp", "avif", "ttf", "otf", "woff", "woff2", "bin",
            "unknown",
        ] {
            let path = format!("/assets/file.{extension}");
            let mime = dioxus_asset_resolver::native::get_mime_from_ext(Some(extension));
            let output =
                required().response(&path, response(StatusCode::OK, Some(mime), b"Asset bytes."));
            assert_eq!(output.status(), StatusCode::OK);
            assert_eq!(output.body().as_ref(), b"Asset bytes.");
        }
        for extension in ["js", "mjs", "cjs", "wasm", "html", "xhtml", "svg", "css"] {
            let mime = dioxus_asset_resolver::native::get_mime_from_ext(Some(extension));
            assert_denied(required().response(
                "/assets/file.bin",
                response(StatusCode::OK, Some(mime), b"Asset bytes."),
            ));
        }
    }

    #[test]
    fn required_mode_detects_renamed_wasm_bytes_without_a_mime_type() {
        for mime in [None, Some("application/octet-stream")] {
            assert_denied(required().response(
                "/assets/code.bin",
                response(StatusCode::OK, mime, b"\0asm\x01\0\0\0"),
            ));
        }
    }

    #[test]
    fn required_mode_allows_passive_resources_without_a_verifier() {
        for (path, mime, body) in [
            (
                "/assets/image.png",
                Some("image/png"),
                b"An image.".as_slice(),
            ),
            (
                "/assets/data.json",
                Some("application/json"),
                b"{}".as_slice(),
            ),
            ("/assets/file", None, b"A file.".as_slice()),
            (
                "/assets/directory.js/file.bin",
                Some("application/octet-stream"),
                b"\0as".as_slice(),
            ),
        ] {
            let response = required().response(path, response(StatusCode::OK, mime, body));
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.body().as_ref(), body);
        }
    }

    #[test]
    fn verifier_receives_the_exact_raw_path_mime_and_served_bytes() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let policy = ResourceResponsePolicy {
            verifier: Some(Arc::new({
                let seen = seen.clone();
                move |path, mime, body| {
                    seen.lock().unwrap().push((
                        path.to_string(),
                        mime.map(str::to_string),
                        body.to_vec(),
                    ));
                    body == b"\0asm\x01\0\0\0"
                }
            })),
            required: true,
        };
        for (path, mime) in [
            ("/assets/code%2ewasm", Some("application/wasm")),
            ("/assets/code.bin", None),
        ] {
            let response =
                policy.response(path, response(StatusCode::OK, mime, b"\0asm\x01\0\0\0"));
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.body().as_ref(), b"\0asm\x01\0\0\0");
        }
        assert_eq!(
            *seen.lock().unwrap(),
            [
                (
                    "/assets/code%2ewasm".to_string(),
                    Some("application/wasm".to_string()),
                    b"\0asm\x01\0\0\0".to_vec()
                ),
                (
                    "/assets/code.bin".to_string(),
                    None,
                    b"\0asm\x01\0\0\0".to_vec()
                ),
            ]
        );
    }

    #[test]
    fn mismatched_bytes_are_denied_in_optional_and_required_modes() {
        for required in [false, true] {
            let policy = ResourceResponsePolicy {
                verifier: Some(Arc::new(|_, _, bytes| bytes == b"Expected bytes.")),
                required,
            };
            assert_denied(policy.response(
                "/assets/code.js",
                response(StatusCode::OK, Some("text/javascript"), b"Changed bytes."),
            ));
        }
    }

    #[test]
    fn verification_applies_to_every_status_and_does_not_preserve_redirects_or_error_bodies() {
        for status in 100..=599 {
            let status = StatusCode::from_u16(status).unwrap();
            for accepted in [false, true] {
                let policy = ResourceResponsePolicy {
                    verifier: Some(Arc::new(move |_, _, _| accepted)),
                    required: true,
                };
                let mut input = response(status, Some("text/javascript"), b"Exact bytes.");
                input
                    .headers_mut()
                    .insert("location", HeaderValue::from_static("/assets/other.js"));
                input
                    .headers_mut()
                    .insert("content-length", HeaderValue::from_static("12"));
                let output = policy.response("/assets/code.js", input);
                if accepted {
                    assert_eq!(output.status(), status);
                    assert_eq!(output.body().as_ref(), b"Exact bytes.");
                    assert_eq!(output.headers()["location"], "/assets/other.js");
                } else {
                    assert_denied(output);
                }
            }
        }
    }

    #[test]
    fn malformed_paths_are_denied_before_calling_the_verifier() {
        let policy = ResourceResponsePolicy {
            verifier: Some(Arc::new(|_, _, _| {
                panic!("Malformed paths must not reach the verifier.")
            })),
            required: true,
        };
        for path in [
            "",
            "/",
            "//assets/code.js",
            "/%2fassets/code.js",
            "/assets//code.js",
            "/assets/",
            "/assets/./code.js",
            "/assets/%2e/code.js",
            "/assets/../code.js",
            "/assets/%2e%2e/code.js",
            "/assets/%2E%2E%2Fcode.js",
            "/assets/..%5ccode.js",
            "C:/code.js",
            "C%3a/code.js",
            "/assets/code.js:stream",
            "/assets/code%00.js",
            "/assets/code%ff.js",
            "/assets/%c0%af.js",
            "/assets/code%",
            "/assets/code%2",
            "/assets/code%2g.js",
            "/assets/code%g2.js",
            "/assets/code%0a.js",
            "/assets/code\n.js",
            "/assets/code?query.js",
            "/assets/code%23.js",
        ] {
            assert_denied(policy.response(
                path,
                response(StatusCode::OK, Some("text/javascript"), b"Exact bytes."),
            ));
            assert_denied(
                required().response(path, response(StatusCode::OK, None, b"Exact bytes.")),
            );
        }
    }

    #[test]
    fn paths_are_decoded_only_once_and_preserve_valid_unicode_and_encoded_names() {
        for path in [
            "/assets/an%20asset-%E9%9F%B3.js",
            "assets/an asset-音.js",
            "/assets/%252e%252e.js",
            "/assets/100%25.js",
        ] {
            let policy = ResourceResponsePolicy {
                verifier: Some(Arc::new(|_, _, _| true)),
                required: true,
            };
            assert_eq!(
                policy
                    .response(path, response(StatusCode::OK, None, b"Exact bytes."))
                    .status(),
                StatusCode::OK
            );
        }
    }

    #[test]
    fn invalid_and_duplicate_content_types_are_denied_before_verification() {
        let policy = ResourceResponsePolicy {
            verifier: Some(Arc::new(|_, _, _| {
                panic!("Ambiguous MIME types must not reach the verifier.")
            })),
            required: true,
        };
        let mut invalid = response(StatusCode::OK, None, b"Exact bytes.");
        invalid
            .headers_mut()
            .insert(CONTENT_TYPE, HeaderValue::from_bytes(b"\xff").unwrap());
        assert_denied(policy.response("/assets/code.js", invalid));
        let mut duplicate = response(StatusCode::OK, Some("text/javascript"), b"Exact bytes.");
        duplicate.headers_mut().append(
            CONTENT_TYPE,
            HeaderValue::from_static("application/octet-stream"),
        );
        assert_denied(policy.response("/assets/code.js", duplicate));
    }

    #[test]
    fn final_response_headers_replace_custom_cache_and_sniffing_permissions() {
        let policy = ResourceResponsePolicy {
            verifier: Some(Arc::new(|_, _, _| true)),
            required: true,
        };
        let mut input = response(StatusCode::OK, Some("text/css"), b"Exact bytes.");
        for (name, value) in [
            (CACHE_CONTROL, "public, max-age=86400"),
            (X_CONTENT_TYPE_OPTIONS, "sniff"),
        ] {
            input
                .headers_mut()
                .append(name.clone(), HeaderValue::from_static(value));
            input
                .headers_mut()
                .append(name, HeaderValue::from_static(value));
        }
        let output = policy.response("/assets/style.css", input);
        assert_eq!(output.headers().get_all(CACHE_CONTROL).iter().count(), 1);
        assert_eq!(output.headers()[CACHE_CONTROL], "no-store");
        assert_eq!(
            output
                .headers()
                .get_all(X_CONTENT_TYPE_OPTIONS)
                .iter()
                .count(),
            1
        );
        assert_eq!(output.headers()[X_CONTENT_TYPE_OPTIONS], "nosniff");
    }

    #[test]
    fn an_owned_response_is_verified_after_it_crosses_a_thread_boundary() {
        let (sender, receiver) = mpsc::channel();
        let responder = required().responder("/assets/code.js".to_string(), move |response| {
            sender.send(response).unwrap()
        });
        std::thread::spawn(move || responder.respond(Response::new(b"Exact bytes.".to_vec())))
            .join()
            .unwrap();
        assert_denied(receiver.recv().unwrap());
    }

    #[test]
    fn config_sets_replaces_and_clones_the_shared_verifier_without_enabling_required_mode() {
        let first: ResourceResponseVerifier = Arc::new(|_, _, _| true);
        let second: ResourceResponseVerifier = Arc::new(|_, _, _| false);
        let config = crate::Config::new();
        assert!(!config.resource_response_policy.required);
        assert!(config.resource_response_policy.verifier.is_none());
        let config = config.with_resource_response_verifier(first.clone());
        assert!(!config.resource_response_policy.required);
        assert!(Arc::ptr_eq(
            config.resource_response_policy.verifier.as_ref().unwrap(),
            &first
        ));
        let config = config
            .with_resource_response_verifier(second.clone())
            .with_resource_response_verifier_required(true);
        let cloned = config.resource_response_policy.clone();
        assert!(config.resource_response_policy.required);
        assert!(Arc::ptr_eq(cloned.verifier.as_ref().unwrap(), &second));
        assert!(
            !config
                .with_resource_response_verifier_required(false)
                .resource_response_policy
                .required
        );
    }
}

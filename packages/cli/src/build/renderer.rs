use crate::Renderer;
use target_lexicon::Triple;

use crate::BuildRequest;

impl BuildRequest {
    pub(crate) fn renderer_enabled_by_dioxus_dependency(
        package: &krates::cm::Package,
    ) -> Option<(Renderer, String)> {
        let mut renderers = vec![];

        // Attempt to discover the platform directly from the dioxus dependency
        //
        // [dependencies]
        // dioxus = { features = ["web"] }
        //
        if let Some(dxs) = package.dependencies.iter().find(|dep| dep.name == "dioxus") {
            for feature in dxs.features.iter() {
                if let Some(renderer) = Renderer::autodetect_from_cargo_feature(feature) {
                    renderers.push((renderer, format!("dioxus/{}", feature)));
                }
            }
        }

        if renderers.len() != 1 {
            return None;
        }

        Some(renderers[0].clone())
    }

    /// Discover the renderer from a renderer crate the package depends on directly, as an
    /// application that launches through `dioxus_native::launch` does.
    pub(crate) fn renderer_enabled_by_direct_dependency(
        package: &krates::cm::Package,
    ) -> Option<(Renderer, String)> {
        let renderers: Vec<(Renderer, String)> = [
            Renderer::Webview,
            Renderer::Native,
            Renderer::Web,
            Renderer::Liveview,
        ]
        .into_iter()
        .filter_map(|renderer| {
            let name = renderer.renderer_crate()?;
            Self::depends_directly_on(package, name).then(|| (renderer, name.to_string()))
        })
        .collect();

        match renderers.as_slice() {
            [renderer] => Some(renderer.clone()),
            _ => None,
        }
    }

    fn depends_directly_on(package: &krates::cm::Package, name: &str) -> bool {
        package.dependencies.iter().any(|dependency| {
            dependency.name == name && matches!(dependency.kind, krates::cm::DependencyKind::Normal)
        })
    }

    pub(crate) fn features_that_enable_renderers(
        package: &krates::cm::Package,
    ) -> Vec<(Renderer, String)> {
        package
            .features
            .keys()
            .filter_map(|key| {
                Renderer::autodetect_from_cargo_feature(key).map(|v| (v, key.to_string()))
            })
            .collect()
    }

    /// Return the platforms that are enabled for the package only from the default features
    ///
    /// Ideally only one platform is enabled but we need to be able to
    pub(crate) fn enabled_cargo_toml_default_features_renderers(
        package: &krates::cm::Package,
    ) -> Vec<(Renderer, String)> {
        let mut renderers = vec![];

        // Start searching through the default features
        //
        // [features]
        // default = ["dioxus/web"]
        //
        // or
        //
        // [features]
        // default = ["web"]
        // web = ["dioxus/web"]
        let Some(default) = package.features.get("default") else {
            return renderers;
        };

        // we only trace features 1 level deep..
        // TODO: trace all enabled features, not just default features
        for feature in default.iter() {
            // If the user directly specified a platform we can just use that.
            if feature.starts_with("dioxus/") {
                let dx_feature = feature.trim_start_matches("dioxus/");
                let auto = Renderer::autodetect_from_cargo_feature(dx_feature);
                if let Some(auto) = auto {
                    renderers.push((auto, dx_feature.to_string()));
                }
            }

            // If the user is specifying an internal feature that points to a platform, we can use that
            let internal_feature = package.features.get(feature);
            if let Some(internal_feature) = internal_feature {
                for feature in internal_feature {
                    if feature.starts_with("dioxus/") {
                        let dx_feature = feature.trim_start_matches("dioxus/");
                        let auto = Renderer::autodetect_from_cargo_feature(dx_feature);
                        if let Some(auto) = auto {
                            renderers.push((auto, dx_feature.to_string()));
                        }
                    }
                }
            }
        }

        renderers.sort();
        renderers.dedup();

        renderers
    }

    /// Gather the features that are enabled for the package
    pub fn rendererless_features(package: &krates::cm::Package) -> Vec<String> {
        let Some(default) = package.features.get("default") else {
            return Vec::new();
        };

        let mut kept_features = vec![];

        // Only keep the top-level features in the default list that don't point to a platform directly
        // IE we want to drop `web` if default = ["web"]
        'top: for feature in default {
            // Don't keep features that point to a platform via dioxus/blah
            if feature.starts_with("dioxus/") {
                let dx_feature = feature.trim_start_matches("dioxus/");
                if Renderer::autodetect_from_cargo_feature(dx_feature).is_some() {
                    tracing::debug!(
                        "Dropping feature {feature} since it points to a platform renderer"
                    );
                    continue 'top;
                }
            }

            // Don't keep features that point to a platform via an internal feature
            if let Some(internal_feature) = package.features.get(feature) {
                for feature in internal_feature {
                    if feature.starts_with("dioxus/") {
                        let dx_feature = feature.trim_start_matches("dioxus/");
                        if Renderer::autodetect_from_cargo_feature(dx_feature).is_some() {
                            tracing::debug!(
                                "Dropping feature {feature} since it points to a platform renderer transitively"
                            );
                            continue 'top;
                        }
                    }
                }
            }

            // Otherwise we can keep it
            kept_features.push(feature.to_string());
        }

        kept_features
    }

    /// Get the features required to build for the given platform
    pub fn feature_for_platform_and_renderer(
        package: &krates::cm::Package,
        triple: &Triple,
        renderer: Renderer,
    ) -> Option<String> {
        // Try to find the feature that activates the dioxus feature for the given platform
        let dioxus_feature = renderer.feature_name(triple);

        let res = package.features.iter().find_map(|(key, features)| {
            // if the feature is just the name of the platform, we use that
            if key == dioxus_feature {
                tracing::debug!("Found feature {key} for renderer {renderer}");
                return Some(key.clone());
            }

            // Otherwise look for the feature that starts with dioxus/ or dioxus?/ and matches just the single platform
            // we are looking for.
            let mut dioxus_renderers_enabled = Vec::new();
            for feature in features {
                if let Some((_, after_dioxus)) = feature.split_once("dioxus") {
                    if let Some(dioxus_feature_enabled) =
                        after_dioxus.trim_start_matches('?').strip_prefix('/')
                    {
                        if Renderer::autodetect_from_cargo_feature(dioxus_feature_enabled).is_some()
                        {
                            dioxus_renderers_enabled.push(dioxus_feature_enabled.to_string());
                        }
                    }
                }
            }

            // If there is exactly one renderer enabled by this feature, we can use it
            if let [feature_name] = dioxus_renderers_enabled.as_slice() {
                if feature_name == dioxus_feature {
                    tracing::debug!(
                        "Found feature {key} for renderer {renderer} which enables dioxus/{renderer}"
                    );
                    return Some(key.clone());
                }
            }

            None
        });

        res.or_else(|| {
            if let Some(name) = renderer.renderer_crate()
                && Self::depends_directly_on(package, name)
            {
                tracing::debug!(
                    "The package depends on {name} directly, so no dioxus feature is added for renderer {renderer}"
                );
                return None;
            }
            let depends_on_dioxus = package.dependencies.iter().any(|dep| dep.name == "dioxus");
            if depends_on_dioxus {
                let fallback = format!("dioxus/{dioxus_feature}");
                tracing::debug!(
                    "Could not find explicit feature for renderer {renderer}, passing `fallback` instead"
                );
                Some(fallback)
            } else {
                None
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::{BuildRequest, Renderer};

    fn package(
        dependencies: &[(&str, Option<&str>)],
        features: serde_json::Value,
    ) -> krates::cm::Package {
        let dependencies: Vec<serde_json::Value> = dependencies
            .iter()
            .map(|(name, kind)| {
                serde_json::json!({
                    "name": name,
                    "req": "*",
                    "kind": kind,
                    "optional": false,
                    "uses_default_features": true,
                    "features": [],
                })
            })
            .collect();
        serde_json::from_value(serde_json::json!({
            "name": "example",
            "version": "0.1.0",
            "id": "example 0.1.0 (path+file:///example)",
            "dependencies": dependencies,
            "targets": [],
            "features": features,
            "manifest_path": "/example/Cargo.toml",
        }))
        .expect("The package description parses.")
    }

    fn host() -> target_lexicon::Triple {
        "aarch64-apple-darwin".parse().unwrap()
    }

    #[test]
    fn a_direct_renderer_crate_names_the_renderer() {
        for (name, renderer) in [
            ("dioxus-native", Renderer::Native),
            ("dioxus-desktop", Renderer::Webview),
            ("dioxus-web", Renderer::Web),
            ("dioxus-liveview", Renderer::Liveview),
        ] {
            let package = package(&[("dioxus", None), (name, None)], serde_json::json!({}));
            assert_eq!(
                BuildRequest::renderer_enabled_by_direct_dependency(&package),
                Some((renderer, name.to_string()))
            );
        }
    }

    #[test]
    fn no_renderer_is_named_by_none_or_by_two_renderer_crates() {
        let none = package(&[("dioxus", None)], serde_json::json!({}));
        assert_eq!(
            BuildRequest::renderer_enabled_by_direct_dependency(&none),
            None
        );
        let two = package(
            &[("dioxus-native", None), ("dioxus-desktop", None)],
            serde_json::json!({}),
        );
        assert_eq!(
            BuildRequest::renderer_enabled_by_direct_dependency(&two),
            None
        );
    }

    #[test]
    fn a_renderer_crate_needed_only_by_tests_or_the_build_names_nothing() {
        for kind in ["dev", "build"] {
            let package = package(&[("dioxus-native", Some(kind))], serde_json::json!({}));
            assert_eq!(
                BuildRequest::renderer_enabled_by_direct_dependency(&package),
                None,
                "{kind}"
            );
        }
    }

    #[test]
    fn a_package_that_depends_on_the_renderer_crate_gets_no_dioxus_feature() {
        let package = package(
            &[("dioxus", None), ("dioxus-native", None)],
            serde_json::json!({}),
        );
        assert_eq!(
            BuildRequest::feature_for_platform_and_renderer(&package, &host(), Renderer::Native),
            None
        );
    }

    #[test]
    fn a_package_without_the_renderer_crate_still_gets_the_dioxus_feature() {
        let package = package(&[("dioxus", None)], serde_json::json!({}));
        assert_eq!(
            BuildRequest::feature_for_platform_and_renderer(&package, &host(), Renderer::Native),
            Some("dioxus/native".to_string())
        );
        let desktop_only = package_with_desktop_crate();
        assert_eq!(
            BuildRequest::feature_for_platform_and_renderer(
                &desktop_only,
                &host(),
                Renderer::Native
            ),
            Some("dioxus/native".to_string())
        );
    }

    fn package_with_desktop_crate() -> krates::cm::Package {
        package(
            &[("dioxus", None), ("dioxus-desktop", None)],
            serde_json::json!({}),
        )
    }

    #[test]
    fn the_package_s_own_renderer_feature_wins_over_the_direct_crate() {
        let package = package(
            &[("dioxus", None), ("dioxus-desktop", None)],
            serde_json::json!({ "desktop": ["dioxus/desktop"], "mobile": ["dioxus/mobile"] }),
        );
        assert_eq!(
            BuildRequest::feature_for_platform_and_renderer(&package, &host(), Renderer::Webview),
            Some("desktop".to_string())
        );
        let ios: target_lexicon::Triple = "aarch64-apple-ios".parse().unwrap();
        assert_eq!(
            BuildRequest::feature_for_platform_and_renderer(&package, &ios, Renderer::Webview),
            Some("mobile".to_string())
        );
    }

    #[test]
    fn every_renderer_but_the_server_has_a_crate() {
        assert_eq!(Renderer::Native.renderer_crate(), Some("dioxus-native"));
        assert_eq!(Renderer::Webview.renderer_crate(), Some("dioxus-desktop"));
        assert_eq!(Renderer::Web.renderer_crate(), Some("dioxus-web"));
        assert_eq!(Renderer::Liveview.renderer_crate(), Some("dioxus-liveview"));
        assert_eq!(Renderer::Server.renderer_crate(), None);
    }
}

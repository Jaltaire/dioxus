use blitz_dom::FontContext;
use dioxus_core::LaunchConfig;
use winit::window::WindowAttributes;

/// What a window's renderer paints behind the document.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Background {
    /// The renderer's base colour, so the window is opaque.
    #[default]
    Opaque,
    /// Nothing, so the desktop shows wherever the document paints nothing.
    #[cfg(all(
        feature = "vello-hybrid",
        not(any(feature = "vello", feature = "vello-cpu-base", feature = "skia"))
    ))]
    Transparent,
}

/// Launch-time configuration for a dioxus-native application.
pub struct Config {
    pub(crate) window_attributes: WindowAttributes,
    pub(crate) font_ctx: Option<FontContext>,
    pub(crate) background: Background,
}

impl LaunchConfig for Config {}

impl Default for Config {
    fn default() -> Self {
        let window_attributes = WindowAttributes::default()
            .with_title(dioxus_cli_config::app_title().unwrap_or_else(|| "Dioxus App".to_string()));

        // On WASM, append the canvas to the document body. Surface size is
        // intentionally not seeded: winit-web's with_surface_size writes inline
        // canvas.style.width/height that overrides host CSS. View::init reads
        // the canvas's CSS layout box when winit reports a 0×0 initial size.
        // To target an existing `<canvas>`, replace these attributes via
        // [`Config::with_window_attributes`].
        #[cfg(target_arch = "wasm32")]
        let window_attributes = {
            use winit::platform::web::WindowAttributesWeb;
            window_attributes.with_platform_attributes(Box::new(
                WindowAttributesWeb::default().with_append(true),
            ))
        };

        Self {
            window_attributes,
            font_ctx: None,
            background: Background::Opaque,
        }
    }
}

impl Config {
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the configuration for the window.
    pub fn with_window_attributes(mut self, attrs: WindowAttributes) -> Self {
        self.window_attributes = attrs;
        self
    }

    /// Set a custom [`FontContext`] for the document.
    ///
    /// On WASM, browsers don't expose system fonts, so a `FontContext` with
    /// bundled fonts must be provided. Use [`crate::build_single_font_ctx`] for
    /// the common one-font case.
    pub fn with_font_ctx(mut self, font_ctx: FontContext) -> Self {
        self.font_ctx = Some(font_ctx);
        self
    }

    /// Paint nothing behind the document, so the desktop shows wherever the
    /// document leaves a pixel empty.
    ///
    /// The window itself must be transparent too, which
    /// [`WindowAttributes::with_transparent`] asks for.
    #[cfg(all(
        feature = "vello-hybrid",
        not(any(feature = "vello", feature = "vello-cpu-base", feature = "skia"))
    ))]
    pub fn with_transparent_background(mut self) -> Self {
        self.background = Background::Transparent;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_are_opaque_unless_asked_otherwise() {
        assert_eq!(Config::new().background, Background::Opaque);
        assert_eq!(Background::default(), Background::Opaque);
    }

    #[cfg(all(
        feature = "vello-hybrid",
        not(any(feature = "vello", feature = "vello-cpu-base", feature = "skia"))
    ))]
    #[test]
    fn a_transparent_background_is_kept_through_other_settings() {
        let config = Config::new()
            .with_transparent_background()
            .with_window_attributes(WindowAttributes::default().with_transparent(true));
        assert_eq!(config.background, Background::Transparent);
        assert!(config.window_attributes.transparent);
    }
}

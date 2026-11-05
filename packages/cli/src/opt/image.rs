use anyhow::Context;
use image::{DynamicImage, EncodableLayout};
use manganis_core::{ImageAssetOptions, ImageFormat, ImageSize};
use std::io::{BufWriter, Write};
use std::path::Path;

pub(crate) fn process_image(
    image_options: &ImageAssetOptions,
    source: &Path,
    output_path: &Path,
) -> anyhow::Result<()> {
    if image_options.format() == ImageFormat::Unknown
        && image_options.size() == ImageSize::Automatic
    {
        return copy_without_metadata(source, output_path);
    }

    let mut image = image::ImageReader::new(std::io::Cursor::new(&*std::fs::read(source)?))
        .with_guessed_format()
        .context("Failed to guess image format")?
        .decode();

    if let Ok(image) = &mut image {
        if let ImageSize::Manual { width, height } = image_options.size() {
            *image = image.resize_exact(width, height, image::imageops::FilterType::Lanczos3);
        }
    }

    match (image, image_options.format()) {
        (image, ImageFormat::Png) => {
            compress_png(image.context("Failed to decode image")?, output_path);
        }
        (image, ImageFormat::Jpg) => {
            compress_jpg(image.context("Failed to decode image")?, output_path)?;
        }
        (Ok(image), ImageFormat::Avif) => {
            if let Err(error) = image.save(output_path) {
                tracing::error!(
                    "Failed to save avif image: {} with path {}. You must have the avif feature enabled to use avif assets",
                    error,
                    output_path.display()
                );
            }
        }
        (Ok(image), ImageFormat::Webp) => {
            if let Err(err) = image.save(output_path) {
                tracing::error!(
                    "Failed to save webp image: {}. You must have the avif feature enabled to use webp assets",
                    err
                );
            }
        }
        (Ok(image), _) => {
            image.save(output_path).with_context(|| {
                format!(
                    "Failed to save image (from {}) with path {}",
                    source.display(),
                    output_path.display()
                )
            })?;
        }
        // If we can't decode the image or it is of an unknown type, we just copy the file
        _ => copy_unchanged(source, output_path)?,
    }

    Ok(())
}

fn copy_without_metadata(source: &Path, output_path: &Path) -> anyhow::Result<()> {
    let bytes = std::fs::read(source)
        .with_context(|| format!("Failed to read image: {}", source.display()))?;
    match super::image_metadata::without_metadata(&bytes) {
        Some(kept) => std::fs::write(output_path, kept)
            .with_context(|| format!("Failed to write image: {}", output_path.display())),
        None => {
            if matches!(
                image::guess_format(&bytes),
                Ok(image::ImageFormat::Png | image::ImageFormat::Jpeg | image::ImageFormat::WebP)
            ) {
                tracing::warn!(
                    "The image {} could not be read to leave its metadata behind, so it was copied as it is.",
                    source.display()
                );
            }
            copy_unchanged(source, output_path)
        }
    }
}

fn copy_unchanged(source: &Path, output_path: &Path) -> anyhow::Result<()> {
    let source_file = std::fs::File::open(source).context("Failed to open source file")?;
    let mut reader = std::io::BufReader::new(source_file);
    let output_file = std::fs::File::create(output_path)
        .with_context(|| format!("Failed to create output file: {}", output_path.display()))?;
    let mut writer = std::io::BufWriter::new(output_file);
    std::io::copy(&mut reader, &mut writer)
        .with_context(|| {
            format!(
                "Failed to write image to output location: {}",
                output_path.display()
            )
        })
        .context("Failed to copy image data")?;
    writer
        .flush()
        .with_context(|| format!("Failed to flush output file: {}", output_path.display()))?;
    Ok(())
}

pub(crate) fn compress_png(image: DynamicImage, output_location: &Path) {
    // Image loading/saving is outside scope of this library
    let width = image.width() as usize;
    let height = image.height() as usize;
    let bitmap: Vec<_> = image
        .into_rgba8()
        .pixels()
        .map(|px| imagequant::RGBA::new(px[0], px[1], px[2], px[3]))
        .collect();

    // Configure the library
    let mut liq = imagequant::new();
    liq.set_speed(5).unwrap();
    liq.set_quality(0, 99).unwrap();

    // Describe the bitmap
    let mut img = liq.new_image(&bitmap[..], width, height, 0.0).unwrap();

    // The magic happens in quantize()
    let mut res = match liq.quantize(&mut img) {
        Ok(res) => res,
        Err(err) => panic!("Quantization failed, because: {err:?}"),
    };

    let (palette, pixels) = res.remapped(&mut img).unwrap();

    let file = std::fs::File::create(output_location).unwrap();
    let w = &mut BufWriter::new(file);

    let mut encoder = png::Encoder::new(w, width as u32, height as u32);
    encoder.set_color(png::ColorType::Rgba);
    let mut flattened_palette = Vec::new();
    let mut alpha_palette = Vec::new();
    for px in palette {
        flattened_palette.push(px.r);
        flattened_palette.push(px.g);
        flattened_palette.push(px.b);
        alpha_palette.push(px.a);
    }
    encoder.set_palette(flattened_palette);
    encoder.set_trns(alpha_palette);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_color(png::ColorType::Indexed);
    encoder.set_compression(png::Compression::Best);
    let mut writer = encoder.write_header().unwrap();
    writer.write_image_data(&pixels).unwrap();
    writer.finish().unwrap();
}

pub(crate) fn compress_jpg(image: DynamicImage, output_location: &Path) -> anyhow::Result<()> {
    let mut comp = mozjpeg::Compress::new(mozjpeg::ColorSpace::JCS_EXT_RGBX);
    let width = image.width() as usize;
    let height = image.height() as usize;

    comp.set_size(width, height);
    let mut comp = comp.start_compress(Vec::new())?; // any io::Write will work

    comp.write_scanlines(image.to_rgba8().as_bytes())?;

    let jpeg_bytes = comp.finish()?;

    let file = std::fs::File::create(output_location)?;
    let w = &mut BufWriter::new(file);
    w.write_all(&jpeg_bytes)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageFormat as Encoding, Rgba, RgbaImage};
    use manganis_core::{AssetOptions, AssetOptionsBuilder, AssetVariant};

    const WIDTH: u32 = 24;
    const HEIGHT: u32 = 16;

    fn picture() -> DynamicImage {
        DynamicImage::ImageRgba8(RgbaImage::from_fn(WIDTH, HEIGHT, |x, y| {
            Rgba([(x * 10) as u8, (y * 15) as u8, 120, 255])
        }))
    }

    fn encoded(encoding: Encoding) -> Vec<u8> {
        let picture = match encoding {
            Encoding::Jpeg => DynamicImage::ImageRgb8(picture().to_rgb8()),
            _ => picture(),
        };
        let mut bytes = std::io::Cursor::new(Vec::new());
        picture
            .write_to(&mut bytes, encoding)
            .expect("The test picture encodes.");
        bytes.into_inner()
    }

    const LOSSY_WEBP: &[u8] = &[
        0x52, 0x49, 0x46, 0x46, 0x3e, 0x00, 0x00, 0x00, 0x57, 0x45, 0x42, 0x50, 0x56, 0x50, 0x38,
        0x20, 0x32, 0x00, 0x00, 0x00, 0x10, 0x02, 0x00, 0x9d, 0x01, 0x2a, 0x08, 0x00, 0x08, 0x00,
        0x02, 0x80, 0x42, 0x25, 0xa8, 0x02, 0x74, 0x6b, 0x80, 0xfe, 0x00, 0x05, 0xb9, 0x82, 0xe0,
        0x00, 0xfe, 0xcd, 0xe4, 0x25, 0xb0, 0xe1, 0xd8, 0x0b, 0x5b, 0xf6, 0x67, 0xfe, 0x37, 0x73,
        0xae, 0x61, 0x5b, 0x54, 0xff, 0xa1, 0xcc, 0x92, 0x00, 0x00,
    ];

    const NOTE: &str = "Kept only when the file is copied.";

    fn png_with_a_text_chunk() -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut encoder = png::Encoder::new(&mut bytes, WIDTH, HEIGHT);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .add_text_chunk("Comment".to_string(), NOTE.to_string())
            .expect("The text chunk is accepted.");
        let mut writer = encoder.write_header().expect("The PNG header is written.");
        writer
            .write_image_data(picture().to_rgba8().as_raw())
            .expect("The PNG pixels are written.");
        writer.finish().expect("The PNG is finished.");
        bytes
    }

    fn jpeg_with_a_comment() -> Vec<u8> {
        let plain = encoded(Encoding::Jpeg);
        let length = u16::try_from(NOTE.len() + 2).expect("The comment fits a segment.");
        let mut bytes = plain[..2].to_vec();
        bytes.extend_from_slice(&[0xFF, 0xFE]);
        bytes.extend_from_slice(&length.to_be_bytes());
        bytes.extend_from_slice(NOTE.as_bytes());
        bytes.extend_from_slice(&plain[2..]);
        bytes
    }

    fn options(builder: AssetOptionsBuilder<ImageAssetOptions>) -> ImageAssetOptions {
        match builder.into_asset_options().variant() {
            AssetVariant::Image(image) => *image,
            other => panic!("An image builder makes image options, not {other:?}."),
        }
    }

    struct Processed {
        source: Vec<u8>,
        output: Vec<u8>,
    }

    fn process(
        options: &ImageAssetOptions,
        source: Vec<u8>,
        source_name: &str,
        output_name: &str,
    ) -> Processed {
        let folder = tempfile::tempdir().expect("A temporary folder is available.");
        let source_path = folder.path().join(source_name);
        let output_path = folder.path().join(output_name);
        std::fs::write(&source_path, &source).expect("The source is written.");
        process_image(options, &source_path, &output_path).expect("The image is processed.");
        let output = std::fs::read(&output_path).expect("The output is written.");
        Processed { source, output }
    }

    fn decoded(bytes: &[u8]) -> DynamicImage {
        image::load_from_memory(bytes).expect("The output decodes.")
    }

    fn png_tightly_packed() -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut encoder = png::Encoder::new(&mut bytes, WIDTH, HEIGHT);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_compression(png::Compression::Best);
        encoder.set_filter(png::FilterType::NoFilter);
        let mut writer = encoder.write_header().expect("The PNG header is written.");
        writer
            .write_image_data(picture().to_rgba8().as_raw())
            .expect("The PNG pixels are written.");
        writer.finish().expect("The PNG is finished.");
        bytes
    }

    fn jpeg_at_quality_95() -> Vec<u8> {
        let mut bytes = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 95)
            .encode_image(&picture().to_rgb8())
            .expect("The JPEG encodes.");
        bytes
    }

    fn encoded_again(bytes: &[u8], encoding: Encoding) -> Vec<u8> {
        let mut again = std::io::Cursor::new(Vec::new());
        decoded(bytes)
            .write_to(&mut again, encoding)
            .expect("The picture encodes again.");
        again.into_inner()
    }

    #[test]
    fn an_image_without_metadata_that_asks_for_no_format_or_size_is_copied_byte_for_byte() {
        for (source, extension) in [
            (png_tightly_packed(), "png"),
            (jpeg_at_quality_95(), "jpg"),
            (jpeg_at_quality_95(), "jpeg"),
            (LOSSY_WEBP.to_vec(), "webp"),
        ] {
            decoded(&source);
            let processed = process(
                &ImageAssetOptions::default(),
                source,
                &format!("picture.{extension}"),
                &format!("output.{extension}"),
            );
            assert_eq!(processed.output, processed.source, "{extension}");
        }
    }

    #[test]
    fn the_fixtures_are_ones_a_fresh_encoding_would_change() {
        let png = png_tightly_packed();
        assert_ne!(encoded_again(&png, Encoding::Png), png);
        let jpeg = jpeg_at_quality_95();
        assert_ne!(encoded_again(&jpeg, Encoding::Jpeg), jpeg);
        assert_eq!(&LOSSY_WEBP[12..16], b"VP8 ", "The WebP is lossy.");
    }

    #[test]
    fn the_image_builder_left_as_it_is_asks_for_nothing_and_is_copied() {
        let processed = process(
            &options(AssetOptions::image()),
            png_tightly_packed(),
            "picture.png",
            "output.png",
        );
        assert_eq!(processed.output, processed.source);
        let processed = process(
            &options(AssetOptions::image().with_preload(true)),
            LOSSY_WEBP.to_vec(),
            "picture.webp",
            "output.webp",
        );
        assert_eq!(processed.output, processed.source);
    }

    #[test]
    fn an_image_s_metadata_is_left_behind_when_it_is_copied() {
        let note = NOTE.as_bytes();
        for (source, extension) in [
            (png_with_a_text_chunk(), "png"),
            (jpeg_with_a_comment(), "jpg"),
        ] {
            assert!(source.windows(note.len()).any(|window| window == note));
            let processed = process(
                &ImageAssetOptions::default(),
                source,
                &format!("picture.{extension}"),
                &format!("output.{extension}"),
            );
            assert!(
                !processed
                    .output
                    .windows(note.len())
                    .any(|window| window == note),
                "{extension} keeps its note."
            );
            assert_eq!(
                decoded(&processed.output).to_rgba8(),
                decoded(&processed.source).to_rgba8(),
                "{extension}"
            );
        }
    }

    #[test]
    fn a_file_that_does_not_decode_is_copied_whatever_its_extension() {
        let bytes = b"These bytes are not a picture.".to_vec();
        let processed = process(
            &ImageAssetOptions::default(),
            bytes,
            "picture.png",
            "output.png",
        );
        assert_eq!(processed.output, processed.source);
    }

    #[test]
    fn a_png_too_broken_to_read_is_copied_as_it_is() {
        let mut bytes = png_tightly_packed();
        bytes.truncate(bytes.len() / 2);
        let processed = process(
            &ImageAssetOptions::default(),
            bytes,
            "picture.png",
            "output.png",
        );
        assert_eq!(processed.output, processed.source);
    }

    #[test]
    fn a_requested_size_still_resizes_in_the_format_of_the_extension() {
        let processed = process(
            &options(AssetOptions::image().with_size(ImageSize::Manual {
                width: 12,
                height: 8,
            })),
            encoded(Encoding::Png),
            "picture.png",
            "output.png",
        );
        assert_ne!(processed.output, processed.source);
        assert_eq!(
            image::guess_format(&processed.output).expect("The output has a format."),
            Encoding::Png
        );
        let resized = decoded(&processed.output);
        assert_eq!((resized.width(), resized.height()), (12, 8));
    }

    #[test]
    fn a_requested_png_is_still_quantized_to_a_palette() {
        let processed = process(
            &options(AssetOptions::image().with_png()),
            encoded(Encoding::Png),
            "picture.png",
            "output.png",
        );
        assert_eq!(&processed.output[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(processed.output[25], 3, "The PNG is indexed.");
        let quantized = decoded(&processed.output);
        assert_eq!((quantized.width(), quantized.height()), (WIDTH, HEIGHT));
    }

    #[test]
    fn a_requested_jpeg_is_still_compressed() {
        let processed = process(
            &options(AssetOptions::image().with_jpg()),
            encoded(Encoding::Png),
            "picture.png",
            "output.jpg",
        );
        assert_eq!(&processed.output[..3], [0xFF, 0xD8, 0xFF]);
        let compressed = decoded(&processed.output);
        assert_eq!((compressed.width(), compressed.height()), (WIDTH, HEIGHT));
    }

    #[test]
    fn a_requested_webp_is_still_encoded_from_another_format() {
        let processed = process(
            &options(AssetOptions::image().with_webp()),
            encoded(Encoding::Png),
            "picture.png",
            "output.webp",
        );
        assert_eq!(&processed.output[..4], b"RIFF");
        assert_eq!(&processed.output[8..12], b"WEBP");
        assert_eq!(decoded(&processed.output).to_rgba8(), picture().to_rgba8());
    }

    #[test]
    fn a_requested_avif_is_still_encoded() {
        let processed = process(
            &options(AssetOptions::image().with_avif()),
            encoded(Encoding::Png),
            "picture.png",
            "output.avif",
        );
        assert_eq!(&processed.output[4..12], b"ftypavif");
    }

    #[test]
    fn a_requested_size_and_format_are_both_applied() {
        let processed = process(
            &options(
                AssetOptions::image()
                    .with_webp()
                    .with_size(ImageSize::Manual {
                        width: 6,
                        height: 4,
                    }),
            ),
            encoded(Encoding::Png),
            "picture.png",
            "output.webp",
        );
        assert_eq!(&processed.output[8..12], b"WEBP");
        let resized = decoded(&processed.output);
        assert_eq!((resized.width(), resized.height()), (6, 4));
    }
}

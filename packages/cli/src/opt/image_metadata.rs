const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];

const PNG_KEPT_ANCILLARY: [&[u8; 4]; 11] = [
    b"tRNS", b"gAMA", b"cHRM", b"sRGB", b"iCCP", b"cICP", b"mDCv", b"cLLi", b"acTL", b"fcTL",
    b"fdAT",
];

const WEBP_KEPT: [&[u8; 4]; 6] = [b"ICCP", b"ANIM", b"ANMF", b"ALPH", b"VP8 ", b"VP8L"];

const WEBP_COLOUR_PROFILE: u8 = 0x20;
const WEBP_EXIF: u8 = 0x08;
const WEBP_XMP: u8 = 0x04;

const EXIF_HEADER: &[u8] = b"Exif\0\0";
const ORIENTATION: u16 = 0x0112;
const SHORT: u16 = 3;

pub(crate) fn without_metadata(bytes: &[u8]) -> Option<Vec<u8>> {
    if bytes.starts_with(&PNG_SIGNATURE) {
        png(bytes)
    } else if bytes.starts_with(&[0xFF, 0xD8]) {
        jpeg(bytes)
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        webp(bytes)
    } else {
        None
    }
}

fn be_u16(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}

fn be_u32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

fn le_u32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

fn orientation(tiff: &[u8]) -> Option<u16> {
    let little = match tiff.get(..4)? {
        [b'I', b'I', 42, 0] => true,
        [b'M', b'M', 0, 42] => false,
        _ => return None,
    };
    let u16_at = |at: usize| -> Option<u16> {
        let pair: [u8; 2] = tiff.get(at..at + 2)?.try_into().ok()?;
        Some(if little {
            u16::from_le_bytes(pair)
        } else {
            u16::from_be_bytes(pair)
        })
    };
    let u32_at = |at: usize| -> Option<u32> {
        let quad: [u8; 4] = tiff.get(at..at + 4)?.try_into().ok()?;
        Some(if little {
            u32::from_le_bytes(quad)
        } else {
            u32::from_be_bytes(quad)
        })
    };
    let directory = usize::try_from(u32_at(4)?).ok()?;
    let entries = usize::from(u16_at(directory)?);
    (0..entries).find_map(|index| {
        let entry = directory + 2 + index * 12;
        (u16_at(entry)? == ORIENTATION && u16_at(entry + 2)? == SHORT && u32_at(entry + 4)? == 1)
            .then(|| u16_at(entry + 8))
            .flatten()
    })
}

fn orientation_only(orientation: u16) -> Vec<u8> {
    let mut tiff = vec![b'I', b'I', 42, 0];
    tiff.extend_from_slice(&8u32.to_le_bytes());
    tiff.extend_from_slice(&1u16.to_le_bytes());
    tiff.extend_from_slice(&ORIENTATION.to_le_bytes());
    tiff.extend_from_slice(&SHORT.to_le_bytes());
    tiff.extend_from_slice(&1u32.to_le_bytes());
    tiff.extend_from_slice(&orientation.to_le_bytes());
    tiff.extend_from_slice(&[0, 0]);
    tiff.extend_from_slice(&0u32.to_le_bytes());
    tiff
}

fn kept_orientation(tiff: &[u8]) -> Option<Vec<u8>> {
    orientation(tiff)
        .filter(|turn| *turn != 1)
        .map(orientation_only)
}

fn png_chunk(kind: &[u8; 4], payload: &[u8]) -> Option<Vec<u8>> {
    let length = u32::try_from(payload.len()).ok()?;
    let mut crc = flate2::Crc::new();
    crc.update(kind);
    crc.update(payload);
    let mut chunk = Vec::with_capacity(12 + payload.len());
    chunk.extend_from_slice(&length.to_be_bytes());
    chunk.extend_from_slice(kind);
    chunk.extend_from_slice(payload);
    chunk.extend_from_slice(&crc.sum().to_be_bytes());
    Some(chunk)
}

fn png(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut kept = PNG_SIGNATURE.to_vec();
    let mut at = PNG_SIGNATURE.len();
    loop {
        let length = usize::try_from(be_u32(bytes, at)?).ok()?;
        let kind: &[u8; 4] = bytes.get(at + 4..at + 8)?.try_into().ok()?;
        let end = at.checked_add(12)?.checked_add(length)?;
        let chunk = bytes.get(at..end)?;
        let critical = kind[0].is_ascii_uppercase();
        if critical || PNG_KEPT_ANCILLARY.contains(&kind) {
            kept.extend_from_slice(chunk);
        } else if kind == b"eXIf" {
            if let Some(tiff) = kept_orientation(&chunk[8..8 + length]) {
                kept.extend_from_slice(&png_chunk(b"eXIf", &tiff)?);
            }
        }
        at = end;
        if kind == b"IEND" {
            return Some(kept);
        }
    }
}

fn jpeg(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut kept = vec![0xFF, 0xD8];
    let mut at = 2;
    loop {
        if *bytes.get(at)? != 0xFF {
            return None;
        }
        while *bytes.get(at + 1)? == 0xFF {
            at += 1;
        }
        let marker = *bytes.get(at + 1)?;
        match marker {
            0xD9 => {
                kept.extend_from_slice(&[0xFF, 0xD9]);
                return Some(kept);
            }
            0x01 | 0xD0..=0xD7 => {
                kept.extend_from_slice(&[0xFF, marker]);
                at += 2;
            }
            _ => {
                let length = usize::from(be_u16(bytes, at + 2)?);
                if length < 2 {
                    return None;
                }
                let end = at + 2 + length;
                let segment = bytes.get(at..end)?;
                let payload = &segment[4..];
                match marker {
                    0xE0 if payload.starts_with(b"JFIF\0") => kept.extend_from_slice(segment),
                    0xE1 if payload.starts_with(EXIF_HEADER) => {
                        if let Some(tiff) = kept_orientation(&payload[EXIF_HEADER.len()..]) {
                            let mut exif = EXIF_HEADER.to_vec();
                            exif.extend_from_slice(&tiff);
                            let length = u16::try_from(exif.len() + 2).ok()?;
                            kept.extend_from_slice(&[0xFF, 0xE1]);
                            kept.extend_from_slice(&length.to_be_bytes());
                            kept.extend_from_slice(&exif);
                        }
                    }
                    0xE2 if payload.starts_with(b"ICC_PROFILE\0") => {
                        kept.extend_from_slice(segment)
                    }
                    0xEE if payload.starts_with(b"Adobe") => kept.extend_from_slice(segment),
                    0xE0..=0xEF | 0xFE => {}
                    _ => kept.extend_from_slice(segment),
                }
                at = end;
                if marker == 0xDA {
                    let mut scan = at;
                    loop {
                        if *bytes.get(scan)? == 0xFF {
                            let next = *bytes.get(scan + 1)?;
                            if next != 0x00 && !(0xD0..=0xD7).contains(&next) {
                                break;
                            }
                            scan += 2;
                        } else {
                            scan += 1;
                        }
                    }
                    kept.extend_from_slice(&bytes[at..scan]);
                    at = scan;
                }
            }
        }
    }
}

fn webp_chunk(kind: &[u8; 4], payload: &[u8]) -> Option<Vec<u8>> {
    let length = u32::try_from(payload.len()).ok()?;
    let mut chunk = Vec::with_capacity(8 + payload.len() + 1);
    chunk.extend_from_slice(kind);
    chunk.extend_from_slice(&length.to_le_bytes());
    chunk.extend_from_slice(payload);
    if payload.len() % 2 == 1 {
        chunk.push(0);
    }
    Some(chunk)
}

fn webp(bytes: &[u8]) -> Option<Vec<u8>> {
    let size = usize::try_from(le_u32(bytes, 4)?).ok()?;
    let riff = bytes.get(..size.checked_add(8)?)?;
    if riff.get(12..16)? != b"VP8X" {
        return Some(riff.to_vec());
    }
    let mut extended: Option<Vec<u8>> = None;
    let mut body = Vec::with_capacity(riff.len());
    let mut flags_extra = 0u8;
    let mut colour = false;
    let mut at = 12;
    while at < riff.len() {
        let kind: &[u8; 4] = riff.get(at..at + 4)?.try_into().ok()?;
        let length = usize::try_from(le_u32(riff, at + 4)?).ok()?;
        let end = at + 8 + length;
        let payload = riff.get(at + 8..end)?;
        let padded = end + length % 2;
        if kind == b"VP8X" {
            extended = Some(payload.to_vec());
        } else if WEBP_KEPT.contains(&kind) {
            colour |= kind == b"ICCP";
            body.extend_from_slice(riff.get(at..padded.min(riff.len()))?);
        } else if kind == b"EXIF" {
            let tiff = payload.strip_prefix(EXIF_HEADER).unwrap_or(payload);
            if let Some(minimal) = kept_orientation(tiff) {
                flags_extra |= WEBP_EXIF;
                body.extend_from_slice(&webp_chunk(b"EXIF", &minimal)?);
            }
        }
        at = padded;
    }
    let mut extended = extended?;
    let first = extended.first_mut()?;
    *first &= !(WEBP_EXIF | WEBP_XMP | WEBP_COLOUR_PROFILE);
    *first |= flags_extra;
    if colour {
        *first |= WEBP_COLOUR_PROFILE;
    }
    let mut chunks = webp_chunk(b"VP8X", &extended)?;
    chunks.extend_from_slice(&body);
    let size = u32::try_from(4 + chunks.len()).ok()?;
    let mut kept = Vec::with_capacity(12 + chunks.len());
    kept.extend_from_slice(b"RIFF");
    kept.extend_from_slice(&size.to_le_bytes());
    kept.extend_from_slice(b"WEBP");
    kept.extend_from_slice(&chunks);
    Some(kept)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, ImageFormat as Encoding, Rgba, RgbaImage};

    const SECRET: &[u8] = b"SECRET 48.8584 N 2.2945 E serial 0042";
    const PROFILE: &[u8] = b"A stand-in ICC colour profile for the tests.";
    const WIDTH: u32 = 20;
    const HEIGHT: u32 = 12;

    fn picture() -> DynamicImage {
        DynamicImage::ImageRgba8(RgbaImage::from_fn(WIDTH, HEIGHT, |x, y| {
            Rgba([(x * 12) as u8, (y * 20) as u8, 70, 255])
        }))
    }

    fn pixels(bytes: &[u8]) -> Vec<u8> {
        image::load_from_memory(bytes)
            .expect("The picture decodes.")
            .to_rgba8()
            .into_raw()
    }

    fn holds(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }

    fn entry(tag: u16, kind: u16, count: u32, value: [u8; 4]) -> Vec<u8> {
        let mut entry = tag.to_le_bytes().to_vec();
        entry.extend_from_slice(&kind.to_le_bytes());
        entry.extend_from_slice(&count.to_le_bytes());
        entry.extend_from_slice(&value);
        entry
    }

    fn tiff(turn: Option<u16>) -> Vec<u8> {
        let first_count: u16 = if turn.is_some() { 3 } else { 2 };
        let first_size = 2 + 12 * usize::from(first_count) + 4;
        let make_at = 8 + first_size;
        let gps_at = make_at + SECRET.len() + SECRET.len() % 2;
        let gps_size = 2 + 12 * 2 + 4;
        let note_at = gps_at + gps_size;
        let as_u32 = |at: usize| u32::try_from(at).expect("Fits.").to_le_bytes();
        let count = u32::try_from(SECRET.len()).expect("Fits.");
        let mut tiff = vec![b'I', b'I', 42, 0];
        tiff.extend_from_slice(&8u32.to_le_bytes());
        tiff.extend_from_slice(&first_count.to_le_bytes());
        tiff.extend_from_slice(&entry(0x010F, 2, count, as_u32(make_at)));
        if let Some(turn) = turn {
            let mut value = [0; 4];
            value[..2].copy_from_slice(&turn.to_le_bytes());
            tiff.extend_from_slice(&entry(ORIENTATION, SHORT, 1, value));
        }
        tiff.extend_from_slice(&entry(0x8825, 4, 1, as_u32(gps_at)));
        tiff.extend_from_slice(&0u32.to_le_bytes());
        tiff.extend_from_slice(SECRET);
        if SECRET.len() % 2 == 1 {
            tiff.push(0);
        }
        tiff.extend_from_slice(&2u16.to_le_bytes());
        tiff.extend_from_slice(&entry(0x0001, 2, 2, [b'N', 0, 0, 0]));
        tiff.extend_from_slice(&entry(0x001B, 7, count, as_u32(note_at)));
        tiff.extend_from_slice(&0u32.to_le_bytes());
        tiff.extend_from_slice(SECRET);
        tiff
    }

    fn png_chunks(png: &[u8]) -> Vec<([u8; 4], Vec<u8>)> {
        let mut found = Vec::new();
        let mut at = 8;
        loop {
            let length = be_u32(png, at).expect("A length.") as usize;
            let kind: [u8; 4] = png[at + 4..at + 8].try_into().expect("A type.");
            found.push((kind, png[at + 8..at + 8 + length].to_vec()));
            let mut crc = flate2::Crc::new();
            crc.update(&png[at + 4..at + 8 + length]);
            assert_eq!(
                be_u32(png, at + 8 + length),
                Some(crc.sum()),
                "The CRC holds."
            );
            at += 12 + length;
            if &kind == b"IEND" {
                return found;
            }
        }
    }

    fn png_ends_at_its_end_chunk(png: &[u8]) -> bool {
        let mut at = 8;
        loop {
            let length = be_u32(png, at).expect("A length.") as usize;
            let end_chunk = &png[at + 4..at + 8] == b"IEND";
            at += 12 + length;
            if end_chunk {
                return at == png.len();
            }
        }
    }

    fn png_with_everything(turn: Option<u16>) -> Vec<u8> {
        let mut info = png::Info::with_size(WIDTH, HEIGHT);
        info.color_type = png::ColorType::Rgba;
        info.bit_depth = png::BitDepth::Eight;
        info.icc_profile = Some(PROFILE.into());
        let mut plain = Vec::new();
        {
            let mut encoder =
                png::Encoder::with_info(&mut plain, info).expect("The header is valid.");
            let secret = String::from_utf8(SECRET.to_vec()).expect("Text.");
            encoder
                .add_text_chunk("Software".to_string(), secret.clone())
                .expect("tEXt.");
            encoder
                .add_ztxt_chunk("Comment".to_string(), secret.clone())
                .expect("zTXt.");
            encoder
                .add_itxt_chunk("Description".to_string(), secret)
                .expect("iTXt.");
            encoder.set_pixel_dims(Some(png::PixelDimensions {
                xppu: 2835,
                yppu: 2835,
                unit: png::Unit::Meter,
            }));
            let mut writer = encoder.write_header().expect("The header is written.");
            writer
                .write_image_data(picture().to_rgba8().as_raw())
                .expect("The pixels are written.");
            writer.finish().expect("The PNG is finished.");
        }
        let header_end = 8 + 25;
        let mut bytes = plain[..header_end].to_vec();
        bytes.extend(png_chunk(b"eXIf", &tiff(turn)).expect("eXIf."));
        bytes.extend(png_chunk(b"tIME", &[0x07, 0xEA, 11, 5, 1, 2, 3]).expect("tIME."));
        bytes.extend(png_chunk(b"caBX", SECRET).expect("caBX."));
        bytes.extend_from_slice(&plain[header_end..]);
        bytes.extend_from_slice(SECRET);
        bytes
    }

    fn segment(marker: u8, payload: &[u8]) -> Vec<u8> {
        let mut segment = vec![0xFF, marker];
        segment.extend_from_slice(
            &u16::try_from(payload.len() + 2)
                .expect("Fits.")
                .to_be_bytes(),
        );
        segment.extend_from_slice(payload);
        segment
    }

    fn jpeg_segments(jpeg: &[u8]) -> Vec<(u8, Vec<u8>)> {
        let mut found = Vec::new();
        let mut at = 2;
        loop {
            let marker = jpeg[at + 1];
            if marker == 0xD9 {
                found.push((marker, Vec::new()));
                return found;
            }
            let length = usize::from(be_u16(jpeg, at + 2).expect("A length."));
            found.push((marker, jpeg[at + 4..at + 2 + length].to_vec()));
            at += 2 + length;
            if marker == 0xDA {
                while !(jpeg[at] == 0xFF
                    && jpeg[at + 1] != 0
                    && !(0xD0..=0xD7).contains(&jpeg[at + 1]))
                {
                    at += 1;
                }
            }
        }
    }

    fn jpeg_with_everything(turn: Option<u16>) -> Vec<u8> {
        let mut plain = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut plain, 90)
            .encode_image(&picture().to_rgb8())
            .expect("The JPEG encodes.");
        let mut exif = EXIF_HEADER.to_vec();
        exif.extend(tiff(turn));
        let mut xmp = b"http://ns.adobe.com/xap/1.0/\0<x:xmpmeta>".to_vec();
        xmp.extend_from_slice(SECRET);
        xmp.extend_from_slice(b"</x:xmpmeta>");
        let mut icc = b"ICC_PROFILE\0\x01\x01".to_vec();
        icc.extend_from_slice(PROFILE);
        let mut photoshop = b"Photoshop 3.0\0".to_vec();
        photoshop.extend_from_slice(SECRET);
        let mut bytes = plain[..2].to_vec();
        bytes.extend(segment(0xE1, &exif));
        bytes.extend(segment(0xE1, &xmp));
        bytes.extend(segment(0xE2, &icc));
        bytes.extend(segment(0xED, &photoshop));
        bytes.extend(segment(0xFE, SECRET));
        bytes.extend_from_slice(&plain[2..plain.len() - 2]);
        bytes.extend(segment(0xFE, SECRET));
        bytes.extend_from_slice(&[0xFF, 0xD9]);
        bytes.extend_from_slice(SECRET);
        bytes
    }

    fn webp_chunks(webp: &[u8]) -> Vec<([u8; 4], Vec<u8>)> {
        let size = le_u32(webp, 4).expect("A size.") as usize;
        assert_eq!(
            size + 8,
            webp.len(),
            "The RIFF size counts every byte after it."
        );
        let mut found = Vec::new();
        let mut at = 12;
        while at < webp.len() {
            let kind: [u8; 4] = webp[at..at + 4].try_into().expect("A type.");
            let length = le_u32(webp, at + 4).expect("A length.") as usize;
            found.push((kind, webp[at + 8..at + 8 + length].to_vec()));
            at += 8 + length + length % 2;
        }
        found
    }

    fn lossless_picture_chunk() -> Vec<u8> {
        let mut simple = std::io::Cursor::new(Vec::new());
        picture()
            .write_to(&mut simple, Encoding::WebP)
            .expect("The WebP encodes.");
        let simple = simple.into_inner();
        assert_eq!(&simple[12..16], b"VP8L");
        simple[12..].to_vec()
    }

    fn webp_with_everything(turn: Option<u16>) -> Vec<u8> {
        let mut canvas = vec![WEBP_COLOUR_PROFILE | WEBP_EXIF | WEBP_XMP, 0, 0, 0];
        canvas.extend_from_slice(&(WIDTH - 1).to_le_bytes()[..3]);
        canvas.extend_from_slice(&(HEIGHT - 1).to_le_bytes()[..3]);
        let mut xmp = b"<x:xmpmeta>".to_vec();
        xmp.extend_from_slice(SECRET);
        let mut chunks = webp_chunk(b"VP8X", &canvas).expect("VP8X.");
        chunks.extend(webp_chunk(b"ICCP", PROFILE).expect("ICCP."));
        chunks.extend(lossless_picture_chunk());
        chunks.extend(webp_chunk(b"EXIF", &tiff(turn)).expect("EXIF."));
        chunks.extend(webp_chunk(b"XMP ", &xmp).expect("XMP."));
        chunks.extend(webp_chunk(b"SECR", SECRET).expect("Unknown."));
        let mut bytes = b"RIFF".to_vec();
        bytes.extend_from_slice(
            &u32::try_from(4 + chunks.len())
                .expect("Fits.")
                .to_le_bytes(),
        );
        bytes.extend_from_slice(b"WEBP");
        bytes.extend(chunks);
        bytes
    }

    #[test]
    fn the_fixtures_carry_every_kind_of_metadata_and_still_decode() {
        for (name, bytes) in [
            ("png", png_with_everything(Some(6))),
            ("jpeg", jpeg_with_everything(Some(6))),
            ("webp", webp_with_everything(Some(6))),
        ] {
            assert!(holds(&bytes, SECRET), "{name}");
            assert_eq!(
                pixels(&bytes).len(),
                (WIDTH * HEIGHT * 4) as usize,
                "{name}"
            );
        }
        assert_eq!(orientation(&tiff(Some(6))), Some(6));
        assert_eq!(orientation(&tiff(None)), None);
    }

    #[test]
    fn a_png_keeps_its_pixels_and_colour_profile_and_nothing_else() {
        let source = png_with_everything(None);
        let kept = without_metadata(&source).expect("The PNG is read.");
        assert_eq!(pixels(&kept), pixels(&source));
        assert!(!holds(&kept, SECRET));
        assert!(
            png_ends_at_its_end_chunk(&kept),
            "Nothing follows the end chunk."
        );
        assert!(
            !png_ends_at_its_end_chunk(&source),
            "The fixture carries a trailer."
        );
        let kinds: Vec<[u8; 4]> = png_chunks(&kept)
            .into_iter()
            .map(|(kind, _)| kind)
            .collect();
        assert_eq!(kinds.first(), Some(b"IHDR"));
        assert_eq!(kinds.last(), Some(b"IEND"));
        assert!(
            kinds
                .iter()
                .all(|kind| matches!(kind, b"IHDR" | b"iCCP" | b"IDAT" | b"IEND")),
            "{:?}",
            kinds
                .iter()
                .map(|kind| String::from_utf8_lossy(kind).into_owned())
                .collect::<Vec<_>>()
        );
        let profile = |png: &[u8]| {
            png_chunks(png)
                .into_iter()
                .find(|(kind, _)| kind == b"iCCP")
        };
        assert_eq!(profile(&kept), profile(&source));
        assert!(profile(&kept).is_some());
    }

    #[test]
    fn a_png_keeps_only_its_orientation_from_its_exif() {
        let kept = without_metadata(&png_with_everything(Some(8))).expect("The PNG is read.");
        let exif = png_chunks(&kept)
            .into_iter()
            .find(|(kind, _)| kind == b"eXIf")
            .expect("The orientation is kept.")
            .1;
        assert_eq!(exif, orientation_only(8));
        assert_eq!(orientation(&exif), Some(8));
        assert!(!holds(&kept, SECRET));
    }

    #[test]
    fn a_jpeg_keeps_its_pixels_and_colour_profile_and_nothing_else() {
        let source = jpeg_with_everything(None);
        let kept = without_metadata(&source).expect("The JPEG is read.");
        assert_eq!(pixels(&kept), pixels(&source));
        assert!(!holds(&kept, SECRET));
        assert!(
            kept.ends_with(&[0xFF, 0xD9]),
            "Nothing follows the end marker."
        );
        let segments = jpeg_segments(&kept);
        for (marker, payload) in &segments {
            assert!(
                *marker != 0xE1 && *marker != 0xED && *marker != 0xFE,
                "{marker:#x}"
            );
            if (0xE0..=0xEF).contains(marker) {
                assert!(
                    payload.starts_with(b"JFIF\0") || payload.starts_with(b"ICC_PROFILE\0"),
                    "{marker:#x}"
                );
            }
        }
        let profile = |jpeg: &[u8]| {
            jpeg_segments(jpeg)
                .into_iter()
                .find(|(marker, payload)| *marker == 0xE2 && payload.starts_with(b"ICC_PROFILE\0"))
        };
        assert_eq!(profile(&kept), profile(&source));
        assert!(profile(&kept).is_some());
    }

    #[test]
    fn a_jpeg_keeps_only_its_orientation_from_its_exif() {
        let kept = without_metadata(&jpeg_with_everything(Some(6))).expect("The JPEG is read.");
        let exif = jpeg_segments(&kept)
            .into_iter()
            .find(|(marker, _)| *marker == 0xE1)
            .expect("The orientation is kept.")
            .1;
        let mut expected = EXIF_HEADER.to_vec();
        expected.extend(orientation_only(6));
        assert_eq!(exif, expected);
        assert!(!holds(&kept, SECRET));
    }

    #[test]
    fn a_webp_keeps_its_pixels_and_colour_profile_and_nothing_else() {
        let source = webp_with_everything(None);
        let kept = without_metadata(&source).expect("The WebP is read.");
        assert_eq!(pixels(&kept), pixels(&source));
        assert!(!holds(&kept, SECRET));
        let chunks = webp_chunks(&kept);
        let kinds: Vec<[u8; 4]> = chunks.iter().map(|(kind, _)| *kind).collect();
        assert_eq!(kinds, [*b"VP8X", *b"ICCP", *b"VP8L"]);
        assert_eq!(chunks[0].1[0], WEBP_COLOUR_PROFILE);
        assert_eq!(chunks[1].1, PROFILE);
    }

    #[test]
    fn a_webp_keeps_only_its_orientation_from_its_exif() {
        let kept = without_metadata(&webp_with_everything(Some(3))).expect("The WebP is read.");
        let chunks = webp_chunks(&kept);
        let kinds: Vec<[u8; 4]> = chunks.iter().map(|(kind, _)| *kind).collect();
        assert_eq!(kinds, [*b"VP8X", *b"ICCP", *b"VP8L", *b"EXIF"]);
        assert_eq!(chunks[0].1[0], WEBP_COLOUR_PROFILE | WEBP_EXIF);
        assert_eq!(chunks[3].1, orientation_only(3));
        assert!(!holds(&kept, SECRET));
    }

    #[test]
    fn an_upright_orientation_is_left_behind_with_the_rest() {
        let png = without_metadata(&png_with_everything(Some(1))).expect("The PNG is read.");
        assert!(png_chunks(&png).iter().all(|(kind, _)| kind != b"eXIf"));
        let jpeg = without_metadata(&jpeg_with_everything(Some(1))).expect("The JPEG is read.");
        assert!(
            jpeg_segments(&jpeg)
                .iter()
                .all(|(marker, _)| *marker != 0xE1)
        );
        let webp = without_metadata(&webp_with_everything(Some(1))).expect("The WebP is read.");
        assert!(webp_chunks(&webp).iter().all(|(kind, _)| kind != b"EXIF"));
        assert_eq!(webp_chunks(&webp)[0].1[0], WEBP_COLOUR_PROFILE);
    }

    #[test]
    fn a_big_endian_orientation_is_read() {
        let mut tiff = vec![b'M', b'M', 0, 42, 0, 0, 0, 8, 0, 1];
        tiff.extend_from_slice(&ORIENTATION.to_be_bytes());
        tiff.extend_from_slice(&SHORT.to_be_bytes());
        tiff.extend_from_slice(&1u32.to_be_bytes());
        tiff.extend_from_slice(&[0, 5, 0, 0, 0, 0, 0, 0]);
        assert_eq!(orientation(&tiff), Some(5));
        assert_eq!(kept_orientation(&tiff), Some(orientation_only(5)));
    }

    #[test]
    fn an_image_without_metadata_comes_back_byte_for_byte() {
        let mut png = std::io::Cursor::new(Vec::new());
        picture()
            .write_to(&mut png, Encoding::Png)
            .expect("Encodes.");
        let png = png.into_inner();
        assert_eq!(without_metadata(&png).as_deref(), Some(&png[..]));
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 90)
            .encode_image(&picture().to_rgb8())
            .expect("Encodes.");
        assert_eq!(without_metadata(&jpeg).as_deref(), Some(&jpeg[..]));
        let mut webp = std::io::Cursor::new(Vec::new());
        picture()
            .write_to(&mut webp, Encoding::WebP)
            .expect("Encodes.");
        let webp = webp.into_inner();
        assert_eq!(without_metadata(&webp).as_deref(), Some(&webp[..]));
    }

    #[test]
    fn a_broken_or_foreign_file_is_not_read() {
        let png = png_with_everything(None);
        let jpeg = jpeg_with_everything(None);
        let webp = webp_with_everything(None);
        for (name, bytes) in [
            ("half a png", png[..png.len() / 2].to_vec()),
            ("half a jpeg", jpeg[..jpeg.len() / 2].to_vec()),
            ("a webp whose size runs past its end", {
                let mut short = webp.clone();
                short.truncate(webp.len() - 10);
                short
            }),
            ("a gif", b"GIF89a\x01\x00\x01\x00".to_vec()),
            ("text", b"These bytes are not a picture.".to_vec()),
        ] {
            assert_eq!(without_metadata(&bytes), None, "{name}");
        }
    }
}

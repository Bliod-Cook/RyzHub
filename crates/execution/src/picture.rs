//! Images for models. Files the Chat model asks to see are read and decoded inside the sandbox;
//! uploaded images are decoded by `render-image`, a process of its own with bounded memory and
//! CPU. The server process never parses untrusted image data. A file a provider accepts as it
//! is passes through unchanged; anything else is decoded, turned upright, scaled to fit and
//! re-encoded.
use base64::Engine;
use image::{
    DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits, RgbImage,
    codecs::jpeg::JpegEncoder, imageops::FilterType, metadata::Orientation,
};
use serde_json::{Value, json};
use std::{
    io::{Cursor, Read},
    path::Path,
};

/// Largest file read; bigger files are refused before decoding.
const MAX_SOURCE: u64 = 64 * 1024 * 1024;
/// Decoder bounds, well inside a sandbox's memory.
const MAX_SIDE: u32 = 16_384;
const MAX_ALLOC: u64 = 384 * 1024 * 1024;
/// JPEG qualities tried in order once a lossless encoding is too large.
const QUALITIES: [u8; 3] = [90, 75, 60];
/// Address space and CPU time of a `render-image` process.
const CHILD_MEMORY: u64 = 2 * 1024 * 1024 * 1024;
const CHILD_CPU_SECS: u64 = 30;

type Refusal = (&'static str, String);

/// The image at `path` (relative paths start at `/workspace`), at most `max_edge` pixels on its
/// long side and `max_bytes` encoded. Failures are answers for the model, never errors.
pub fn read(path: &str, max_edge: u32, max_bytes: usize) -> Value {
    answer(load(
        &Path::new("/workspace").join(path),
        max_edge,
        max_bytes,
    ))
}

/// The image in `bytes`, rendered as [`read`] renders a file.
pub fn render(bytes: &[u8], max_edge: u32, max_bytes: usize) -> Value {
    answer(decode(bytes, max_edge, max_bytes))
}

/// `render-image <max_edge> <max_bytes>`: render the image on stdin to stdout as [`render`]
/// does. Run as a process of its own, so a hostile file can exhaust only this process.
pub fn child_main(args: &[String]) -> anyhow::Result<()> {
    use anyhow::Context;
    use nix::sys::resource::{Resource, setrlimit};
    setrlimit(Resource::RLIMIT_AS, CHILD_MEMORY, CHILD_MEMORY)?;
    setrlimit(Resource::RLIMIT_CPU, CHILD_CPU_SECS, CHILD_CPU_SECS)?;
    let max_edge: u32 = args.first().context("max_edge")?.parse()?;
    let max_bytes: usize = args.get(1).context("max_bytes")?.parse()?;
    let mut bytes = Vec::new();
    std::io::stdin()
        .lock()
        .take(MAX_SOURCE + 1)
        .read_to_end(&mut bytes)?;
    let value = if bytes.len() as u64 > MAX_SOURCE {
        json!({"error": "too_large", "message": format!("files over {MAX_SOURCE} bytes are not read")})
    } else {
        render(&bytes, max_edge, max_bytes)
    };
    serde_json::to_writer(std::io::stdout().lock(), &value)?;
    Ok(())
}

fn answer(result: Result<Value, Refusal>) -> Value {
    result.unwrap_or_else(|(code, message)| json!({"error": code, "message": message}))
}

fn load(path: &Path, max_edge: u32, max_bytes: usize) -> Result<Value, Refusal> {
    let metadata = std::fs::metadata(path).map_err(|error| ("not_found", error.to_string()))?;
    if !metadata.is_file() {
        return Err(("not_a_file", format!("{} is not a file", path.display())));
    }
    if metadata.len() > MAX_SOURCE {
        return Err((
            "too_large",
            format!("files over {MAX_SOURCE} bytes are not read"),
        ));
    }
    let bytes = std::fs::read(path).map_err(|error| ("unreadable", error.to_string()))?;
    decode(&bytes, max_edge, max_bytes)
}

fn decode(bytes: &[u8], max_edge: u32, max_bytes: usize) -> Result<Value, Refusal> {
    let format = image::guess_format(bytes).map_err(|_| {
        if looks_like_svg(bytes) {
            (
                "unsupported_format",
                "SVG is not supported; render it to PNG first".to_owned(),
            )
        } else {
            (
                "not_an_image",
                "not a PNG, JPEG, GIF, WebP or BMP image".to_owned(),
            )
        }
    })?;
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_SIDE);
    limits.max_image_height = Some(MAX_SIDE);
    limits.max_alloc = Some(MAX_ALLOC);
    reader.limits(limits);
    let unsupported = |error: image::ImageError| ("unsupported_format", error.to_string());
    let mut decoder = reader.into_decoder().map_err(unsupported)?;
    let (width, height) = decoder.dimensions();
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let native = match format {
        ImageFormat::Png => Some("image/png"),
        ImageFormat::Jpeg => Some("image/jpeg"),
        ImageFormat::WebP => Some("image/webp"),
        // GIF may be animated, which not every provider accepts; BMP no provider accepts.
        _ => None,
    };
    if let Some(media_type) = native
        && orientation == Orientation::NoTransforms
        && width.max(height) <= max_edge
        && bytes.len() <= max_bytes
    {
        return Ok(reply(media_type, bytes, (width, height), (width, height)));
    }
    let mut image = DynamicImage::from_decoder(decoder).map_err(unsupported)?;
    image.apply_orientation(orientation);
    let original = (image.width(), image.height());
    if original.0.max(original.1) > max_edge {
        image = image.resize(max_edge, max_edge, FilterType::CatmullRom);
    }
    loop {
        if let Some((media_type, encoded)) = encode(&image, max_bytes) {
            return Ok(reply(
                media_type,
                &encoded,
                (image.width(), image.height()),
                original,
            ));
        }
        let (width, height) = (image.width() * 3 / 4, image.height() * 3 / 4);
        if width.max(height) < 64 {
            return Err((
                "too_large",
                "the image cannot be encoded small enough".into(),
            ));
        }
        image = image.resize(width, height, FilterType::CatmullRom);
    }
}

/// Lossless PNG keeps text sharp; photographs that do not fit fall back to JPEG.
fn encode(image: &DynamicImage, max_bytes: usize) -> Option<(&'static str, Vec<u8>)> {
    let mut png = Vec::new();
    image
        .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
        .ok()?;
    if png.len() <= max_bytes {
        return Some(("image/png", png));
    }
    let flat = flatten(image);
    QUALITIES.iter().find_map(|&quality| {
        let mut jpeg = Vec::new();
        JpegEncoder::new_with_quality(&mut jpeg, quality)
            .encode_image(&flat)
            .ok()?;
        (jpeg.len() <= max_bytes).then_some(("image/jpeg", jpeg))
    })
}

/// The image over a white background, for formats without transparency.
fn flatten(image: &DynamicImage) -> RgbImage {
    if !image.color().has_alpha() {
        return image.to_rgb8();
    }
    let rgba = image.to_rgba8();
    RgbImage::from_fn(rgba.width(), rgba.height(), |x, y| {
        let [r, g, b, a] = rgba.get_pixel(x, y).0;
        let over = |c: u8| ((c as u16 * a as u16 + 255 * (255 - a as u16)) / 255) as u8;
        image::Rgb([over(r), over(g), over(b)])
    })
}

fn looks_like_svg(bytes: &[u8]) -> bool {
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(1024)]).to_ascii_lowercase();
    head.contains("<svg")
}

fn reply(media_type: &str, data: &[u8], size: (u32, u32), original: (u32, u32)) -> Value {
    json!({
        "media_type": media_type,
        "data": base64::engine::general_purpose::STANDARD.encode(data),
        "width": size.0,
        "height": size.1,
        "original_width": original.0,
        "original_height": original.1,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgba, RgbaImage};

    fn decoded(value: &Value) -> DynamicImage {
        let data = base64::engine::general_purpose::STANDARD
            .decode(value["data"].as_str().unwrap())
            .unwrap();
        image::load_from_memory(&data).unwrap()
    }

    fn save(dir: &Path, name: &str, image: &DynamicImage, format: ImageFormat) -> String {
        let path = dir.join(name);
        image.save_with_format(&path, format).unwrap();
        path.to_str().unwrap().to_owned()
    }

    #[test]
    fn small_png_passes_through_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let image = DynamicImage::ImageRgba8(RgbaImage::from_pixel(40, 20, Rgba([1, 2, 3, 128])));
        let path = save(dir.path(), "dot.png", &image, ImageFormat::Png);
        let value = read(&path, 2000, 1 << 20);
        let data = base64::engine::general_purpose::STANDARD
            .decode(value["data"].as_str().unwrap())
            .unwrap();
        assert_eq!(data, std::fs::read(&path).unwrap());
        assert_eq!(
            (value["media_type"].as_str(), value["width"].as_u64()),
            (Some("image/png"), Some(40))
        );
    }

    #[test]
    fn large_images_scale_to_the_long_edge_and_keep_their_aspect() {
        let dir = tempfile::tempdir().unwrap();
        let image =
            DynamicImage::ImageRgb8(RgbImage::from_pixel(3000, 1500, image::Rgb([9, 9, 9])));
        let path = save(dir.path(), "wide.jpg", &image, ImageFormat::Jpeg);
        let value = read(&path, 2000, 1 << 20);
        assert_eq!(
            (value["width"].as_u64(), value["height"].as_u64()),
            (Some(2000), Some(1000))
        );
        assert_eq!(value["original_width"].as_u64(), Some(3000));
        assert_eq!(decoded(&value).width(), 2000);
    }

    #[test]
    fn gif_and_bmp_are_reencoded_and_noise_falls_back_to_jpeg() {
        let dir = tempfile::tempdir().unwrap();
        let image = DynamicImage::ImageRgba8(RgbaImage::from_pixel(30, 30, Rgba([200, 0, 0, 255])));
        let gif = read(
            &save(dir.path(), "a.gif", &image, ImageFormat::Gif),
            2000,
            1 << 20,
        );
        assert_eq!(gif["media_type"], "image/png");
        let bmp = read(
            &save(dir.path(), "a.bmp", &image, ImageFormat::Bmp),
            2000,
            1 << 20,
        );
        assert_eq!(bmp["media_type"], "image/png");
        // Noise does not compress losslessly under the limit.
        let mut seed = 1u32;
        let noise = RgbImage::from_fn(400, 400, |_, _| {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
            let [a, b, c, _] = seed.to_le_bytes();
            image::Rgb([a, b, c])
        });
        let path = save(
            dir.path(),
            "noise.bmp",
            &DynamicImage::ImageRgb8(noise),
            ImageFormat::Bmp,
        );
        let value = read(&path, 2000, 200_000);
        assert_eq!(value["media_type"], "image/jpeg");
        let data = value["data"].as_str().unwrap();
        assert!(
            base64::engine::general_purpose::STANDARD
                .decode(data)
                .unwrap()
                .len()
                <= 200_000
        );
    }

    #[test]
    fn rendering_bytes_matches_reading_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let image = DynamicImage::ImageRgb8(RgbImage::from_pixel(2400, 600, image::Rgb([5, 6, 7])));
        let path = save(dir.path(), "wide.bmp", &image, ImageFormat::Bmp);
        let from_file = read(&path, 2000, 1 << 20);
        assert_eq!(
            render(&std::fs::read(&path).unwrap(), 2000, 1 << 20),
            from_file
        );
        assert_eq!(from_file["width"].as_u64(), Some(2000));
        assert_eq!(
            render(b"plain text", 2000, 1 << 20)["error"],
            "not_an_image"
        );
    }

    #[test]
    fn refusals_are_answers() {
        let dir = tempfile::tempdir().unwrap();
        let text = dir.path().join("notes.txt");
        std::fs::write(&text, "hello").unwrap();
        assert_eq!(
            read(text.to_str().unwrap(), 2000, 1 << 20)["error"],
            "not_an_image"
        );
        let svg = dir.path().join("a.svg");
        std::fs::write(
            &svg,
            "<?xml version=\"1.0\"?><svg xmlns=\"http://www.w3.org/2000/svg\"/>",
        )
        .unwrap();
        assert_eq!(
            read(svg.to_str().unwrap(), 2000, 1 << 20)["error"],
            "unsupported_format"
        );
        let missing = dir.path().join("missing.png");
        assert_eq!(
            read(missing.to_str().unwrap(), 2000, 1 << 20)["error"],
            "not_found"
        );
        assert_eq!(
            read(dir.path().to_str().unwrap(), 2000, 1 << 20)["error"],
            "not_a_file"
        );
    }
}

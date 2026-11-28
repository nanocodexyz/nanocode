//! Kimi Code's `ReadMediaFileTool` and its image pipeline (`compressImageForModel`, `cropImageForModel`,
//! `encodeWithinBudget`). Kimi Code processes images with Jimp; this ports Jimp's shrinking resize and
//! pngjs's encoder, so PNGs come out byte-identical, and uses nanocode-image to decode and to encode JPEG,
//! whose bytes (and so sizes) differ slightly from jpeg-js's. `kimi-file://` attachment references are not
//! supported.

use super::files::{IMAGE_SUFFIXES, VIDEO_SUFFIXES, file_kind, fs_error};
use super::io::{self, Kind};
use super::{Access, Builtins, Outcome, input};
use nanocode_image::{Encoding, Format, Image, Pixels};
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::Read as _;

const MAX_MEDIA_BYTES: u64 = 104_857_600;
const MAX_DECODE_BYTES: u64 = 67_108_864;
const MAX_DECODE_PIXELS: i64 = 100_000_000;
/// `resolveReadImageByteBudget()` and `resolveMaxImageEdgePx()` defaults.
const READ_BYTE_BUDGET: usize = 262_144;
const MAX_EDGE: u32 = 2000;
/// `DEFAULT_INLINE_IMAGE_BYTE_BUDGET`, the budget of region crops.
const CROP_BYTE_BUDGET: usize = 3_932_160;
const JPEG_QUALITIES: [u8; 4] = [80, 60, 40, 20];
const FALLBACK_EDGES: [u32; 6] = [2000, 1000, 768, 512, 384, 256];
const PNG_RESCALE_FLOOR: u32 = 1000;
/// `KIMI_IMAGE_POLICY.acceptedMimes`: other formats reach the model as their saved path.
const KIMI_IMAGE_MIMES: [&str; 7] = [
    "image/png",
    "image/jpeg",
    "image/gif",
    "image/webp",
    "image/bmp",
    "image/heic",
    "image/heif",
];

#[derive(Deserialize)]
struct MediaInput {
    path: String,
    region: Option<Region>,
    #[serde(default)]
    full_resolution: bool,
}

#[derive(Clone, Copy, Deserialize)]
struct Region {
    x: u64,
    y: u64,
    width: u64,
    height: u64,
}

enum Delivery {
    Untouched,
    Full,
    Downsampled(u32, u32, usize, &'static str),
    Crop(Region, Option<(u32, u32)>),
}

/// Kimi Code's `ReadMediaFileTool.execution`.
pub async fn read(builtins: &Builtins, args: &Value) -> Outcome {
    let args: MediaInput = match input("ReadMediaFile", args) {
        Ok(args) => args,
        Err(error) => return error,
    };
    if args.path.is_empty() {
        return Outcome::error("File path cannot be empty.");
    }
    let path = match builtins.resolve("ReadMediaFile", &args.path, Access::Read) {
        Ok(path) => path,
        Err(error) => return error,
    };
    let shown = &args.path;
    let failed = |error: std::io::Error| {
        Outcome::error(format!(
            "Failed to read {shown}: {}",
            fs_error(&error, "read")
        ))
    };
    // ponytail: a host has no partial reads, so the whole file is read up front and once more below;
    // add a ranged read to `FileHost` if large media over a host gets slow.
    let mut header = Vec::with_capacity(512);
    let opened = match builtins.host() {
        Some(host) => io::read(Some(host), &path).await.map(|bytes| {
            header.extend_from_slice(&bytes[..bytes.len().min(512)]);
        }),
        None => std::fs::File::open(&path)
            .and_then(|file| file.take(512).read_to_end(&mut header))
            .map(drop),
    };
    if let Err(error) = opened {
        return failed(error);
    }
    let name = path.to_string_lossy();
    let (kind, mime) = file_kind(&name, &header, true);
    match kind {
        "text" => {
            return Outcome::error(format!(
                "\"{shown}\" is a text file. Use Read to read text files."
            ));
        }
        "unknown" => {
            return Outcome::error(format!(
                "\"{shown}\" is not a supported image or video file. Use Read for text files, or Bash or an MCP tool for other binary formats."
            ));
        }
        _ => {}
    }
    let size = match io::kind(builtins.host(), &path).await {
        Ok(Kind::File(size)) => size,
        Ok(_) => return failed(std::io::ErrorKind::NotFound.into()),
        Err(error) => return failed(error),
    };
    let image = kind == "image";
    let options = args.region.is_some() || args.full_resolution;
    let too_large = |bytes: u64| {
        Outcome::error(format!(
            "Image is too large to send safely after compression ({bytes} bytes; limit {READ_BYTE_BUDGET} bytes and {MAX_EDGE}px on the longest edge). The original image was not sent to the model. Do not retry the same file unchanged. Use Bash or an available image-processing tool to create a smaller copy within both limits, then call ReadMediaFile on the smaller copy."
        ))
    };
    if size == 0 {
        return Outcome::error(format!("\"{shown}\" is empty."));
    }
    if size > MAX_MEDIA_BYTES {
        return Outcome::error(format!(
            "\"{shown}\" is {size} bytes, which exceeds the maximum 100MB for media files."
        ));
    }
    if !image && options {
        return Outcome::error("region and full_resolution apply only to image files.");
    }
    if image && options && size > MAX_DECODE_BYTES {
        return Outcome::error(format!(
            "Image is too large to process safely for region or full_resolution ({size} bytes; safe decode limit {MAX_DECODE_BYTES} bytes). The original image was not sent to the model. Do not retry the same file unchanged. Use Bash or an available image-processing tool to create a smaller copy or crop the needed region into a separate image, then call ReadMediaFile on the resulting file."
        ));
    }
    if image && args.region.is_none() && args.full_resolution && size > READ_BYTE_BUDGET as u64 {
        return Outcome::error(format!(
            "\"{shown}\" is {size} bytes ({}), over the {READ_BYTE_BUDGET}-byte ({}) per-image limit, so full_resolution cannot be honored. Use region to view a crop at full fidelity instead.",
            byte_size(size as usize),
            byte_size(READ_BYTE_BUDGET)
        ));
    }
    if image && !options && size > MAX_DECODE_BYTES {
        return too_large(size);
    }
    let data = match io::read(builtins.host(), &path).await {
        Ok(data) => data,
        Err(error) => return failed(error),
    };
    let mut dimensions = image.then(|| sniff_dimensions(&data)).flatten();
    let (bytes, out_mime, delivery) = match (image, args.region) {
        (false, _) => (data, mime, None),
        (true, Some(region)) => match crop(&data, mime, region, args.full_resolution) {
            Ok((encoded, original, applied)) => {
                dimensions = Some(original);
                let size = (encoded.width, encoded.height);
                let resized =
                    (u64::from(size.0), u64::from(size.1)) != (applied.width, applied.height);
                let delivery = Delivery::Crop(applied, resized.then_some(size));
                (encoded.data, encoded.mime, Some(delivery))
            }
            Err(error) => {
                return Outcome::error(format!("Cannot read region from \"{shown}\": {error}"));
            }
        },
        (true, None) if args.full_resolution => (data, mime, Some(Delivery::Full)),
        (true, None) => match compress(&data, mime, dimensions) {
            Some((encoded, original)) => {
                if encoded.data.len() > READ_BYTE_BUDGET {
                    return too_large(encoded.data.len() as u64);
                }
                dimensions = Some(original);
                let Encoded {
                    data,
                    mime,
                    width,
                    height,
                } = encoded;
                let delivery = Delivery::Downsampled(width, height, data.len(), mime);
                (data, mime, Some(delivery))
            }
            None if recodable(&data, mime)
                && (data.len() > READ_BYTE_BUDGET
                    || dimensions.is_some_and(|(w, h)| w.max(h) > i64::from(MAX_EDGE))) =>
            {
                return too_large(data.len() as u64);
            }
            None => (data, mime, Some(Delivery::Untouched)),
        },
    };
    let part = match kind {
        "image" if KIMI_IMAGE_MIMES.contains(&out_mime) => json!({
            "type": "image_url",
            "image_url": {"url": format!("data:{out_mime};base64,{}", nanocode_sys::base64::STANDARD.encode(&bytes))},
        }),
        "image" => {
            json!({"type": "image_url", "image_url": {"url": save(builtins, &bytes, out_mime, &name).await}})
        }
        _ => {
            json!({"type": "video_url", "video_url": {"url": save(builtins, &bytes, out_mime, &name).await}})
        }
    };
    Outcome {
        output: format!("<{kind} path=\"{name}\">"),
        note: Some(note(kind, mime, size, dimensions, delivery.as_ref())),
        media: vec![part, json!({"type": "text", "text": format!("</{kind}>")})],
        ..Outcome::default()
    }
}

/// Kimi Code's `SessionMediaStoreService.put`: saves the bytes as `f_<sha256(mime NUL bytes)><ext>` in the
/// session's media directory and returns the path, or nothing when there is no store.
async fn save(builtins: &Builtins, bytes: &[u8], mime: &str, name: &str) -> String {
    let file = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let ext = match file.rfind('.') {
        Some(dot) if dot > 0 => &file[dot..],
        _ => VIDEO_SUFFIXES
            .iter()
            .chain(&IMAGE_SUFFIXES)
            .find(|(_, known)| *known == mime)
            .map_or(".bin", |(suffix, _)| suffix),
    };
    let mut hash = nanocode_digest::Sha256::new();
    for chunk in [mime.as_bytes(), b"\0", bytes] {
        hash.update(chunk);
    }
    let id = hash
        .finalize()
        .iter()
        .fold(String::from("f_"), |mut id, byte| {
            id.push_str(&format!("{byte:02x}"));
            id
        });
    let Some(directory) = &builtins.media else {
        return String::new();
    };
    let path = directory.join(format!("{id}{ext}"));
    let saved = io::store(builtins.host(), &path, bytes).await;
    saved.map_or_else(|_| String::new(), |()| path.display().to_string())
}

/// Kimi Code's `buildMediaNote`.
fn note(
    kind: &str,
    mime: &str,
    size: u64,
    dimensions: Option<(i64, i64)>,
    delivery: Option<&Delivery>,
) -> String {
    let mut parts = vec![
        format!("Read {kind} file."),
        format!("Mime type: {mime}."),
        format!("Size: {size} bytes."),
    ];
    if let Some((width, height)) = dimensions {
        parts.push(format!("Original dimensions: {width}x{height} pixels."));
    }
    match delivery {
        Some(Delivery::Downsampled(width, height, bytes, mime)) => parts.extend([
            format!(
                "The attached image was downsampled to {width}x{height} pixels ({mime}, {}) to fit model limits; fine detail may be lost.",
                byte_size(*bytes)
            ),
            "To inspect fine detail, call ReadMediaFile again with the region parameter (original-image pixel coordinates) to view a crop at full fidelity.".to_owned(),
        ]),
        Some(Delivery::Crop(region, resized)) => {
            let Region {
                x,
                y,
                width,
                height,
            } = region;
            let scale = match resized {
                Some((w, h)) => format!(", downsampled to {w}x{h} pixels"),
                None => " at native resolution".to_owned(),
            };
            parts.extend([
                format!("Showing region (x={x}, y={y}, width={width}, height={height}) of the original image{scale}."),
                format!("To output coordinates in original-image pixels, locate them within this crop and add the region offset (x={x}, y={y})."),
            ]);
        }
        Some(Delivery::Full) => {
            parts.push("Shown at native resolution; no downscaling applied.".to_owned());
        }
        Some(Delivery::Untouched) | None => {}
    }
    if dimensions.is_some() && !matches!(delivery, Some(Delivery::Crop(..))) {
        parts.push("If you need to output coordinates, output relative coordinates first and compute absolute coordinates using the original image size.".to_owned());
    }
    parts.push("If you generate or edit images or videos via commands or scripts, read the result back immediately before continuing.".to_owned());
    format!("<system>{}</system>", parts.join(" "))
}

/// Kimi Code's `formatByteSize`, rounding halves up like `Math.round` and `toFixed`.
fn byte_size(bytes: usize) -> String {
    match bytes {
        0..1024 => format!("{bytes} B"),
        1024..1_048_576 => format!("{} KB", (bytes + 512) / 1024),
        _ => {
            let tenths = (bytes * 10 + 524_288) / 1_048_576;
            format!("{}.{} MB", tenths / 10, tenths % 10)
        }
    }
}

/// A JPEG's frame size and EXIF orientation (Kimi Code's `sniffImageDimensions` scan and
/// `readExifOrientation`).
fn jpeg_frame(b: &[u8]) -> Option<(i64, i64, u16)> {
    let be16 = |at: usize| u16::from_be_bytes([b[at], b[at + 1]]);
    if !b.starts_with(&[0xff, 0xd8]) {
        return None;
    }
    let mut orientation = None;
    let mut offset = 2;
    while offset + 9 < b.len() {
        if b[offset] != 0xff {
            offset += 1;
            continue;
        }
        let marker = b[offset + 1];
        if (0xc0..=0xcf).contains(&marker) && ![0xc4, 0xc8, 0xcc].contains(&marker) {
            let (height, width) = (be16(offset + 5), be16(offset + 7));
            return Some((width.into(), height.into(), orientation.unwrap_or(1)));
        }
        if matches!(marker, 0xd0..=0xd9) {
            offset += 2;
            continue;
        }
        let length = usize::from(be16(offset + 2));
        if length < 2 {
            break;
        }
        if marker == 0xe1 && orientation.is_none() {
            orientation = exif_orientation(&b[..(offset + 2 + length).min(b.len())], offset + 4);
        }
        offset += 2 + length;
    }
    None
}

fn exif_orientation(b: &[u8], start: usize) -> Option<u16> {
    let tiff = start + 6;
    if b.get(start..tiff)? != b"Exif\0\0" || tiff + 8 > b.len() {
        return None;
    }
    let le = match &b[tiff..tiff + 2] {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let u16_at = |at: usize| {
        let bytes = [b[at], b[at + 1]];
        if le {
            u16::from_le_bytes(bytes)
        } else {
            u16::from_be_bytes(bytes)
        }
    };
    let u32_at = |at: usize| {
        let bytes = [b[at], b[at + 1], b[at + 2], b[at + 3]];
        if le {
            u32::from_le_bytes(bytes)
        } else {
            u32::from_be_bytes(bytes)
        }
    };
    if u16_at(tiff + 2) != 42 {
        return None;
    }
    let ifd = tiff + usize::try_from(u32_at(tiff + 4)).ok()?;
    if ifd + 2 > b.len() {
        return None;
    }
    for index in 0..usize::from(u16_at(ifd)) {
        let entry = ifd + 2 + index * 12;
        if entry + 12 > b.len() {
            return None;
        }
        if u16_at(entry) == 274 {
            return Some(u16_at(entry + 8)).filter(|value| (1..=8).contains(value));
        }
    }
    None
}

/// Kimi Code's `sniffImageDimensions`: PNG, GIF, BMP, WebP, and JPEG transposed by its EXIF orientation.
fn sniff_dimensions(b: &[u8]) -> Option<(i64, i64)> {
    let le16 = |at: usize| i64::from(u16::from_le_bytes([b[at], b[at + 1]]));
    let le32 = |at: usize| u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]);
    let be32 = |at: usize| i64::from(u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]));
    let le24 = |at: usize| i64::from(le32(at - 1) >> 8);
    if b.starts_with(b"\x89PNG\r\n\x1a\n") && b.len() >= 24 {
        return Some((be32(16), be32(20)));
    }
    if (b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a")) && b.len() >= 10 {
        return Some((le16(6), le16(8)));
    }
    if b.starts_with(b"BM") && b.len() >= 26 {
        let signed = |at: usize| i64::from(le32(at).cast_signed());
        return Some((signed(18), signed(22).abs()));
    }
    if b.starts_with(b"RIFF") && b.len() >= 30 {
        return match &b[12..16] {
            b"VP8 " => Some((le16(26) & 16383, le16(28) & 16383)),
            b"VP8L" => {
                let bits = i64::from(le32(21));
                Some(((bits & 16383) + 1, (bits >> 14 & 16383) + 1))
            }
            b"VP8X" => Some((1 + le24(24), 1 + le24(27))),
            _ => None,
        };
    }
    let (width, height, orientation) = jpeg_frame(b)?;
    Some(match orientation {
        5.. => (height, width),
        _ => (width, height),
    })
}

/// Kimi Code's `isRecodableImage`: PNG, JPEG and still WebP.
fn recodable(bytes: &[u8], mime: &str) -> bool {
    let animated = bytes.len() >= 21
        && bytes.starts_with(b"RIFF")
        && &bytes[8..16] == b"WEBPVP8X"
        && bytes[20] & 2 != 0;
    matches!(mime, "image/png" | "image/jpeg") || (mime == "image/webp" && !animated)
}

/// Decodes to RGBA, turning a JPEG upright by its EXIF orientation as Jimp does.
fn decode(bytes: &[u8]) -> Result<Image, String> {
    let format = nanocode_image::guess_format(bytes)
        .ok_or_else(|| nanocode_image::DecodeError::UnknownFormat.to_string())?;
    let limits = nanocode_image::Limits {
        alloc_bytes_max: nanocode_image::DEFAULT_ALLOC_BYTES_MAX,
        dimension_max: None,
    };
    let image = nanocode_image::Decoder::new(bytes, format, limits)
        .and_then(|mut decoder| {
            decoder.reserve()?;
            decoder.decode()
        })
        .map_err(|error| error.to_string())?
        .image;
    let (w, h) = (image.width() as usize, image.height() as usize);
    let rgba = image.to_rgba8();
    let orientation = match format {
        Format::Jpeg => jpeg_frame(bytes).map_or(1, |frame| frame.2),
        _ => 1,
    };
    if orientation < 2 {
        return Image::new(w as u32, h as u32, Pixels::Rgba8(rgba))
            .ok_or_else(|| "inconsistent image dimensions".to_owned());
    }
    let (width, height) = match orientation {
        5.. => (h, w),
        _ => (w, h),
    };
    let mut upright = vec![0; rgba.len()];
    for y in 0..height {
        for x in 0..width {
            let (sx, sy) = match orientation {
                2 => (w - x - 1, y),
                3 => (w - x - 1, h - y - 1),
                4 => (x, h - y - 1),
                5 => (y, x),
                6 => (y, h - x - 1),
                7 => (w - y - 1, h - x - 1),
                _ => (w - y - 1, x),
            };
            let (to, from) = ((y * width + x) * 4, (sy * w + sx) * 4);
            upright[to..to + 4].copy_from_slice(&rgba[from..from + 4]);
        }
    }
    Image::new(width as u32, height as u32, Pixels::Rgba8(upright))
        .ok_or_else(|| "inconsistent image dimensions".to_owned())
}

/// Kimi Code's `fitWithinEdge`: scales the image so its longest edge is `edge`, if it is longer.
fn fit_within(image: &mut Image, edge: u32) -> bool {
    let (width, height) = (image.width(), image.height());
    let longest = width.max(height);
    if longest <= edge {
        return false;
    }
    let factor = f64::from(edge) / f64::from(longest);
    let scaled = |side: u32| ((f64::from(side) * factor).round() as usize).max(1);
    let (to_width, to_height) = (scaled(width), scaled(height));
    let rgba = image.to_rgba8();
    let (width, height) = (width as usize, height as usize);
    let rows: Vec<f32> = match to_width == width {
        true => rgba.iter().map(|&byte| f32::from(byte)).collect(),
        false => shrink(&rgba, width, height, to_width, 4, 4 * width, 4 * to_width)
            .iter()
            .map(|&value| value as f32)
            .collect(),
    };
    let pixels = match to_height == height {
        // Jimp hands back the width pass's floats, which `Buffer.from` truncates.
        true => rows.iter().map(|&value| value as u8).collect(),
        false => shrink(&rows, height, to_width, to_height, 4 * to_width, 4, 4)
            .iter()
            .map(|&value| value.round() as u8)
            .collect(),
    };
    *image = Image::new(to_width as u32, to_height as u32, Pixels::Rgba8(pixels))
        .expect("resized dimensions");
    true
}

/// One pass of Jimp's `Resize` when shrinking (`_resizeWidthRGBChannels` / `_resizeHeightRGBChannels` with
/// alpha): averages `length` source pixels into `to` along one axis, colors weighted by alpha, summing in
/// Float32 as Jimp does. `step` is the distance between pixels along the axis (also Jimp's position unit),
/// `across` between lines in the source and `out_across` in the output.
fn shrink<T: Copy + Into<f64>>(
    source: &[T],
    length: usize,
    lines: usize,
    to: usize,
    step: usize,
    across: usize,
    out_across: usize,
) -> Vec<f64> {
    let ratio = length as f64 / to as f64;
    let mut output = vec![0f64; lines * to * 4];
    let mut sums = vec![0f32; lines * 4];
    let mut opaque = vec![0f64; lines];
    let (mut actual, mut current) = (0usize, 0f64);
    for target in 0..to {
        sums.fill(0.0);
        opaque.fill(0.0);
        let mut weight = ratio;
        loop {
            let next = 1.0 + actual as f64 - current;
            let multiplier = weight.min(next);
            for line in 0..lines {
                let at = actual + line * across;
                let alpha: f64 = source[at + 3].into();
                for channel in 0..4 {
                    let value: f64 = source[at + channel].into();
                    let value = if channel == 3 || alpha != 0.0 {
                        value
                    } else {
                        0.0
                    };
                    let sum = &mut sums[line * 4 + channel];
                    *sum = (f64::from(*sum) + value * multiplier) as f32;
                }
                opaque[line] += if alpha != 0.0 { multiplier } else { 0.0 };
            }
            if weight >= next {
                actual += step;
                current = actual as f64;
                weight -= next;
                if weight > 0.0 && actual < length * step {
                    continue;
                }
            } else {
                current += weight;
            }
            break;
        }
        for line in 0..lines {
            let color = match opaque[line] {
                0.0 => 0.0,
                weight => 1.0 / weight,
            };
            for channel in 0..4 {
                let scale = if channel == 3 { 1.0 / ratio } else { color };
                output[target * step + line * out_across + channel] =
                    f64::from(sums[line * 4 + channel]) * scale;
            }
        }
    }
    output
}

struct Encoded {
    data: Vec<u8>,
    mime: &'static str,
    width: u32,
    height: u32,
}

/// PNG as Jimp writes it, or JPEG at `quality` (alpha dropped, as jpeg-js does).
fn encode(image: &Image, quality: Option<u8>) -> Result<Encoded, String> {
    let (data, mime) = match quality {
        None => (png(image)?, "image/png"),
        Some(quality) => {
            let encoding = Encoding {
                exif: None,
                format: Format::Jpeg,
                icc_profile: None,
                image,
                quality,
            };
            (nanocode_image::encode(encoding)?, "image/jpeg")
        }
    };
    Ok(Encoded {
        data,
        mime,
        width: image.width(),
        height: image.height(),
    })
}

/// pngjs's encoder with Jimp's options: 8-bit RGBA, each row's filter chosen by the least sum of absolute
/// differences, zlib level 9 with the RLE strategy.
fn png(image: &Image) -> Result<Vec<u8>, String> {
    use nanocode_compression::checksum::crc32;
    let (width, height) = (image.width(), image.height());
    let rgba = image.to_rgba8();
    let stride = width as usize * 4;
    let mut raw = Vec::with_capacity((stride + 1) * height as usize);
    for (y, line) in rgba.chunks_exact(stride.max(1)).enumerate() {
        let byte = |row: Option<&[u8]>, x: Option<usize>| match (row, x) {
            (Some(row), Some(x)) => i16::from(row[x]),
            _ => 0,
        };
        let above = y.checked_sub(1).map(|up| &rgba[up * stride..][..stride]);
        let predict = |filter: u8, x: usize| {
            let (left, up) = (byte(Some(line), x.checked_sub(4)), byte(above, Some(x)));
            let corner = byte(above, x.checked_sub(4));
            match filter {
                0 => 0,
                1 => left,
                2 => up,
                3 => (left + up) >> 1,
                _ => {
                    let guess = left + up - corner;
                    let (to_left, to_up) = ((guess - left).abs(), (guess - up).abs());
                    match (guess - corner).abs() {
                        to_corner if to_left <= to_up && to_left <= to_corner => left,
                        to_corner if to_up <= to_corner => up,
                        _ => corner,
                    }
                }
            }
        };
        let residual = |filter: u8, x: usize| i16::from(line[x]) - predict(filter, x);
        let cost = |filter: u8| {
            (0..stride)
                .map(|x| u64::from(residual(filter, x).unsigned_abs()))
                .sum::<u64>()
        };
        let mut best = (0, cost(0));
        for filter in 1..5 {
            let total = cost(filter);
            if total < best.1 {
                best = (filter, total);
            }
        }
        raw.push(best.0);
        raw.extend((0..stride).map(|x| residual(best.0, x) as u8));
    }
    let mut header = [0u8; 13];
    header[..4].copy_from_slice(&width.to_be_bytes());
    header[4..8].copy_from_slice(&height.to_be_bytes());
    header[8..10].copy_from_slice(&[8, 6]);
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    for (kind, data) in [
        (b"IHDR", header.to_vec()),
        (b"IDAT", zlib_rle(&raw)?),
        (b"IEND", Vec::new()),
    ] {
        png.extend((data.len() as u32).to_be_bytes());
        png.extend(kind);
        png.extend(&data);
        png.extend(crc32(crc32(0, kind), &data).to_be_bytes());
    }
    Ok(png)
}

/// The smallest encoding seen so far, for when none fits.
struct Budget {
    limit: usize,
    smallest: Option<Encoded>,
}

impl Budget {
    fn fits(&mut self, encoded: Encoded) -> Option<Encoded> {
        if encoded.data.len() <= self.limit {
            return Some(encoded);
        }
        if self
            .smallest
            .as_ref()
            .is_none_or(|smallest| encoded.data.len() < smallest.data.len())
        {
            self.smallest = Some(encoded);
        }
        None
    }

    fn jpeg_ladder(&mut self, image: &Image) -> Result<Option<Encoded>, String> {
        for quality in JPEG_QUALITIES {
            if let Some(fit) = self.fits(encode(image, Some(quality))?) {
                return Ok(Some(fit));
            }
        }
        Ok(None)
    }
}

/// Kimi Code's `encodeWithinBudget`: PNG first for lossless sources (rescaling down to 1000px), then JPEG
/// qualities, then smaller edges; the smallest attempt when nothing fits.
fn encode_within(image: &mut Image, lossless: bool, limit: usize) -> Result<Encoded, String> {
    let mut budget = Budget {
        limit,
        smallest: None,
    };
    if lossless {
        if let Some(fit) = budget.fits(encode(image, None)?) {
            return Ok(fit);
        }
        for edge in FALLBACK_EDGES {
            if edge < PNG_RESCALE_FLOOR {
                break;
            }
            if fit_within(image, edge)
                && let Some(fit) = budget.fits(encode(image, None)?)
            {
                return Ok(fit);
            }
        }
    }
    if let Some(fit) = budget.jpeg_ladder(image)? {
        return Ok(fit);
    }
    for edge in FALLBACK_EDGES {
        if (!lossless || edge < PNG_RESCALE_FLOOR)
            && fit_within(image, edge)
            && let Some(fit) = budget.jpeg_ladder(image)?
        {
            return Ok(fit);
        }
    }
    budget
        .smallest
        .ok_or_else(|| "nothing was encoded".to_owned())
}

/// Kimi Code's `compressImageForModel`: the re-encoded image and the decoded size, or `None` to send the
/// original bytes.
fn compress(bytes: &[u8], mime: &str, dims: Option<(i64, i64)>) -> Option<(Encoded, (i64, i64))> {
    let longest = dims.map_or(0, |(width, height)| width.max(height));
    if !recodable(bytes, mime)
        || (bytes.len() <= READ_BYTE_BUDGET && longest <= i64::from(MAX_EDGE))
        || dims.is_some_and(|(width, height)| width * height > MAX_DECODE_PIXELS)
    {
        return None;
    }
    let mut image = decode(bytes).ok()?;
    let original = (i64::from(image.width()), i64::from(image.height()));
    fit_within(&mut image, MAX_EDGE);
    let encoded = encode_within(&mut image, mime != "image/jpeg", READ_BYTE_BUDGET).ok()?;
    let pixels = i64::from(encoded.width) * i64::from(encoded.height);
    (encoded.data.len() < bytes.len() || pixels < original.0 * original.1)
        .then_some((encoded, original))
}

/// Kimi Code's `cropImageForModel`: the encoded crop, the original size and the region applied (clipped to
/// the image).
fn crop(
    bytes: &[u8],
    mime: &str,
    region: Region,
    full: bool,
) -> Result<(Encoded, (i64, i64), Region), String> {
    if !matches!(mime, "image/png" | "image/jpeg" | "image/webp") {
        return Err(format!(
            "Cropping is only supported for PNG, JPEG, and WebP images; got {mime}."
        ));
    }
    if !recodable(bytes, mime) {
        return Err("Cropping is not supported for animated WebP images.".to_owned());
    }
    if let Some((width, height)) = sniff_dimensions(bytes)
        && width * height > MAX_DECODE_PIXELS
    {
        return Err(format!(
            "The image ({width}x{height} pixels) is too large to decode for cropping."
        ));
    }
    let failed = |error: String| format!("Failed to decode the image for cropping: {error}");
    let image = decode(bytes).map_err(failed)?;
    let (width, height) = (u64::from(image.width()), u64::from(image.height()));
    let Region { x, y, .. } = region;
    if x >= width || y >= height {
        return Err(format!(
            "Region (x={x}, y={y}, width={}, height={}) lies outside the {width}x{height} image.",
            region.width, region.height
        ));
    }
    let applied = Region {
        x,
        y,
        width: region.width.min(width - x),
        height: region.height.min(height - y),
    };
    let rgba = image.to_rgba8();
    let stride = width as usize * 4;
    let row = applied.width as usize * 4;
    let pixels = (y as usize..(y + applied.height) as usize)
        .flat_map(|line| &rgba[line * stride + x as usize * 4..][..row])
        .copied()
        .collect();
    let mut image = Image::new(
        applied.width as u32,
        applied.height as u32,
        Pixels::Rgba8(pixels),
    )
    .ok_or_else(|| failed("inconsistent crop".to_owned()))?;
    let original = (width as i64, height as i64);
    let lossless = mime != "image/jpeg";
    if !full {
        fit_within(&mut image, MAX_EDGE);
        let encoded = encode_within(&mut image, lossless, CROP_BYTE_BUDGET).map_err(failed)?;
        return Ok((encoded, original, applied));
    }
    let encoded = encode(&image, (!lossless).then_some(90)).map_err(failed)?;
    let size = encoded.data.len();
    if size > CROP_BYTE_BUDGET {
        return Err(format!(
            "The cropped region encodes to {size} bytes ({}), over the {CROP_BYTE_BUDGET}-byte ({}) per-image limit. Choose a smaller region, or allow downscaling.",
            byte_size(size),
            byte_size(CROP_BYTE_BUDGET)
        ));
    }
    Ok((encoded, original, applied))
}

/// zlib at level 9 with the RLE strategy, as pngjs deflates for Jimp.
fn zlib_rle(data: &[u8]) -> Result<Vec<u8>, String> {
    use nanocode_compression::deflate::{
        Deflate, DeflateConfig, DeflateFlush, DeflateStatus, Strategy,
    };
    let mut deflate = Deflate::new(DeflateConfig {
        level: 9,
        window_bits: 15,
        mem_level: 8,
        strategy: Strategy::Rle,
    })
    .map_err(|error| error.as_str().to_owned())?;
    let mut output = vec![0; data.len() + data.len() / 1000 + 64];
    match deflate.deflate(data, &mut output, DeflateFlush::Finish) {
        Ok(DeflateStatus::StreamEnd) => {
            output.truncate(deflate.total_out() as usize);
            Ok(output)
        }
        Ok(_) => Err("deflate output overflow".to_owned()),
        Err(error) => Err(error.as_str().to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::{byte_size, sniff_dimensions};

    #[test]
    fn formats_sizes_like_kimi() {
        assert_eq!(byte_size(1023), "1023 B");
        assert_eq!(byte_size(1536), "2 KB");
        assert_eq!(byte_size(1_048_576 + 262_144), "1.3 MB");
        assert_eq!(byte_size(3_932_160), "3.8 MB");
    }

    #[test]
    fn sniffs_jpeg_orientation() {
        let tiff = b"MM\0\x2a\0\0\0\x08\0\x01\x01\x12\0\x03\0\0\0\x01\0\x06\0\0\0\0\0\0";
        let mut jpeg = vec![0xff, 0xd8, 0xff, 0xe1, 0, (2 + 6 + tiff.len()) as u8];
        jpeg.extend(b"Exif\0\0");
        jpeg.extend(tiff);
        jpeg.extend([0xff, 0xc0, 0, 17, 8, 0, 20, 0, 40, 3, 0, 0, 0, 0, 0, 0]);
        assert_eq!(sniff_dimensions(&jpeg), Some((20, 40)));
    }
}

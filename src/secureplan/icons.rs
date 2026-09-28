//! Device icons carried by the overlay and export payloads (schema version 2).
//!
//! The web sends each distinct icon once (`icons[]`), exactly what its canvas
//! draws: a catalog SVG, a lucide glyph serialized as SVG, or an uploaded
//! PNG, JPEG or WebP. Devices name it by `iconId`, or carry `null` for the
//! desktop's standard symbol.
//!
//! Decoding reads nothing but the icon's own bytes:
//! - SVG goes through usvg with an image resolver that resolves nothing (no
//!   file, `data:` or network image is ever loaded), no fonts beyond the
//!   empty default database, no gzip (`.svgz`) and no XML entity declarations;
//! - PNG, JPEG and WebP go through the `image` decoder with dimension and
//!   allocation limits.
//!
//! An icon that fails to decode is `None`: its devices get the standard
//! symbol, and the session goes on.

use std::sync::Arc;

use base64::Engine;
use resvg::usvg;
use serde_json::Value;

/// At most this many icons per payload.
pub const MAX_ICONS: usize = 256;
/// At most this many decoded bytes per icon.
pub const MAX_ICON_BYTES: usize = 262_144;
/// A raster icon wider or taller than this is refused.
pub const MAX_RASTER_SIDE: u32 = 2048;
/// Raster icons are kept at most this size (their longer side, in pixels).
pub const RASTER_KEEP_SIDE: u32 = 256;

/// An icon's media type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaType {
    Svg,
    Png,
    Jpeg,
    Webp,
}

impl MediaType {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "image/svg+xml" => Some(MediaType::Svg),
            "image/png" => Some(MediaType::Png),
            "image/jpeg" => Some(MediaType::Jpeg),
            "image/webp" => Some(MediaType::Webp),
            _ => None,
        }
    }
}

/// One `icons[]` entry, its data decoded from base64.
#[derive(Debug, Clone, PartialEq)]
pub struct IconData {
    pub id: String,
    pub media_type: MediaType,
    pub bytes: Arc<Vec<u8>>,
}

/// Read a payload's `icons` and check the rules the schema states in prose:
/// ids are unique, the data is standard base64 of at least one byte, and
/// every `iconId` a device uses names one of them. Returns the icons in
/// payload order.
pub fn parse_list(icons: &Value, devices: &Value) -> Result<Vec<IconData>, String> {
    let mut out: Vec<IconData> = Vec::new();
    for icon in icons.as_array().into_iter().flatten() {
        let id = icon["id"].as_str().unwrap_or_default().to_string();
        if out.iter().any(|seen| seen.id == id) {
            return Err("icon ids are not unique".into());
        }
        let media_type = MediaType::parse(icon["mediaType"].as_str().unwrap_or_default()).ok_or("unknown icon media type")?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(icon["data"].as_str().unwrap_or_default())
            .map_err(|_| "icon data is not base64".to_string())?;
        if bytes.is_empty() || bytes.len() > MAX_ICON_BYTES {
            return Err("icon data is empty or too large".into());
        }
        out.push(IconData { id, media_type, bytes: Arc::new(bytes) });
    }
    if out.len() > MAX_ICONS {
        return Err("too many icons".into());
    }
    for device in devices.as_array().into_iter().flatten() {
        if let Some(id) = device["iconId"].as_str() {
            if !out.iter().any(|icon| icon.id == id) {
                return Err("a device names an icon the payload does not carry".into());
            }
        }
    }
    Ok(out)
}

/// usvg options that load nothing from outside the SVG: every `<image>`
/// reference, `data:` URLs included, resolves to nothing.
pub fn svg_options() -> usvg::Options<'static> {
    usvg::Options {
        resources_dir: None,
        image_href_resolver: usvg::ImageHrefResolver { resolve_data: Box::new(|_, _, _| None), resolve_string: Box::new(|_, _| None) },
        ..usvg::Options::default()
    }
}

/// Parse an SVG icon, or `None` when it is not a plain SVG document.
pub fn parse_svg(bytes: &[u8]) -> Option<usvg::Tree> {
    // Plain text only: no gzip (whose inflated size is unbounded) and no
    // entity declarations (entity expansion).
    let text = std::str::from_utf8(bytes).ok()?;
    if text.contains("<!ENTITY") {
        return None;
    }
    let tree = usvg::Tree::from_str(text, &svg_options()).ok()?;
    let size = tree.size();
    (size.width() > 0.0 && size.height() > 0.0).then_some(tree)
}

/// A raster icon as straight-alpha RGBA, at most [`RASTER_KEEP_SIDE`] on its
/// longer side.
#[derive(Debug, Clone)]
pub struct Raster {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Decode a PNG, JPEG or WebP icon within the dimension limits.
pub fn decode_raster(media_type: MediaType, bytes: &[u8]) -> Option<Raster> {
    let format = match media_type {
        MediaType::Png => image::ImageFormat::Png,
        MediaType::Jpeg => image::ImageFormat::Jpeg,
        MediaType::Webp => image::ImageFormat::WebP,
        MediaType::Svg => return None,
    };
    let mut reader = image::ImageReader::with_format(std::io::Cursor::new(bytes), format);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_RASTER_SIDE);
    limits.max_image_height = Some(MAX_RASTER_SIDE);
    limits.max_alloc = Some(64 * 1024 * 1024);
    reader.limits(limits);
    let image = reader.decode().ok()?;
    let image = if image.width() > RASTER_KEEP_SIDE || image.height() > RASTER_KEEP_SIDE {
        image.resize(RASTER_KEEP_SIDE, RASTER_KEEP_SIDE, image::imageops::FilterType::Triangle)
    } else {
        image
    };
    let rgba = image.to_rgba8();
    let (width, height) = rgba.dimensions();
    (width > 0 && height > 0).then(|| Raster { width, height, rgba: rgba.into_raw() })
}

/// A decoded icon.
#[derive(Debug, Clone)]
pub enum Decoded {
    Svg(Arc<usvg::Tree>),
    Raster(Arc<Raster>),
}

/// Decode an icon; `None` when it cannot be read.
pub fn decode(icon: &IconData) -> Option<Decoded> {
    match icon.media_type {
        MediaType::Svg => parse_svg(&icon.bytes).map(|tree| Decoded::Svg(Arc::new(tree))),
        other => decode_raster(other, &icon.bytes).map(|raster| Decoded::Raster(Arc::new(raster))),
    }
}

/// Render an SVG icon into a `side` × `side` square, stretched to fill it as
/// the web canvas draws an image into its box, as straight-alpha RGBA.
pub fn rasterize_svg(tree: &usvg::Tree, side: u32) -> Option<Vec<u8>> {
    let mut pixmap = resvg::tiny_skia::Pixmap::new(side, side)?;
    let size = tree.size();
    let transform = resvg::tiny_skia::Transform::from_scale(side as f32 / size.width(), side as f32 / size.height());
    resvg::render(tree, transform, &mut pixmap.as_mut());
    Some(
        pixmap
            .pixels()
            .iter()
            .flat_map(|pixel| {
                let c = pixel.demultiply();
                [c.red(), c.green(), c.blue(), c.alpha()]
            })
            .collect(),
    )
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) const CIRCLE_SVG: &str =
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="#fbfaf4" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="8"/><path d="M4 4h16v16z" fill="#fbfaf4" stroke="none"/></svg>"##;

    /// A 2 × 2 PNG, made here so no binary fixture is needed.
    pub(crate) fn png_bytes(width: u32, height: u32) -> Vec<u8> {
        let image = image::RgbaImage::from_pixel(width, height, image::Rgba([200, 30, 30, 255]));
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image).write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    pub(crate) fn b64(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    fn has_image(group: &usvg::Group) -> bool {
        group.children().iter().any(|node| match node {
            usvg::Node::Image(_) => true,
            usvg::Node::Group(group) => has_image(group),
            _ => false,
        })
    }

    #[test]
    fn svg_icons_never_load_external_or_embedded_images() {
        let dir = std::env::temp_dir().join(format!("secureplan-icons-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("outside.png");
        std::fs::write(&png, png_bytes(2, 2)).unwrap();
        let hrefs = [
            png.display().to_string(),
            format!("file://{}", png.display()),
            "https://example.invalid/icon.png".to_string(),
            format!("data:image/png;base64,{}", b64(&png_bytes(2, 2))),
        ];
        for href in hrefs {
            let svg = format!(
                r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="24" height="24"><image width="24" height="24" href="{href}"/><image width="24" height="24" xlink:href="{href}"/><rect width="4" height="4"/></svg>"#
            );
            let tree = parse_svg(svg.as_bytes()).expect("the SVG itself parses");
            assert!(!has_image(tree.root()), "{href} was resolved");
        }
        // The resolvers themselves refuse every reference, whatever it is.
        let options = svg_options();
        assert!((options.image_href_resolver.resolve_string)(&png.display().to_string(), &options).is_none());
        assert!((options.image_href_resolver.resolve_data)("image/png", Arc::new(png_bytes(2, 2)), &options).is_none());
        let _ = std::fs::remove_dir_all(&dir);
        // No entity declarations and no gzip.
        let entities = r#"<?xml version="1.0"?><!DOCTYPE svg [<!ENTITY a "aaaa">]><svg xmlns="http://www.w3.org/2000/svg" width="4" height="4"><text>&a;</text></svg>"#;
        assert!(parse_svg(entities.as_bytes()).is_none());
        assert!(parse_svg(&[0x1f, 0x8b, 8, 0, 0, 0, 0, 0]).is_none());
        assert!(parse_svg(CIRCLE_SVG.as_bytes()).is_some());
    }

    #[test]
    fn bad_icons_decode_to_nothing_and_rasters_are_bounded() {
        let icon = |media_type: MediaType, bytes: Vec<u8>| IconData { id: "a".into(), media_type, bytes: Arc::new(bytes) };
        assert!(decode(&icon(MediaType::Svg, b"<svg".to_vec())).is_none());
        assert!(decode(&icon(MediaType::Png, b"not a png".to_vec())).is_none());
        assert!(decode(&icon(MediaType::Jpeg, png_bytes(2, 2))).is_none(), "the declared type decides");
        assert!(decode(&icon(MediaType::Webp, vec![0; 16])).is_none());
        assert!(decode(&icon(MediaType::Png, png_bytes(MAX_RASTER_SIDE + 1, 1))).is_none(), "over the dimension limit");
        let Some(Decoded::Raster(raster)) = decode(&icon(MediaType::Png, png_bytes(600, 300))) else { panic!("a PNG") };
        assert_eq!((raster.width, raster.height), (256, 128), "kept at most 256 px");
        let Some(Decoded::Svg(tree)) = decode(&icon(MediaType::Svg, CIRCLE_SVG.as_bytes().to_vec())) else { panic!("an SVG") };
        let pixels = rasterize_svg(&tree, 32).unwrap();
        assert_eq!(pixels.len(), 32 * 32 * 4);
        assert!(pixels.chunks(4).any(|p| p[3] > 0), "something is drawn");
    }

    #[test]
    fn icon_lists_follow_the_semantic_rules() {
        let icon = |id: &str, data: &str| serde_json::json!({ "id": id, "mediaType": "image/svg+xml", "data": data });
        let svg = b64(CIRCLE_SVG.as_bytes());
        let devices = serde_json::json!([{ "iconId": "a1" }, { "iconId": null }]);
        let parsed = parse_list(&serde_json::json!([icon("a1", &svg), icon("b2", &svg)]), &devices).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].bytes.as_slice(), CIRCLE_SVG.as_bytes());
        assert!(parse_list(&serde_json::json!([icon("a1", &svg), icon("a1", &svg)]), &devices).is_err(), "duplicate ids");
        assert!(parse_list(&serde_json::json!([icon("b2", &svg)]), &devices).is_err(), "a device names a missing icon");
        assert!(parse_list(&serde_json::json!([icon("a1", "")]), &devices).is_err(), "empty data");
        assert!(parse_list(&serde_json::json!([icon("a1", "abc")]), &devices).is_err(), "not padded base64");
        assert!(parse_list(&serde_json::json!([]), &serde_json::json!([{ "iconId": null }])).unwrap().is_empty());
    }
}

//! Device icons carried by the overlay and export payloads (schema version 2).
//!
//! The web sends each distinct icon once (`icons[]`), exactly what its canvas
//! draws: a catalog SVG, a lucide glyph serialized as SVG, or an uploaded
//! PNG, JPEG or WebP. Devices name it by `iconId`, or carry `null` for the
//! desktop's standard symbol.
//!
//! Decoding reads nothing but the icon's own bytes:
//! - SVG is plain shapes only: before usvg sees it, the XML must be at most
//!   [`MAX_SVG_NODES`] nodes, [`MAX_SVG_DEPTH`] deep and use only the
//!   elements the web's catalog SVGs and lucide glyphs use (shapes, groups,
//!   `defs`, `style`, gradients, titles). `use`, `filter`, `mask`,
//!   `clipPath`, `pattern`, `marker`, `symbol`, `image`, `text`, nested
//!   `svg` and every other element are refused, so nothing is expanded or
//!   allocated beyond the icon's own shapes. The parsed tree is checked
//!   again (no filters, including CSS filter functions, masks, clip paths,
//!   patterns, images or text) and must stay within path, dash and layer
//!   budgets before it is ever rendered. usvg loads no image (the resolver
//!   resolves nothing), no fonts beyond the empty default database, no gzip
//!   (`.svgz`) and no XML entity declarations;
//! - PNG, JPEG and WebP go through the `image` decoder with dimension and
//!   allocation limits: up to 4096 pixels a side, as the web accepts.
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
/// A raster icon wider or taller than this is refused (the web accepts
/// custom icons up to 4096 × 4096).
pub const MAX_RASTER_SIDE: u32 = 4096;
/// At most this many bytes are allocated to decode a raster icon: a
/// 4096 × 4096 RGBA image fits.
pub const MAX_RASTER_ALLOC: u64 = 64 * 1024 * 1024;
/// SVG budgets, checked before rendering (see the module notes). The web's
/// catalog SVGs have at most 63 elements, 13 deep.
pub const MAX_SVG_NODES: u32 = 20_000;
pub const MAX_SVG_DEPTH: usize = 32;
/// Path segments in the parsed tree.
pub const MAX_SVG_SEGMENTS: usize = 100_000;
/// Dashes a dashed stroke may produce, summed over the icon.
pub const MAX_SVG_DASHES: f64 = 20_000.0;
/// Groups that need their own layer (opacity), nested: each layer is at most
/// 5 × 5 times the rendered size.
pub const MAX_SVG_LAYERS: usize = 4;
/// The SVG elements an icon may use: what the catalog SVGs and the lucide
/// glyphs use, plus gradients. Elements outside the SVG namespace are
/// ignored by usvg and allowed.
const SVG_ELEMENTS: &[&str] = &[
    "svg", "g", "defs", "title", "desc", "metadata", "style", "path", "rect", "circle", "ellipse", "line", "polyline", "polygon",
    "linearGradient", "radialGradient", "stop",
];
const SVG_NS: &str = "http://www.w3.org/2000/svg";
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

/// Parse an SVG icon, or `None` when it is not a plain SVG document within
/// the budgets.
pub fn parse_svg(bytes: &[u8]) -> Option<usvg::Tree> {
    // Plain text only: no gzip (whose inflated size is unbounded) and no
    // entity declarations (entity expansion).
    let text = std::str::from_utf8(bytes).ok()?;
    if text.contains("<!ENTITY") {
        return None;
    }
    let options = usvg::roxmltree::ParsingOptions { allow_dtd: true, nodes_limit: MAX_SVG_NODES };
    let xml = usvg::roxmltree::Document::parse_with_options(text, options).ok()?;
    if !plain_elements(&xml) {
        return None;
    }
    let tree = usvg::Tree::from_xmltree(&xml, &svg_options()).ok()?;
    let size = tree.size();
    (size.width() > 0.0 && size.height() > 0.0 && within_budget(&tree)).then_some(tree)
}

/// Whether the XML uses only [`SVG_ELEMENTS`], one root `svg` and at most
/// [`MAX_SVG_DEPTH`] levels.
fn plain_elements(xml: &usvg::roxmltree::Document) -> bool {
    // Document order visits a parent before its children.
    let mut depth = vec![0usize; xml.descendants().map(|n| n.id().get_usize() + 1).max().unwrap_or(0)];
    for node in xml.descendants().filter(|n| n.is_element()) {
        let level = node.parent().map_or(0, |p| depth[p.id().get_usize()]) + 1;
        depth[node.id().get_usize()] = level;
        if level > MAX_SVG_DEPTH {
            return false;
        }
        if node.tag_name().namespace() == Some(SVG_NS) {
            let name = node.tag_name().name();
            if !SVG_ELEMENTS.contains(&name) || (name == "svg" && node.parent_element().is_some()) {
                return false;
            }
        }
    }
    true
}

/// Whether the parsed tree draws only plain shapes within the path, dash
/// and layer budgets. CSS filter functions (`filter="blur(…)"`) become
/// filters here, so they are refused here.
fn within_budget(tree: &usvg::Tree) -> bool {
    // Text is refused in the walk below (usvg 0.45's `has_text_nodes` says
    // true for every tree).
    if !tree.filters().is_empty() || !tree.masks().is_empty() || !tree.clip_paths().is_empty() || !tree.patterns().is_empty() {
        return false;
    }
    fn walk(group: &usvg::Group, layers: usize, segments: &mut usize, dashes: &mut f64) -> bool {
        group.children().iter().all(|node| match node {
            usvg::Node::Group(group) => {
                let layers = layers + usize::from(group.should_isolate());
                layers <= MAX_SVG_LAYERS && walk(group, layers, segments, dashes)
            }
            usvg::Node::Path(path) => {
                let data = path.data();
                *segments += data.verbs().len();
                if let Some(period) = path.stroke().and_then(|s| s.dasharray()).map(|d| d.iter().map(|v| f64::from(*v)).sum::<f64>()) {
                    // The control polygon is at least as long as the path.
                    let length: f64 = data.points().windows(2).map(|w| f64::from((w[1].x - w[0].x).hypot(w[1].y - w[0].y))).sum();
                    if period > 0.0 {
                        *dashes += length / period;
                    }
                }
                *segments <= MAX_SVG_SEGMENTS && *dashes <= MAX_SVG_DASHES
            }
            usvg::Node::Image(_) | usvg::Node::Text(_) => false,
        })
    }
    walk(tree.root(), 0, &mut 0, &mut 0.0)
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
    limits.max_alloc = Some(MAX_RASTER_ALLOC);
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
            // An `<image>` is refused outright, whatever it names.
            assert!(parse_svg(svg.as_bytes()).is_none_or(|tree| !has_image(tree.root())), "{href} was resolved");
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
        assert!(decode(&icon(MediaType::Png, png_bytes(1, MAX_RASTER_SIDE + 1))).is_none(), "over the dimension limit");
        // The web accepts custom icons up to 4096 × 4096; the largest decodes
        // within the allocation limit.
        assert_eq!(MAX_RASTER_SIDE, 4096);
        assert!(u64::from(MAX_RASTER_SIDE).pow(2) * 4 <= MAX_RASTER_ALLOC);
        for (width, height) in [(4096, 40), (40, 4096), (3000, 2100)] {
            let Some(Decoded::Raster(raster)) = decode(&icon(MediaType::Png, png_bytes(width, height))) else { panic!("{width} × {height} is refused") };
            assert!(raster.width.max(raster.height) == RASTER_KEEP_SIDE && raster.width.min(raster.height) >= 1, "{width} × {height} kept at 256 px");
        }
        let Some(Decoded::Raster(raster)) = decode(&icon(MediaType::Png, png_bytes(600, 300))) else { panic!("a PNG") };
        assert_eq!((raster.width, raster.height), (256, 128), "kept at most 256 px");
        let Some(Decoded::Svg(tree)) = decode(&icon(MediaType::Svg, CIRCLE_SVG.as_bytes().to_vec())) else { panic!("an SVG") };
        let pixels = rasterize_svg(&tree, 32).unwrap();
        assert_eq!(pixels.len(), 32 * 32 * 4);
        assert!(pixels.chunks(4).any(|p| p[3] > 0), "something is drawn");
    }

    /// An SVG document around `body`, 24 × 24.
    fn svg(body: &str) -> String {
        format!(r##"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="24" height="24" viewBox="0 0 24 24">{body}</svg>"##)
    }

    #[test]
    fn everything_the_web_icons_use_is_drawn() {
        // The features of the catalog SVGs (a stylesheet, defs, titles,
        // transforms, fill rules, dashes, every basic shape) and a gradient.
        let icon = svg(concat!(
            r##"<title>Icon</title><desc>A test icon</desc><style type="text/css">.st0{fill:#FFFFFF;}</style>"##,
            r##"<defs><linearGradient id="g" x1="0" x2="1"><stop offset="0" stop-color="#fff"/><stop offset="1" stop-color="#000"/></linearGradient></defs>"##,
            r##"<g transform="translate(1 1)" style="stroke:#fbfaf4;stroke-width:1"><path class="st0" fill-rule="evenodd" d="M2 2h8v8h-8zM4 4h4v4h-4z"/>"##,
            r##"<rect x="12" y="2" width="8" height="8" rx="2" fill="url(#g)"/><circle cx="6" cy="16" r="3"/><ellipse cx="16" cy="16" rx="4" ry="2"/>"##,
            r##"<line x1="0" y1="22" x2="22" y2="22" stroke-dasharray="2"/><polyline points="0,0 4,4 8,0" fill="none"/><polygon points="10,20 12,18 14,20"/></g>"##,
            r##"<g opacity="0.5"><g opacity="0.5"><rect width="2" height="2"/></g></g>"##,
        ));
        let tree = parse_svg(icon.as_bytes()).expect("drawn");
        assert!(rasterize_svg(&tree, 64).unwrap().chunks(4).any(|p| p[3] > 0));
        assert!(parse_svg(CIRCLE_SVG.as_bytes()).is_some(), "a lucide glyph as the web serializes it");
    }

    /// Adversarial SVGs are refused before anything is rendered, so none of
    /// them can make a large allocation: filters (the review's 100 km
    /// filter region would need about 18 GB), `use` graphs that expand
    /// exponentially, and the other features icons do not need.
    #[test]
    fn unsafe_svg_features_and_budgets_are_refused_before_rendering() {
        let refused = |what: &str, body: &str| {
            let started = std::time::Instant::now();
            assert!(parse_svg(svg(body).as_bytes()).is_none(), "{what} was accepted");
            assert!(started.elapsed() < std::time::Duration::from_secs(2), "{what} took {:?}", started.elapsed());
        };
        refused(
            "an oversized filter region",
            r##"<filter id="f" filterUnits="userSpaceOnUse" x="-50000" y="-50000" width="100000" height="100000"><feFlood flood-color="red"/></filter><rect width="24" height="24" filter="url(#f)"/>"##,
        );
        refused("a CSS filter function", r##"<g filter="drop-shadow(0 0 5000 red) blur(5000)"><rect width="24" height="24"/></g>"##);
        // Ten uses of the level below, eight levels deep: 10^8 rectangles.
        let mut graph = r#"<defs><g id="l0"><rect width="1" height="1"/></g>"#.to_string();
        for level in 1..=8 {
            graph.push_str(&format!(r##"<g id="l{level}">"##));
            for _ in 0..10 {
                graph.push_str(&format!(r##"<use href="#l{}"/>"##, level - 1));
            }
            graph.push_str("</g>");
        }
        graph.push_str(r##"</defs><use xlink:href="#l8"/>"##);
        refused("an expanding use graph", &graph);
        refused("a mask", r##"<mask id="m"><rect width="24" height="24" fill="#fff"/></mask><rect width="24" height="24" mask="url(#m)"/>"##);
        refused("a clip path", r##"<clipPath id="c"><rect width="4" height="4"/></clipPath><rect width="24" height="24" clip-path="url(#c)"/>"##);
        refused("a pattern", r##"<pattern id="p" width="0.001" height="0.001" patternUnits="userSpaceOnUse"><rect width="1" height="1"/></pattern><rect width="24" height="24" fill="url(#p)"/>"##);
        refused("a marker", r##"<marker id="k"><rect width="4" height="4"/></marker><path d="M0 0L1 1L2 2" marker-mid="url(#k)"/>"##);
        refused("text", r##"<text x="2" y="12">SW</text>"##);
        refused("a nested svg", r##"<svg width="4" height="4"><rect width="4" height="4"/></svg>"##);
        refused("a foreign object", r##"<foreignObject width="4" height="4"/>"##);
        refused("a symbol", r##"<symbol id="s"><rect width="4" height="4"/></symbol>"##);
        // Budgets.
        refused("nesting too deep", &format!("{}<rect width=\"1\" height=\"1\"/>{}", "<g>".repeat(MAX_SVG_DEPTH), "</g>".repeat(MAX_SVG_DEPTH)));
        assert!(parse_svg(svg(&format!("{}<rect width=\"1\" height=\"1\"/>{}", "<g>".repeat(20), "</g>".repeat(20))).as_bytes()).is_some());
        refused("too many nodes", &"<rect/>".repeat(MAX_SVG_NODES as usize));
        refused("too many segments", &format!(r##"<path d="M0 0{}"/>"##, "h1".repeat(MAX_SVG_SEGMENTS)));
        refused("a dash bomb", r##"<path d="M0 0H1000000" stroke="#000" stroke-dasharray="0.01"/>"##);
        let layers = |n: usize| format!("{}<rect width=\"4\" height=\"4\"/>{}", r##"<g opacity="0.5"><rect width="1" height="1"/>"##.repeat(n), "</g>".repeat(n));
        refused("too many nested layers", &layers(MAX_SVG_LAYERS + 1));
        assert!(parse_svg(svg(&layers(MAX_SVG_LAYERS)).as_bytes()).is_some());
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

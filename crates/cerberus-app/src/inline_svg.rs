//! Inline `<svg>` → synthetic replaced element.
//!
//! The engine already rasterizes SVG *files* through `resvg` (`cerberus-image`,
//! ADR-0009), but inline `<svg>` subtrees never reached that decoder — the UA
//! stylesheet simply hid them, collapsing the space Chrome reserves and
//! shifting siblings. This module closes the wiring gap at the document-
//! preparation seam: **after** page scripts have run and **before** styling,
//! each inline `<svg>` subtree is serialized back to standalone SVG markup and
//! the DOM node is rewritten *in place* (ids stable) into
//! `<img src="cerb-inline-svg:<hash>" width=… height=…>`, so layout's existing
//! replaced-element path — attribute sizing, CSS `width`/`height` overrides,
//! intrinsic-ratio derivation — just works. The serialized bytes are handed to
//! the caller to decode through the ordinary `ImageCodec` (keeping its SVG
//! byte/size ceilings) and register in the per-page image store under the
//! synthetic key.
//!
//! Sizing matches headless Chromium 139 (measured 2026-07-13,
//! `--headless=new --dump-dom` over `getBoundingClientRect` on a `file://`
//! page, container 800px wide):
//!
//! | svg attributes                    | Chrome box  | rule                     |
//! |-----------------------------------|-------------|--------------------------|
//! | `width=100 height=40`             | 100×40      | both attrs win           |
//! | `width=200 viewBox="0 0 100 50"`  | 200×100     | height from ratio        |
//! | `height=100 viewBox="0 0 100 50"` | 200×100     | width from ratio         |
//! | `width=200` (no viewBox)          | 200×150     | missing axis defaults    |
//! | `height=80` (no viewBox)          | 300×80      | missing axis defaults    |
//! | `viewBox="0 0 400 100"` only      | 800×200     | stretch to container,    |
//! | `viewBox="0 0 100 400"` only      | 800×3200    |   height from ratio      |
//! | no attrs at all                   | 300×150     | default object size      |
//! | `width=120px height=60px`         | 120×60      | `px` suffix accepted     |
//!
//! The container-stretch rows are approximated with the viewport content
//! width: layout clamps a decoded image to its actual containing block
//! (ratio-preserving), which lands on Chrome's number whenever the container
//! is the body. Percentage attributes (`width="50%"`) resolve against a
//! containing block we don't know pre-style, so they are treated as absent
//! (falling into the stretch/default rows) — a knowing simplification.
//!
//! `fill="currentColor"` is left for `resvg` to resolve; without the CSS
//! cascade (this runs pre-style) that is SVG's initial `color`, black — noted
//! in the module contract rather than plumbed from computed style.

use cerberus_dom::{Document, NodeId, NodeRef};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// Scheme prefix of the synthetic `src` a rewritten inline `<svg>` carries.
/// Never fetched: the app registers the decoded bitmap in the per-page image
/// store under this exact key, and the URL joiner round-trips it verbatim
/// (opaque scheme form), so layout's provider lookup hits. Not http(s), so the
/// subresource collectors skip it (no network, no consent gate — inline SVG is
/// first-party document content).
pub(crate) const INLINE_SVG_PREFIX: &str = "cerb-inline-svg:";

/// Longest raster side for an inline SVG bitmap. The *box* keeps the full
/// Chrome-matching size via the `<img>` attributes; the bitmap may be smaller
/// and scale up at paint. This bounds decoded memory for the container-stretch
/// case (a viewBox-only icon at a 1280px viewport would otherwise decode to a
/// ~6.5 MB RGBA buffer — memory is priority #1).
const MAX_RASTER_DIM: f32 = 1024.0;

/// Chrome's default object size for a replaced element with no intrinsic
/// dimensions (CSS2 §10.3.2): 300×150.
const DEFAULT_W: f32 = 300.0;
const DEFAULT_H: f32 = 150.0;

/// Rewrite every inline `<svg>` subtree in `doc` into a synthetic
/// `<img src="cerb-inline-svg:<hash>">` sized like Chrome (see module docs),
/// returning the serialized SVG payloads to register, one per *distinct*
/// document (`(key, bytes)`, content-hash keyed, so a page repeating one icon
/// registers — and decodes — it once). `viewport_w` is the layout content
/// width, used for the container-stretch sizing row.
pub(crate) fn replace_inline_svgs(doc: &mut Document, viewport_w: u32) -> Vec<(String, Vec<u8>)> {
    let mut roots = Vec::new();
    collect_svg_roots(doc.root(), &mut roots);
    let mut out: Vec<(String, Vec<u8>)> = Vec::new();
    for id in roots {
        let Some(node) = doc.node(id) else { continue };
        let (w, h) = svg_box_size(node, viewport_w);
        let (rw, rh) = raster_size(w, h);
        let markup = serialize_svg(node, rw, rh);

        let mut hasher = DefaultHasher::new();
        markup.hash(&mut hasher);
        let key = format!("{INLINE_SVG_PREFIX}{:016x}", hasher.finish());

        let mut attrs = vec![
            ("src".to_string(), key.clone()),
            ("width".to_string(), w.to_string()),
            ("height".to_string(), h.to_string()),
        ];
        // Carry the styling hooks over so author CSS that sizes the svg by
        // class/id (`.icon { width: 24px }`) still overrides the attributes.
        for name in ["class", "id", "style"] {
            if let Some(v) = node.attr(name) {
                attrs.push((name.to_string(), v.to_string()));
            }
        }
        if !out.iter().any(|(k, _)| *k == key) {
            out.push((key, markup.into_bytes()));
        }
        // The element KEEPS its `svg` tag (now childless, with a synthetic
        // `src`): tag-selector rules (`svg{…}`, `.logo svg{width:84px}`,
        // media-query variant hiding) are how real sites size and toggle
        // inline icons — renaming to `img` broke all of them (bbc's header
        // logo stretched to the container and both responsive variants
        // painted). Layout routes a src-carrying `svg` through the replaced
        // `<img>` path and skips raw `<svg>` subtrees entirely.
        doc.replace_element(id, "svg", attrs);
    }
    out
}

/// Collect the ids of top-level `<svg>` elements (an `<svg>` nested inside
/// another is part of the outer one's serialized subtree, not a root).
fn collect_svg_roots(node: NodeRef<'_>, out: &mut Vec<NodeId>) {
    if node.tag() == "svg" {
        out.push(node.id());
        return;
    }
    for child in node.children() {
        if child.is_element() {
            collect_svg_roots(child, out);
        }
    }
}

/// The CSS box an inline `<svg>` gets in Chrome, from its `width`/`height`
/// presentation attributes and `viewBox` ratio (measurements in module docs).
fn svg_box_size(node: NodeRef<'_>, viewport_w: u32) -> (u32, u32) {
    let w = node.attr("width").and_then(px_len);
    let h = node.attr("height").and_then(px_len);
    // ratio = width / height, from a positive viewBox.
    let ratio = view_box_ratio(node);
    let (w, h) = match (w, h) {
        (Some(w), Some(h)) => (w, h),
        (Some(w), None) => (w, ratio.map_or(DEFAULT_H, |r| w / r)),
        (None, Some(h)) => (ratio.map_or(DEFAULT_W, |r| h * r), h),
        (None, None) => match ratio {
            // Stretch to the container (approximated by the viewport content
            // width; layout re-clamps to the actual containing block).
            Some(r) => {
                let w = (viewport_w.max(1)) as f32;
                (w, w / r)
            }
            None => (DEFAULT_W, DEFAULT_H),
        },
    };
    (w.round().max(0.0) as u32, h.round().max(0.0) as u32)
}

/// Parse an SVG length attribute that resolves without a containing block: a
/// bare number or a `px` length. Percentages and other units need layout
/// context we don't have pre-style, so they read as absent.
fn px_len(v: &str) -> Option<f32> {
    let t = v.trim().trim_end_matches("px").trim();
    let n: f32 = t.parse().ok()?;
    (n.is_finite() && n >= 0.0).then_some(n)
}

/// The `viewBox` aspect ratio (w/h), if the attribute has four numbers and a
/// positive size. The HTML parser lowercased the attribute name to `viewbox`.
fn view_box_ratio(node: NodeRef<'_>) -> Option<f32> {
    let vb = node.attr("viewbox")?;
    let mut parts = vb
        .split(|c: char| c.is_ascii_whitespace() || c == ',')
        .filter(|s| !s.is_empty())
        .map(|s| s.parse::<f32>().ok());
    let _min_x = parts.next()??;
    let _min_y = parts.next()??;
    let w = parts.next()??;
    let h = parts.next()??;
    (w > 0.0 && h > 0.0 && w.is_finite() && h.is_finite()).then(|| w / h)
}

/// The size to rasterize at: the box size with the longest side capped at
/// [`MAX_RASTER_DIM`] (ratio preserved) and floored at 1 so the decoder always
/// has a positive canvas. The bitmap's ratio equals the box's, so layout's
/// intrinsic-ratio derivations (CSS `width` only, etc.) stay correct.
fn raster_size(w: u32, h: u32) -> (u32, u32) {
    let (w, h) = (w.max(1) as f32, h.max(1) as f32);
    let longest = w.max(h);
    let scale = if longest > MAX_RASTER_DIM {
        MAX_RASTER_DIM / longest
    } else {
        1.0
    };
    (
        ((w * scale).round() as u32).max(1),
        ((h * scale).round() as u32).max(1),
    )
}

/// Serialize an `<svg>` subtree back to standalone SVG markup for `resvg`.
///
/// The HTML tokenizer lowercased every tag and attribute name; XML-based SVG is
/// case-sensitive, so the HTML5 "adjust SVG tag/attribute names" tables are
/// applied in reverse ([`svg_tag_case`], [`svg_attr_case`]) — without them
/// `viewbox`/`lineargradient` would be meaningless to `usvg`. The root gets
/// `xmlns` (required by `resvg`), `xmlns:xlink` when the subtree uses `xlink:`
/// attributes without declaring it, and its `width`/`height` replaced by the
/// raster size, so the bitmap comes back at exactly the requested resolution
/// (`viewBox` content scales to fill; a viewBox-less document draws on the
/// enlarged canvas unscaled, matching browsers). Text and attribute values are
/// re-escaped (entities were decoded at parse time). Attributes with a
/// namespace prefix other than `xlink:`/`xml:`/`xmlns` (e.g. Inkscape residue)
/// are dropped — undeclared prefixes are fatal to a strict XML parser.
fn serialize_svg(root: NodeRef<'_>, raster_w: u32, raster_h: u32) -> String {
    let mut out = String::new();
    out.push_str("<svg");
    let mut has_xmlns = false;
    let mut has_xlink_ns = false;
    for (name, value) in root.attrs() {
        match name.as_str() {
            // Replaced by the raster size below.
            "width" | "height" => continue,
            "xmlns" => has_xmlns = true,
            "xmlns:xlink" => has_xlink_ns = true,
            _ => {}
        }
        write_attr(&mut out, name, value);
    }
    if !has_xmlns {
        write_attr(&mut out, "xmlns", "http://www.w3.org/2000/svg");
    }
    if !has_xlink_ns && subtree_uses_xlink(root) {
        write_attr(&mut out, "xmlns:xlink", "http://www.w3.org/1999/xlink");
    }
    write_attr(&mut out, "width", &raster_w.to_string());
    write_attr(&mut out, "height", &raster_h.to_string());
    if root.children().next().is_none() {
        out.push_str("/>");
        return out;
    }
    out.push('>');
    for child in root.children() {
        serialize_node(child, &mut out);
    }
    out.push_str("</svg>");
    out
}

/// Serialize one descendant node (element or text) into `out`.
fn serialize_node(node: NodeRef<'_>, out: &mut String) {
    if let Some(text) = node.text() {
        push_escaped(out, text, false);
        return;
    }
    let tag = node.tag();
    if let Some((prefix, _)) = tag.split_once(':') {
        // A prefixed element (foreign residue) would need its namespace
        // declared; drop the subtree rather than emit invalid XML.
        let _ = prefix;
        return;
    }
    let tag = svg_tag_case(tag);
    out.push('<');
    out.push_str(tag);
    for (name, value) in node.attrs() {
        write_attr(out, name, value);
    }
    if node.children().next().is_none() {
        out.push_str("/>");
        return;
    }
    out.push('>');
    for child in node.children() {
        serialize_node(child, out);
    }
    out.push_str("</");
    out.push_str(tag);
    out.push('>');
}

/// Append ` name="value"` (attribute name case-fixed, value escaped), or
/// nothing for a name XML could not parse.
fn write_attr(out: &mut String, name: &str, value: &str) {
    match name.split_once(':') {
        Some(("xlink" | "xml" | "xmlns", _)) | None => {}
        Some(_) => return, // undeclared namespace prefix — drop
    }
    out.push(' ');
    out.push_str(svg_attr_case(name));
    out.push_str("=\"");
    push_escaped(out, value, true);
    out.push('"');
}

/// Escape XML-significant characters; quotes only matter inside attributes.
fn push_escaped(out: &mut String, s: &str, in_attr: bool) {
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' if in_attr => out.push_str("&quot;"),
            _ => out.push(ch),
        }
    }
}

/// Whether any element in the subtree carries an `xlink:`-prefixed attribute
/// (commonly `<use xlink:href>`), which needs the namespace declared.
fn subtree_uses_xlink(node: NodeRef<'_>) -> bool {
    node.attrs().iter().any(|(k, _)| k.starts_with("xlink:"))
        || node
            .children()
            .any(|c| c.is_element() && subtree_uses_xlink(c))
}

/// Restore an SVG element name's canonical case from its HTML-lowercased form
/// (the HTML5 tree-construction "SVG tag name adjustment" table, reversed).
fn svg_tag_case(lower: &str) -> &str {
    match lower {
        "altglyph" => "altGlyph",
        "altglyphdef" => "altGlyphDef",
        "altglyphitem" => "altGlyphItem",
        "animatecolor" => "animateColor",
        "animatemotion" => "animateMotion",
        "animatetransform" => "animateTransform",
        "clippath" => "clipPath",
        "feblend" => "feBlend",
        "fecolormatrix" => "feColorMatrix",
        "fecomponenttransfer" => "feComponentTransfer",
        "fecomposite" => "feComposite",
        "feconvolvematrix" => "feConvolveMatrix",
        "fediffuselighting" => "feDiffuseLighting",
        "fedisplacementmap" => "feDisplacementMap",
        "fedistantlight" => "feDistantLight",
        "fedropshadow" => "feDropShadow",
        "feflood" => "feFlood",
        "fefunca" => "feFuncA",
        "fefuncb" => "feFuncB",
        "fefuncg" => "feFuncG",
        "fefuncr" => "feFuncR",
        "fegaussianblur" => "feGaussianBlur",
        "feimage" => "feImage",
        "femerge" => "feMerge",
        "femergenode" => "feMergeNode",
        "femorphology" => "feMorphology",
        "feoffset" => "feOffset",
        "fepointlight" => "fePointLight",
        "fespecularlighting" => "feSpecularLighting",
        "fespotlight" => "feSpotLight",
        "fetile" => "feTile",
        "feturbulence" => "feTurbulence",
        "foreignobject" => "foreignObject",
        "glyphref" => "glyphRef",
        "lineargradient" => "linearGradient",
        "radialgradient" => "radialGradient",
        "textpath" => "textPath",
        other => other,
    }
}

/// Restore an SVG attribute name's canonical case from its HTML-lowercased
/// form (the HTML5 "SVG attribute adjustment" table, reversed).
fn svg_attr_case(lower: &str) -> &str {
    match lower {
        "attributename" => "attributeName",
        "attributetype" => "attributeType",
        "basefrequency" => "baseFrequency",
        "baseprofile" => "baseProfile",
        "calcmode" => "calcMode",
        "clippathunits" => "clipPathUnits",
        "diffuseconstant" => "diffuseConstant",
        "edgemode" => "edgeMode",
        "filterunits" => "filterUnits",
        "glyphref" => "glyphRef",
        "gradienttransform" => "gradientTransform",
        "gradientunits" => "gradientUnits",
        "kernelmatrix" => "kernelMatrix",
        "kernelunitlength" => "kernelUnitLength",
        "keypoints" => "keyPoints",
        "keysplines" => "keySplines",
        "keytimes" => "keyTimes",
        "lengthadjust" => "lengthAdjust",
        "limitingconeangle" => "limitingConeAngle",
        "markerheight" => "markerHeight",
        "markerunits" => "markerUnits",
        "markerwidth" => "markerWidth",
        "maskcontentunits" => "maskContentUnits",
        "maskunits" => "maskUnits",
        "numoctaves" => "numOctaves",
        "pathlength" => "pathLength",
        "patterncontentunits" => "patternContentUnits",
        "patterntransform" => "patternTransform",
        "patternunits" => "patternUnits",
        "pointsatx" => "pointsAtX",
        "pointsaty" => "pointsAtY",
        "pointsatz" => "pointsAtZ",
        "preservealpha" => "preserveAlpha",
        "preserveaspectratio" => "preserveAspectRatio",
        "primitiveunits" => "primitiveUnits",
        "refx" => "refX",
        "refy" => "refY",
        "repeatcount" => "repeatCount",
        "repeatdur" => "repeatDur",
        "requiredextensions" => "requiredExtensions",
        "requiredfeatures" => "requiredFeatures",
        "specularconstant" => "specularConstant",
        "specularexponent" => "specularExponent",
        "spreadmethod" => "spreadMethod",
        "startoffset" => "startOffset",
        "stddeviation" => "stdDeviation",
        "stitchtiles" => "stitchTiles",
        "surfacescale" => "surfaceScale",
        "systemlanguage" => "systemLanguage",
        "tablevalues" => "tableValues",
        "targetx" => "targetX",
        "targety" => "targetY",
        "textlength" => "textLength",
        "viewbox" => "viewBox",
        "viewtarget" => "viewTarget",
        "xchannelselector" => "xChannelSelector",
        "ychannelselector" => "yChannelSelector",
        "zoomandpan" => "zoomAndPan",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cerberus_dom::parse_html;
    use cerberus_paint::ImageDecoder;

    fn find<'a>(node: NodeRef<'a>, tag: &str) -> Option<NodeRef<'a>> {
        if node.tag() == tag {
            return Some(node);
        }
        node.children()
            .filter(|c| c.is_element())
            .find_map(|c| find(c, tag))
    }

    fn transform(html: &str, viewport_w: u32) -> (Document, Vec<(String, Vec<u8>)>) {
        let mut doc = parse_html(html);
        let pairs = replace_inline_svgs(&mut doc, viewport_w);
        (doc, pairs)
    }

    /// The (width, height) attrs of the first synthetic `<img>`.
    fn img_attrs(doc: &Document) -> (u32, u32) {
        let img = find(doc.root(), "svg").expect("synthetic replaced svg");
        (
            img.attr("width").unwrap().parse().unwrap(),
            img.attr("height").unwrap().parse().unwrap(),
        )
    }

    #[test]
    fn svg_becomes_img_with_synthetic_src_and_payload() {
        let (doc, pairs) = transform(
            "<p>before</p><svg width='100' height='40'><rect width='100' height='40' \
             fill='#ff0000'/></svg><p>after</p>",
            800,
        );
        // The tag stays `svg` (so author tag selectors keep matching) but the
        // subtree is gone, replaced by a childless src-carrying element.
        let img = find(doc.root(), "svg").expect("replaced svg");
        assert_eq!(img.children().count(), 0, "subtree consumed");
        let src = img.attr("src").unwrap();
        assert!(src.starts_with(INLINE_SVG_PREFIX), "synthetic src: {src}");
        assert_eq!(img_attrs(&doc), (100, 40));
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].0, src, "payload registered under the img's key");
        let markup = std::str::from_utf8(&pairs[0].1).unwrap();
        assert!(
            markup.contains("xmlns=\"http://www.w3.org/2000/svg\""),
            "namespace injected: {markup}"
        );
        // The surrounding document is untouched.
        assert!(doc.root().text_content().contains("before"));
        assert!(doc.root().text_content().contains("after"));
    }

    #[test]
    fn serialized_svg_decodes_through_the_image_codec() {
        // End-to-end through resvg: the serialized bytes must rasterize at the
        // requested size with the viewBox content scaled to fill — proving the
        // `viewbox` → `viewBox` case fix-up and the width/height injection.
        let (_, pairs) = transform(
            "<svg viewBox='0 0 10 5'><rect width='10' height='5' fill='#ff0000'/></svg>",
            400,
        );
        let img = cerberus_image::ImageCodec::new()
            .decode(&pairs[0].1)
            .expect("resvg accepts the serialized markup");
        assert_eq!((img.size.w, img.size.h), (400, 200), "raster at box size");
        let center = ((img.size.h / 2 * img.size.w + img.size.w / 2) * 4) as usize;
        assert_eq!(
            &img.rgba[center..center + 4],
            &[255, 0, 0, 255],
            "viewBox content scaled to fill the canvas"
        );
    }

    #[test]
    fn sizes_match_chrome_measurements() {
        // Each row cites headless Chromium 139 (see module docs; container
        // 800px). The container-stretch rows use the viewport width here.
        let cases = [
            ("<svg width='100' height='40'/>", (100, 40)),
            ("<svg width='200' viewBox='0 0 100 50'/>", (200, 100)),
            ("<svg height='100' viewBox='0 0 100 50'/>", (200, 100)),
            ("<svg width='200'/>", (200, 150)),
            ("<svg height='80'/>", (300, 80)),
            ("<svg viewBox='0 0 400 100'/>", (800, 200)),
            ("<svg viewBox='0 0 100 400'/>", (800, 3200)),
            ("<svg/>", (300, 150)),
            ("<svg width='120px' height='60px'/>", (120, 60)),
            // Percent lengths resolve against a box we don't know pre-style:
            // treated as absent, falling into the stretch row (Chrome: 400×200).
            ("<svg width='50%' viewBox='0 0 100 50'/>", (800, 400)),
        ];
        for (html, want) in cases {
            let (doc, _) = transform(html, 800);
            assert_eq!(img_attrs(&doc), want, "case {html}");
        }
    }

    #[test]
    fn oversize_boxes_raster_capped_but_box_kept() {
        // viewBox-only 1:4 at a 1280 viewport: box 1280×5120 (Chrome's stretch
        // rule), bitmap capped to 1024 on the longest side, ratio preserved.
        let (doc, pairs) = transform("<svg viewBox='0 0 100 400'/>", 1280);
        assert_eq!(img_attrs(&doc), (1280, 5120));
        let markup = std::str::from_utf8(&pairs[0].1).unwrap();
        assert!(
            markup.contains("width=\"256\"") && markup.contains("height=\"1024\""),
            "raster capped: {markup}"
        );
    }

    #[test]
    fn identical_svgs_dedupe_to_one_payload() {
        let icon = "<svg viewBox='0 0 24 24'><path d='M0 0h24v24H0z'/></svg>";
        let (doc, pairs) = transform(&format!("<p>{icon}{icon}</p>"), 800);
        assert_eq!(pairs.len(), 1, "one payload for two identical icons");
        let mut srcs = Vec::new();
        fn imgs<'a>(n: NodeRef<'a>, out: &mut Vec<&'a str>) {
            if n.tag() == "svg" {
                out.push(n.attr("src").unwrap());
            }
            n.children()
                .filter(|c| c.is_element())
                .for_each(|c| imgs(c, out));
        }
        imgs(doc.root(), &mut srcs);
        assert_eq!(srcs.len(), 2);
        assert_eq!(srcs[0], srcs[1], "both point at the shared key");
    }

    #[test]
    fn case_fixups_namespaces_and_escaping() {
        let (_, pairs) = transform(
            "<svg viewBox='0 0 10 10'>\
               <linearGradient id='g' gradientUnits='userSpaceOnUse'/>\
               <clipPath id='c'/>\
               <use xlink:href='#c'/>\
               <inkscape:junk foo='1'/>\
               <text sodipodi:role='line'>a &amp; b</text>\
             </svg>",
            800,
        );
        let markup = std::str::from_utf8(&pairs[0].1).unwrap();
        assert!(markup.contains("viewBox=\"0 0 10 10\""), "{markup}");
        assert!(markup.contains("<linearGradient"), "{markup}");
        assert!(
            markup.contains("gradientUnits=\"userSpaceOnUse\""),
            "{markup}"
        );
        assert!(markup.contains("<clipPath"), "{markup}");
        assert!(
            markup.contains("xmlns:xlink=\"http://www.w3.org/1999/xlink\""),
            "xlink namespace declared: {markup}"
        );
        assert!(markup.contains("xlink:href=\"#c\""), "{markup}");
        assert!(
            !markup.contains("inkscape"),
            "foreign element dropped: {markup}"
        );
        assert!(
            !markup.contains("sodipodi"),
            "foreign attr dropped: {markup}"
        );
        assert!(markup.contains("a &amp; b"), "text re-escaped: {markup}");
    }

    #[test]
    fn styling_hooks_carry_over_to_the_img() {
        let (doc, _) = transform(
            "<svg class='icon big' id='logo' style='margin:2px' viewBox='0 0 24 24'/>",
            800,
        );
        let img = find(doc.root(), "svg").expect("replaced svg");
        assert_eq!(img.attr("class"), Some("icon big"));
        assert_eq!(img.attr("id"), Some("logo"));
        assert_eq!(img.attr("style"), Some("margin:2px"));
    }

    #[test]
    fn nested_svg_is_serialized_inside_its_root_not_split() {
        let (doc, pairs) = transform(
            "<svg width='20' height='20'><svg width='10' height='10'/></svg>",
            800,
        );
        assert_eq!(pairs.len(), 1, "one root, one payload");
        let markup = std::str::from_utf8(&pairs[0].1).unwrap();
        assert!(
            markup.matches("<svg").count() == 2,
            "inner svg kept: {markup}"
        );
        let mut count = 0;
        fn imgs(n: NodeRef<'_>, count: &mut u32) {
            if n.tag() == "svg" {
                *count += 1;
            }
            n.children()
                .filter(|c| c.is_element())
                .for_each(|c| imgs(c, count));
        }
        imgs(doc.root(), &mut count);
        assert_eq!(count, 1, "one replaced element");
    }
}

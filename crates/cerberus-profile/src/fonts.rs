//! The font-enumeration surface — the pool of font *names* a head reports as
//! "installed", and the per-head sampling that makes that report privacy-safe.
//!
//! Font enumeration is one of the strongest fingerprinting signals on the web: a
//! tracker probes hundreds of font names (via `document.fonts.check`, or the
//! classic trick of measuring text width in a candidate font versus a fallback)
//! and the exact set that resolves is highly identifying — it leaks the OS, the
//! Office/Adobe/creative apps installed, and the user's downloaded webfonts.
//!
//! Cerberus ships only a couple of bundled faces (see `cerberus-text`), so it
//! renders nothing from these names. Instead each head *presents* a synthetic,
//! **per-head-random** set drawn from this catalog: a fixed OS core (always
//! present, so the report stays internally coherent with the persona's platform)
//! plus a seeded sample of the optional pools (Office, Adobe, common webfonts).
//! Because the sample is keyed off the head's seed, the three heads report
//! different font sets and a tracker cannot build one stable cross-site
//! font-fingerprint — while any single head answers consistently within itself
//! (a probe of the same name twice agrees), as a real browser would.
//!
//! These names are **never downloaded**: this is a reporting surface, not a
//! rendering one. The bytes we actually rasterize with live in `cerberus-text`.

use crate::Os;

/// Windows 10/11 default fonts — present on essentially every install (the
/// system + ClearType + bundled international faces). A Windows head always
/// reports these, so its enumeration is coherent with its platform string.
pub const WINDOWS_CORE: &[&str] = &[
    "Arial",
    "Arial Black",
    "Bahnschrift",
    "Calibri",
    "Cambria",
    "Cambria Math",
    "Candara",
    "Comic Sans MS",
    "Consolas",
    "Constantia",
    "Corbel",
    "Courier New",
    "Ebrima",
    "Franklin Gothic Medium",
    "Gabriola",
    "Gadugi",
    "Georgia",
    "Impact",
    "Ink Free",
    "Javanese Text",
    "Leelawadee UI",
    "Lucida Console",
    "Lucida Sans Unicode",
    "Malgun Gothic",
    "Marlett",
    "Microsoft Himalaya",
    "Microsoft JhengHei",
    "Microsoft New Tai Lue",
    "Microsoft PhagsPa",
    "Microsoft Sans Serif",
    "Microsoft Tai Le",
    "Microsoft YaHei",
    "Microsoft Yi Baiti",
    "MingLiU-ExtB",
    "Mongolian Baiti",
    "MS Gothic",
    "MV Boli",
    "Myanmar Text",
    "Nirmala UI",
    "Palatino Linotype",
    "Segoe Print",
    "Segoe Script",
    "Segoe UI",
    "Segoe UI Emoji",
    "Segoe UI Historic",
    "Segoe UI Symbol",
    "SimSun",
    "Sitka",
    "Sylfaen",
    "Symbol",
    "Tahoma",
    "Times New Roman",
    "Trebuchet MS",
    "Verdana",
    "Webdings",
    "Wingdings",
    "Yu Gothic",
];

/// macOS default fonts — present on a stock install (system UI, the classic
/// document faces, and Apple's bundled families).
pub const MACOS_CORE: &[&str] = &[
    "American Typewriter",
    "Andale Mono",
    "Apple Chancery",
    "Apple Color Emoji",
    "Apple SD Gothic Neo",
    "AppleGothic",
    "Arial",
    "Arial Black",
    "Arial Narrow",
    "Arial Rounded MT Bold",
    "Arial Unicode MS",
    "Avenir",
    "Avenir Next",
    "Avenir Next Condensed",
    "Baskerville",
    "Big Caslon",
    "Bodoni 72",
    "Bradley Hand",
    "Brush Script MT",
    "Chalkboard",
    "Chalkboard SE",
    "Chalkduster",
    "Charter",
    "Cochin",
    "Comic Sans MS",
    "Copperplate",
    "Courier",
    "Courier New",
    "Didot",
    "Futura",
    "Geneva",
    "Georgia",
    "Gill Sans",
    "Helvetica",
    "Helvetica Neue",
    "Herculanum",
    "Hoefler Text",
    "Impact",
    "Iowan Old Style",
    "Lucida Grande",
    "Luminari",
    "Marker Felt",
    "Menlo",
    "Monaco",
    "Noteworthy",
    "Optima",
    "Palatino",
    "Papyrus",
    "Phosphate",
    "Rockwell",
    "Savoye LET",
    "SignPainter",
    "Skia",
    "Snell Roundhand",
    "Times",
    "Times New Roman",
    "Trattatello",
    "Trebuchet MS",
    "Verdana",
    "Zapfino",
];

/// Common desktop Linux fonts (metric-compatible libre families plus the usual
/// GNOME/Ubuntu defaults). Reserved for a future Linux persona — no archetype
/// presents Linux yet, but keeping the pool here means adding one is data-only.
pub const LINUX_CORE: &[&str] = &[
    "Bitstream Vera Sans",
    "Bitstream Vera Serif",
    "Bitstream Vera Sans Mono",
    "Cantarell",
    "DejaVu Sans",
    "DejaVu Sans Mono",
    "DejaVu Serif",
    "FreeMono",
    "FreeSans",
    "FreeSerif",
    "Liberation Mono",
    "Liberation Sans",
    "Liberation Serif",
    "Nimbus Mono PS",
    "Nimbus Roman",
    "Nimbus Sans",
    "Noto Sans",
    "Noto Serif",
    "Noto Mono",
    "Ubuntu",
    "Ubuntu Condensed",
    "Ubuntu Mono",
    "URW Bookman",
    "URW Gothic",
];

/// Fonts installed by Microsoft Office (and other productivity suites) — the
/// classic "Office font" enumeration set. Common but not universal, so sampled
/// per head rather than always present.
pub const OFFICE_OPTIONAL: &[&str] = &[
    "Agency FB",
    "Algerian",
    "Baskerville Old Face",
    "Bauhaus 93",
    "Bell MT",
    "Berlin Sans FB",
    "Bernard MT Condensed",
    "Blackadder ITC",
    "Bodoni MT",
    "Book Antiqua",
    "Bookman Old Style",
    "Bookshelf Symbol 7",
    "Bradley Hand ITC",
    "Britannic Bold",
    "Broadway",
    "Californian FB",
    "Calibri Light",
    "Calisto MT",
    "Castellar",
    "Centaur",
    "Century",
    "Century Gothic",
    "Century Schoolbook",
    "Chiller",
    "Colonna MT",
    "Cooper Black",
    "Copperplate Gothic Bold",
    "Copperplate Gothic Light",
    "Curlz MT",
    "Dubai",
    "Elephant",
    "Engravers MT",
    "Eras Bold ITC",
    "Eras Light ITC",
    "Felix Titling",
    "Footlight MT Light",
    "Forte",
    "Franklin Gothic Book",
    "Franklin Gothic Heavy",
    "Freestyle Script",
    "French Script MT",
    "Garamond",
    "Gigi",
    "Gill Sans MT",
    "Gloucester MT Extra Condensed",
    "Goudy Old Style",
    "Goudy Stout",
    "Haettenschweiler",
    "Harlow Solid Italic",
    "Harrington",
    "High Tower Text",
    "Imprint MT Shadow",
    "Informal Roman",
    "Jokerman",
    "Juice ITC",
    "Kristen ITC",
    "Kunstler Script",
    "Lucida Bright",
    "Lucida Calligraphy",
    "Lucida Fax",
    "Lucida Handwriting",
    "Lucida Sans",
    "Lucida Sans Typewriter",
    "Magneto",
    "Maiandra GD",
    "Matura MT Script Capitals",
    "Mistral",
    "Modern No. 20",
    "Monotype Corsiva",
    "MS Reference Sans Serif",
    "MS Reference Specialty",
    "Niagara Engraved",
    "Niagara Solid",
    "OCR A Extended",
    "Old English Text MT",
    "Onyx",
    "Palace Script MT",
    "Papyrus",
    "Parchment",
    "Perpetua",
    "Perpetua Titling MT",
    "Playbill",
    "Poor Richard",
    "Pristina",
    "Rage Italic",
    "Ravie",
    "Rockwell Condensed",
    "Rockwell Extra Bold",
    "Script MT Bold",
    "Showcard Gothic",
    "Snap ITC",
    "Stencil",
    "Tempus Sans ITC",
    "Tw Cen MT",
    "Viner Hand ITC",
    "Vivaldi",
    "Vladimir Script",
    "Wide Latin",
];

/// Fonts installed by Adobe Creative Cloud apps (Photoshop/Illustrator/etc.) and
/// the Source/Kozuka open families Adobe ships. A strong "creative professional"
/// tell, so sampled sparsely per head.
pub const ADOBE_OPTIONAL: &[&str] = &[
    "Adobe Arabic",
    "Adobe Caslon Pro",
    "Adobe Devanagari",
    "Adobe Fan Heiti Std",
    "Adobe Fangsong Std",
    "Adobe Garamond Pro",
    "Adobe Gothic Std",
    "Adobe Hebrew",
    "Adobe Heiti Std",
    "Adobe Kaiti Std",
    "Adobe Ming Std",
    "Adobe Myungjo Std",
    "Adobe Naskh Medium",
    "Adobe Song Std",
    "Adobe Text",
    "Birch Std",
    "Blackoak Std",
    "Brush Script Std",
    "Chaparral Pro",
    "Charlemagne Std",
    "Cooper Std",
    "Giddyup Std",
    "Hobo Std",
    "Kozuka Gothic Pr6N",
    "Kozuka Mincho Pr6N",
    "Letter Gothic Std",
    "Lithos Pro",
    "Mesquite Std",
    "Minion Pro",
    "Myriad Arabic",
    "Myriad Hebrew",
    "Myriad Pro",
    "Nueva Std",
    "OCR A Std",
    "Orator Std",
    "Poplar Std",
    "Prestige Elite Std",
    "Rosewood Std",
    "Source Code Pro",
    "Source Sans Pro",
    "Source Serif Pro",
    "Tekton Pro",
    "Trajan Pro",
];

/// Popular webfonts (Google Fonts and friends) that users download and install
/// locally — a common "developer / designer" signal. Sampled per head.
pub const WEB_OPTIONAL: &[&str] = &[
    "Cascadia Code",
    "DejaVu Sans",
    "DejaVu Sans Mono",
    "DejaVu Serif",
    "Droid Sans",
    "Fira Code",
    "Fira Sans",
    "Inconsolata",
    "Inter",
    "JetBrains Mono",
    "Lato",
    "Liberation Mono",
    "Liberation Sans",
    "Liberation Serif",
    "Merriweather",
    "Montserrat",
    "Noto Sans",
    "Noto Serif",
    "Nunito",
    "Open Sans",
    "Oswald",
    "Poppins",
    "PT Sans",
    "PT Serif",
    "Raleway",
    "Roboto",
    "Roboto Condensed",
    "Roboto Mono",
    "Roboto Slab",
    "Source Code Pro",
    "Ubuntu",
    "Ubuntu Mono",
    "Work Sans",
];

/// A tiny xorshift step for the sampling PRNG. Kept local so the font sampler is
/// self-contained; seeded from the head's dedicated font sub-stream.
fn next(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    x
}

/// Draw a seeded sample of `pool`: a count in `[min, max]`, chosen by a partial
/// Fisher–Yates shuffle so the selection is uniform and stable for a given seed.
fn sample(pool: &[&'static str], seed: u64, min: usize, max: usize) -> Vec<&'static str> {
    if pool.is_empty() {
        return Vec::new();
    }
    let mut state = seed | 1; // never zero (xorshift fixed point)
    let span = max.saturating_sub(min) + 1;
    let count = (min + (next(&mut state) as usize % span)).min(pool.len());
    let mut idx: Vec<usize> = (0..pool.len()).collect();
    for i in 0..count {
        let j = i + (next(&mut state) as usize % (pool.len() - i));
        idx.swap(i, j);
    }
    idx[..count].iter().map(|&i| pool[i]).collect()
}

/// Build the font set a head presents, given its `seed` and presented `os`: the
/// OS core (always) plus a per-head-random sample of the optional pools coherent
/// with that OS. The result is deduplicated (preserving first-seen order) and
/// sorted case-insensitively, matching how browsers expose font family names.
///
/// Coherence: a Windows head can plausibly carry Office/Adobe/webfonts; a macOS
/// head carries its own core plus Adobe/webfonts (Office-for-Mac overlaps the
/// core), but never the Windows-only Office display faces. The counts are chosen
/// so a typical head reports ~70–110 families — realistic, and wide enough that
/// no two heads collide.
pub fn derive_fonts(seed: u64, os: Os) -> Vec<&'static str> {
    let (core, include_office): (&[&str], bool) = match os {
        Os::Windows => (WINDOWS_CORE, true),
        Os::MacOs => (MACOS_CORE, false),
        Os::Linux => (LINUX_CORE, false),
    };

    let mut out: Vec<&'static str> = core.to_vec();
    if include_office {
        out.extend(sample(OFFICE_OPTIONAL, seed ^ 0x0F0F_0F0F_0F0F_0F0F, 8, 40));
    }
    out.extend(sample(ADOBE_OPTIONAL, seed ^ 0x00FF_00FF_00FF_00FF, 0, 14));
    out.extend(sample(WEB_OPTIONAL, seed ^ 0xF0F0_F0F0_F0F0_F0F0, 3, 18));

    // Dedupe (a name may appear in both the core and an optional pool), then sort
    // case-insensitively so enumeration order doesn't itself leak the seed.
    out.sort_by_key(|s| s.to_ascii_lowercase());
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_head_reports_windows_core() {
        let f = derive_fonts(0xABCD_1234, Os::Windows);
        for name in ["Segoe UI", "Calibri", "Arial", "Times New Roman"] {
            assert!(f.contains(&name), "windows core font {name} present");
        }
        // Never a macOS-only face.
        assert!(
            !f.contains(&"Helvetica Neue"),
            "no macOS-only font on Windows"
        );
    }

    #[test]
    fn macos_head_reports_macos_core_not_office() {
        let f = derive_fonts(0x9999, Os::MacOs);
        assert!(f.contains(&"Helvetica Neue"));
        assert!(f.contains(&"Menlo"));
        // Windows-only Office display faces never appear on macOS.
        assert!(!f.contains(&"Segoe UI"));
        assert!(!f.contains(&"Jokerman"));
    }

    #[test]
    fn per_head_sets_differ_but_are_stable() {
        let a1 = derive_fonts(111, Os::Windows);
        let a2 = derive_fonts(111, Os::Windows);
        let b = derive_fonts(222, Os::Windows);
        assert_eq!(a1, a2, "same seed → identical set (self-consistent)");
        assert_ne!(
            a1, b,
            "different seeds → different sets (no cross-head link)"
        );
    }

    #[test]
    fn output_is_sorted_and_deduped() {
        let f = derive_fonts(42, Os::Windows);
        let mut sorted = f.clone();
        sorted.sort_by_key(|s| s.to_ascii_lowercase());
        assert_eq!(f, sorted, "sorted case-insensitively");
        let mut uniq = f.clone();
        uniq.dedup();
        assert_eq!(f.len(), uniq.len(), "no duplicates");
    }

    #[test]
    fn realistic_family_count() {
        // A typical head lands in a believable range, not "3 fonts" (headless
        // tell) nor thousands.
        for seed in [1u64, 2, 3, 1000, 999_999] {
            let n = derive_fonts(seed, Os::Windows).len();
            assert!((50..=140).contains(&n), "windows head reports {n} families");
        }
    }
}

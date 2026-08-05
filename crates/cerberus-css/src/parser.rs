//! CSS parsing + selector matching + specificity. Bootstrapped; no dependencies.
//!
//! Supported selectors: universal `*`, type, `.class`, `#id`, attribute
//! selectors (`[a]`, `[a=v]`, `~= |= ^= $= *=`), structural pseudo-classes
//! (`:first-child`, `:last-child`, `:only-child`, `:nth-child(an+b)`,
//! `:nth-last-child(an+b)`, the `*-of-type` family (`:first-of-type`,
//! `:last-of-type`, `:only-of-type`, `:nth-of-type(an+b)`,
//! `:nth-last-of-type(an+b)`), `:not(…)`,
//! `:is(…)`/`:where(…)` over simple-compound argument lists, `:has(…)` as a
//! direct-child subset,
//! `:root`), grouping `,`, and the descendant / child (`>`) / adjacent-sibling
//! (`+`) / general-sibling (`~`) combinators. Statically answerable state
//! pseudo-classes match from attributes: `:link`/`:any-link` (an `<a>`/`<area>`
//! with `href` — nothing is visited in a fresh head), `:disabled`/`:enabled`
//! (the `disabled` attribute on a form control), `:checked` (`input[checked]`,
//! `option[selected]`). Interaction pseudo-classes (`:hover`, `:focus`,
//! `:active`, `:visited`) and any unknown/unsupported pseudo parse but never
//! match in the static
//! cascade. `@media` blocks are parsed and gated on the viewport plus the fixed
//! desktop persona (`prefers-color-scheme: light`, no reduced-motion/contrast,
//! `hover`/`pointer: fine`, no forced colors) — matching what JS `matchMedia`
//! reports; an unrecognized `@media` feature evaluates to false (never a vacuous
//! match). Other `@`-rules are skipped.

use std::rc::Rc;

/// `(ids, classes+attrs+pseudos, types)` specificity, compared as a tuple.
pub type Specificity = (u32, u32, u32);

/// The viewport an `@media` query is evaluated against.
#[derive(Clone, Copy, Debug)]
pub struct MediaContext {
    pub width: u32,
    pub height: u32,
}

/// An element reduced to what selectors match against.
#[derive(Clone, Debug, Default)]
pub struct SiblingRef {
    pub tag: String,
    pub id: Option<String>,
    pub classes: Vec<String>,
    pub attrs: Vec<(String, String)>,
    /// The element's direct element children, one level deep (the children's
    /// own lists are empty), so `:has(...)` can check them during the cascade.
    pub children: Rc<[SiblingRef]>,
}

/// An element on the match path: its parent's element children (shared via `Rc`
/// so the cascade is O(n) not O(n²)) and its index among them. This lets sibling
/// combinators and `:nth-child` be evaluated without parent pointers in the DOM.
pub struct ElemRef {
    pub siblings: Rc<[SiblingRef]>,
    pub index: usize,
}

/// How a compound relates to the compound on its left.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Combinator {
    #[default]
    Descendant,
    Child,
    Adjacent,
    General,
}

/// An attribute selector, e.g. `[type="text"]` or `[href^="https"]`.
#[derive(Clone, Debug)]
struct AttrSel {
    name: String,
    op: AttrOp,
    value: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum AttrOp {
    Exists,
    Eq,
    Include, // ~=  (space-separated word match)
    Dash,    // |=  (exact or prefix-then-hyphen)
    Prefix,  // ^=
    Suffix,  // $=
    Substr,  // *=
}

/// A renderable pseudo-element (`::before`/`::after`, either colon form).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PseudoElement {
    Before,
    After,
}

#[derive(Clone, Debug)]
enum Pseudo {
    FirstChild,
    LastChild,
    OnlyChild,
    NthChild(i32, i32),     // a, b  for an+b (1-based position among all siblings)
    NthLastChild(i32, i32), // an+b counting from the END of the sibling list
    // The `*-of-type` family: same as the `*-child` ones but counting only
    // siblings that share this element's tag (1-based position among them).
    FirstOfType,
    LastOfType,
    OnlyOfType,
    NthOfType(i32, i32),
    NthLastOfType(i32, i32), // an+b among same-tag siblings, counting from the end
    Root,
    /// `:link` / `:any-link` — an `<a>`/`<area>` carrying `href`. Nothing is
    /// ever visited in a fresh sealed head, so `:link` covers every hyperlink,
    /// exactly like Chrome with empty history (and `a:link{…}` is how a large
    /// share of sites style their links — dropping it left them UA blue).
    Link,
    /// `:disabled` / `:enabled` — a form control with/without the `disabled`
    /// attribute (rest state; fieldset inheritance not modelled).
    Disabled,
    Enabled,
    /// `:checked` — `input[checked]` / `option[selected]` (rest state).
    Checked,
    /// `:is(...)`/`:where(...)`: matches when ANY argument compound matches
    /// the element itself. They differ only in specificity — `:where` adds
    /// nothing, `:is` its most specific argument (CSS Selectors 4).
    Is(Vec<Compound>),
    Where(Vec<Compound>),
    /// `:has(...)` restricted to DIRECT children: matches when any element
    /// child matches any argument compound. Both `:has(> x)` and `:has(x)`
    /// check children only — `SiblingRef` carries one level of children, so
    /// deeper descent isn't reachable from the match path. This under-matches
    /// Chrome for descendant arguments but never over-matches.
    HasChild(Vec<Compound>),
    Never, // :hover/:focus/unknown/unsupported — no static match
}

#[derive(Clone, Debug, Default)]
struct Compound {
    combinator: Combinator,
    universal: bool,
    tag: Option<String>,
    id: Option<String>,
    classes: Vec<String>,
    attrs: Vec<AttrSel>,
    pseudos: Vec<Pseudo>,
    not: Vec<Compound>,
    /// `::before`/`::after` on this compound. Only meaningful on a selector's
    /// LAST compound: the selector then targets the generated box, never the
    /// real element ([`Selector::matches`] refuses it; the cascade queries it
    /// through [`Rule::matches_pseudo`]).
    pseudo_element: Option<PseudoElement>,
}

impl Compound {
    fn specificity(&self) -> Specificity {
        fn add(s: &mut Specificity, o: Specificity) {
            s.0 += o.0;
            s.1 += o.1;
            s.2 += o.2;
        }
        let mut s = (
            u32::from(self.id.is_some()),
            self.classes.len() as u32 + self.attrs.len() as u32,
            // A pseudo-element counts at type level (CSS 2.1 §6.4.3).
            u32::from(self.tag.is_some()) + u32::from(self.pseudo_element.is_some()),
        );
        for p in &self.pseudos {
            match p {
                // `:is()`/`:has()` contribute their most specific argument;
                // `:where()` contributes zero (CSS Selectors 4).
                Pseudo::Is(args) | Pseudo::HasChild(args) => {
                    if let Some(m) = args.iter().map(Compound::specificity).max() {
                        add(&mut s, m);
                    }
                }
                Pseudo::Where(_) => {}
                // Every other pseudo-class counts at class level.
                _ => s.1 += 1,
            }
        }
        for n in &self.not {
            add(&mut s, n.specificity());
        }
        s
    }

    /// Match this compound against `el` at sibling position `index` of `total`.
    /// `siblings` is the full element-sibling list at this level (for the
    /// `*-of-type` pseudos, which count only same-tag siblings).
    fn matches(
        &self,
        el: &SiblingRef,
        index: usize,
        total: usize,
        siblings: &[SiblingRef],
    ) -> bool {
        if let Some(t) = &self.tag {
            if t != &el.tag {
                return false;
            }
        }
        if let Some(id) = &self.id {
            if Some(id) != el.id.as_ref() {
                return false;
            }
        }
        if !self.classes.iter().all(|c| el.classes.contains(c)) {
            return false;
        }
        if !self.attrs.iter().all(|a| attr_matches(a, el)) {
            return false;
        }
        for p in &self.pseudos {
            if !pseudo_matches(p, el, index, total, siblings) {
                return false;
            }
        }
        // :not(...) — none of the inner simple compounds may match.
        if self
            .not
            .iter()
            .any(|n| n.matches(el, index, total, siblings))
        {
            return false;
        }
        true
    }
}

fn attr_matches(a: &AttrSel, el: &SiblingRef) -> bool {
    let Some((_, v)) = el.attrs.iter().find(|(k, _)| k == &a.name) else {
        return false;
    };
    match a.op {
        AttrOp::Exists => true,
        AttrOp::Eq => v == &a.value,
        AttrOp::Include => v.split_whitespace().any(|w| w == a.value),
        AttrOp::Dash => v == &a.value || v.starts_with(&format!("{}-", a.value)),
        AttrOp::Prefix => !a.value.is_empty() && v.starts_with(&a.value),
        AttrOp::Suffix => !a.value.is_empty() && v.ends_with(&a.value),
        AttrOp::Substr => !a.value.is_empty() && v.contains(&a.value),
    }
}

fn pseudo_matches(
    p: &Pseudo,
    el: &SiblingRef,
    index: usize,
    total: usize,
    siblings: &[SiblingRef],
) -> bool {
    let pos = index as i32 + 1; // 1-based position among all siblings

    // For the `*-of-type` pseudos: this element's 1-based rank among same-tag
    // siblings, and how many same-tag siblings there are in total.
    let type_pos = || {
        siblings[..=index.min(siblings.len().saturating_sub(1))]
            .iter()
            .filter(|s| s.tag == el.tag)
            .count() as i32
    };
    let type_total = || siblings.iter().filter(|s| s.tag == el.tag).count();
    match p {
        Pseudo::FirstChild => index == 0,
        Pseudo::LastChild => index + 1 == total,
        Pseudo::OnlyChild => total == 1,
        Pseudo::NthChild(a, b) => nth_matches(*a, *b, pos),
        Pseudo::NthLastChild(a, b) => nth_matches(*a, *b, total as i32 - index as i32),
        Pseudo::FirstOfType => type_pos() == 1,
        Pseudo::LastOfType => type_pos() as usize == type_total(),
        Pseudo::OnlyOfType => type_total() == 1,
        Pseudo::NthOfType(a, b) => nth_matches(*a, *b, type_pos()),
        Pseudo::NthLastOfType(a, b) => nth_matches(*a, *b, type_total() as i32 - type_pos() + 1),
        Pseudo::Root => el.tag == "html",
        Pseudo::Link => (el.tag == "a" || el.tag == "area") && has_attr(el, "href"),
        Pseudo::Disabled => is_form_control(&el.tag) && has_attr(el, "disabled"),
        Pseudo::Enabled => is_form_control(&el.tag) && !has_attr(el, "disabled"),
        Pseudo::Checked => {
            (el.tag == "input" && has_attr(el, "checked"))
                || (el.tag == "option" && has_attr(el, "selected"))
        }
        Pseudo::Is(args) | Pseudo::Where(args) => {
            args.iter().any(|a| a.matches(el, index, total, siblings))
        }
        Pseudo::HasChild(args) => el.children.iter().enumerate().any(|(i, child)| {
            args.iter()
                .any(|a| a.matches(child, i, el.children.len(), &el.children))
        }),
        Pseudo::Never => false,
    }
}

fn has_attr(el: &SiblingRef, name: &str) -> bool {
    el.attrs.iter().any(|(k, _)| k == name)
}

/// The tags `:enabled`/`:disabled` apply to (form-associated elements).
fn is_form_control(tag: &str) -> bool {
    matches!(
        tag,
        "button" | "input" | "select" | "textarea" | "option" | "optgroup" | "fieldset"
    )
}

/// Whether `pos` (1-based) satisfies `an + b` for some integer n ≥ 0.
fn nth_matches(a: i32, b: i32, pos: i32) -> bool {
    if a == 0 {
        return pos == b;
    }
    let diff = pos - b;
    diff % a == 0 && diff / a >= 0
}

/// A compound chain (left to right) with combinators between them.
#[derive(Clone, Debug)]
pub struct Selector {
    compounds: Vec<Compound>,
}

impl Selector {
    fn specificity(&self) -> Specificity {
        self.compounds.iter().fold((0, 0, 0), |a, c| {
            let s = c.specificity();
            (a.0 + s.0, a.1 + s.1, a.2 + s.2)
        })
    }

    /// Match against an ancestor path (root … element); the element is last.
    /// A selector targeting a pseudo-element never matches the element itself.
    fn matches(&self, path: &[ElemRef]) -> bool {
        if self.compounds.is_empty()
            || path.is_empty()
            || self.compounds.iter().any(|c| c.pseudo_element.is_some())
        {
            return false;
        }
        let last = path.len() - 1;
        self.match_at(self.compounds.len() - 1, path, last, path[last].index)
    }

    /// Match this selector's BASE (originating-element part) against `path`,
    /// for a selector whose last compound targets the given pseudo-element.
    fn matches_pseudo(&self, path: &[ElemRef], which: PseudoElement) -> bool {
        let Some(last_c) = self.compounds.last() else {
            return false;
        };
        if last_c.pseudo_element != Some(which)
            || path.is_empty()
            || self.compounds[..self.compounds.len() - 1]
                .iter()
                .any(|c| c.pseudo_element.is_some())
        {
            return false;
        }
        let last = path.len() - 1;
        self.match_at(self.compounds.len() - 1, path, last, path[last].index)
    }

    /// Recursive, backtracking match: compound `ci` against the element at
    /// path level `pi`, sibling index `sib`.
    fn match_at(&self, ci: usize, path: &[ElemRef], pi: usize, sib: usize) -> bool {
        let level = &path[pi];
        let total = level.siblings.len();
        if !self.compounds[ci].matches(&level.siblings[sib], sib, total, &level.siblings) {
            return false;
        }
        if ci == 0 {
            return true;
        }
        // The combinator stored on `ci` describes how it relates to `ci - 1`.
        match self.compounds[ci].combinator {
            Combinator::Descendant => {
                // Some ancestor matches compound ci-1.
                (0..pi)
                    .rev()
                    .any(|p| self.match_at(ci - 1, path, p, path[p].index))
            }
            Combinator::Child => pi > 0 && self.match_at(ci - 1, path, pi - 1, path[pi - 1].index),
            Combinator::Adjacent => sib > 0 && self.match_at(ci - 1, path, pi, sib - 1),
            Combinator::General => (0..sib).rev().any(|s| self.match_at(ci - 1, path, pi, s)),
        }
    }
}

/// An `@media` query: an OR of AND-ed feature lists (a comma list of queries).
#[derive(Clone, Debug)]
pub struct MediaQuery {
    branches: Vec<Vec<MediaFeature>>,
}

#[derive(Clone, Copy, Debug)]
enum MediaFeature {
    MinWidth(u32),
    MaxWidth(u32),
    MinHeight(u32),
    MaxHeight(u32),
    Portrait,
    Landscape,
    /// A discrete preference/capability feature the desktop persona satisfies
    /// (e.g. `prefers-color-scheme: light`), pre-evaluated at parse time.
    AlwaysTrue,
    /// A recognized feature the persona does NOT satisfy (e.g. a
    /// `prefers-color-scheme: dark` block), or an *unrecognized* feature — which
    /// per the CSS spec evaluates to false. Dropping unknown features instead
    /// left an empty, vacuously-matching branch, so every dark-mode block
    /// applied on a light page and washed out the design-system colors.
    AlwaysFalse,
}

/// Map a persona-match boolean to the corresponding pre-evaluated feature.
fn persona(matches: bool) -> MediaFeature {
    if matches {
        MediaFeature::AlwaysTrue
    } else {
        MediaFeature::AlwaysFalse
    }
}

impl MediaQuery {
    /// Whether the query matches `ctx` (any branch fully matches).
    pub fn matches(&self, ctx: MediaContext) -> bool {
        self.branches
            .iter()
            .any(|feats| feats.iter().all(|f| feature_matches(*f, ctx)))
    }
}

fn feature_matches(f: MediaFeature, ctx: MediaContext) -> bool {
    match f {
        MediaFeature::MinWidth(px) => ctx.width >= px,
        MediaFeature::MaxWidth(px) => ctx.width <= px,
        MediaFeature::MinHeight(px) => ctx.height >= px,
        MediaFeature::MaxHeight(px) => ctx.height <= px,
        MediaFeature::Portrait => ctx.height >= ctx.width,
        MediaFeature::Landscape => ctx.width > ctx.height,
        MediaFeature::AlwaysTrue => true,
        MediaFeature::AlwaysFalse => false,
    }
}

/// A rule: a group of selectors, declarations, and the `@media` it lives in.
#[derive(Clone, Debug)]
pub struct Rule {
    pub selectors: Vec<Selector>,
    pub declarations: Vec<(String, String, bool)>,
    pub media: Option<MediaQuery>,
}

impl Rule {
    /// The highest specificity among selectors matching `path`, if any.
    pub fn matches(&self, path: &[ElemRef]) -> Option<Specificity> {
        self.selectors
            .iter()
            .filter(|s| s.matches(path))
            .map(Selector::specificity)
            .max()
    }

    /// Whether this rule applies under `ctx` (no `@media`, or it matches).
    pub fn applies(&self, ctx: MediaContext) -> bool {
        self.media.as_ref().is_none_or(|m| m.matches(ctx))
    }

    /// The highest specificity among this rule's selectors that target the
    /// given pseudo-element of the element at `path`, if any — how the cascade
    /// finds the declarations for a generated `::before`/`::after` box.
    pub fn matches_pseudo(&self, path: &[ElemRef], which: PseudoElement) -> Option<Specificity> {
        self.selectors
            .iter()
            .filter(|s| s.matches_pseudo(path, which))
            .map(Selector::specificity)
            .max()
    }
}

/// The bucket a selector is indexed under for cascade pruning: its rightmost
/// (subject) compound's most specific of id > class > tag, else universal. An
/// element only needs to test rules whose key it actually carries, which turns
/// the per-element cascade from O(all rules) into O(rules that could match) —
/// the difference between usable and unusable on a big page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BucketKey {
    Id(String),
    Class(String),
    Tag(String),
    Universal,
}

impl Selector {
    /// The subject (rightmost) compound's key. Uses raw strings so it stays
    /// exactly consistent with [`Compound::matches`] (which compares them
    /// verbatim) — an element probing by the same raw tag/id/class can never
    /// miss a rule this pruning kept out.
    fn bucket_key(&self) -> BucketKey {
        match self.compounds.last() {
            Some(c) if c.id.is_some() => BucketKey::Id(c.id.clone().unwrap()),
            Some(c) if !c.classes.is_empty() => BucketKey::Class(c.classes[0].clone()),
            Some(c) if c.tag.is_some() => BucketKey::Tag(c.tag.clone().unwrap()),
            _ => BucketKey::Universal,
        }
    }

    /// Whether this selector targets a `::before`/`::after` (its subject carries
    /// a pseudo-element) — i.e. it feeds the generated-content cascade, not the
    /// element cascade.
    fn targets_pseudo_element(&self) -> bool {
        self.compounds
            .last()
            .is_some_and(|c| c.pseudo_element.is_some())
    }
}

impl Rule {
    /// Bucket keys for this rule's element-matching selectors (one per selector
    /// that targets the element itself), for indexing the normal cascade.
    pub fn bucket_keys_normal(&self) -> Vec<BucketKey> {
        self.selectors
            .iter()
            .filter(|s| !s.targets_pseudo_element())
            .map(Selector::bucket_key)
            .collect()
    }

    /// Bucket keys for this rule's `::before`/`::after` selectors, for indexing
    /// the generated-content cascade.
    pub fn bucket_keys_pseudo(&self) -> Vec<BucketKey> {
        self.selectors
            .iter()
            .filter(|s| s.targets_pseudo_element())
            .map(Selector::bucket_key)
            .collect()
    }
}

/// A parsed stylesheet.
#[derive(Clone, Debug, Default)]
pub struct Stylesheet {
    pub rules: Vec<Rule>,
    /// The `font-family` names declared by this sheet's `@font-face` rules,
    /// lowercased. We never fetch the font bytes (ADR-0005: a fixed bundled set,
    /// no external font loads) — but a page's own web font, once "loaded", is
    /// reported by `document.fonts.check()` in a real browser, so we surface
    /// these names to present the same answer (rendering substitutes a
    /// metric-compatible bundled face). See the DOM prelude's `document.fonts`.
    pub font_face_families: Vec<String>,
}

/// Parse a full stylesheet.
pub fn parse_stylesheet(css: &str) -> Stylesheet {
    let css = strip_comments(css);
    let mut rules = Vec::new();
    parse_rules_into(&css, None, &mut rules);
    let font_face_families = extract_font_face_families(&css);
    Stylesheet {
        rules,
        font_face_families,
    }
}

/// Scan a (comment-stripped) stylesheet for every `@font-face` block's declared
/// `font-family`, returning the names lowercased and de-quoted. A flat scan (not
/// the recursive rule parser) so it also catches `@font-face` nested in `@media`.
fn extract_font_face_families(css: &str) -> Vec<String> {
    let lower = css.to_ascii_lowercase();
    let mut out: Vec<String> = Vec::new();
    let mut search = 0;
    while let Some(rel) = lower[search..].find("@font-face") {
        let at = search + rel;
        // The block body between the next `{` and its matching `}`.
        let Some(open) = css[at..].find('{').map(|i| at + i + 1) else {
            break;
        };
        let Some(len) = matching_brace(&css[open..]) else {
            break;
        };
        let body = &css[open..open + len];
        // Pull `font-family: <name>` (the first; @font-face has exactly one).
        for decl in body.split(';') {
            if let Some((prop, val)) = decl.split_once(':') {
                if prop.trim().eq_ignore_ascii_case("font-family") {
                    let name = val
                        .trim()
                        .trim_matches(['"', '\''])
                        .trim()
                        .to_ascii_lowercase();
                    if !name.is_empty() && !out.contains(&name) {
                        out.push(name);
                    }
                    break;
                }
            }
        }
        search = open + len + 1;
    }
    out
}

/// Whether an `@supports` block's rules should apply.
///
/// We can't fully probe feature support the way a complete engine does, so the
/// default stays "apply the inner rules" — an `@supports` block is overwhelmingly
/// a progressive enhancement. We refine that in one safe, high-value direction: a
/// `not(<X>)` whose feature `X` we *definitely* support is false, so its block —
/// typically a legacy fallback that would otherwise override the modern rule in
/// source order — is dropped. Every condition we can't decide still applies, so
/// this never discards rules we would previously have kept for a feature we do
/// support.
fn supports_condition_holds(cond: &str) -> bool {
    match strip_supports_not(cond.trim()) {
        Some(inner) => !feature_definitely_supported(inner),
        None => true,
    }
}

/// If `cond` is a `not(<x>)` / `not (<x>)` query, return `<x>` (still wrapped in
/// its parentheses); otherwise `None`. Requires a `(` after `not` so a property
/// name that merely starts with "not" isn't mistaken for the operator.
fn strip_supports_not(cond: &str) -> Option<&str> {
    let rest = cond.strip_prefix("not")?.trim_start();
    rest.starts_with('(').then_some(rest)
}

/// Whether we definitely support the feature query `(prop: value)`. Conservative:
/// only `display`/`position` — by far the most common `@supports` detections
/// (grid/flex/sticky fallbacks) — are answered, each against the same value set
/// the cascade actually accepts, so there's no drift. Anything else returns
/// `false`, which makes an undecidable `not(...)` fall back to applying.
fn feature_definitely_supported(query: &str) -> bool {
    let Some((prop, value)) = query
        .trim()
        .strip_prefix('(')
        .and_then(|s| s.strip_suffix(')'))
        .and_then(|inner| inner.split_once(':'))
    else {
        return false;
    };
    let value = value.trim().to_ascii_lowercase();
    match prop.trim().to_ascii_lowercase().as_str() {
        "display" => crate::parse_display(&value).is_some(),
        "position" => matches!(
            value.as_str(),
            "static" | "relative" | "absolute" | "fixed" | "sticky"
        ),
        _ => false,
    }
}

/// Parse rules from `text`, tagging each with `media`. `@media` blocks recurse
/// (their inner rules inherit the query); other `@`-rules are skipped.
fn parse_rules_into(text: &str, media: Option<&MediaQuery>, out: &mut Vec<Rule>) {
    let mut rest = text;
    loop {
        rest = rest.trim_start();
        if rest.is_empty() {
            break;
        }
        if rest.starts_with('@') {
            if let Some(after_at) = rest.strip_prefix("@media") {
                if let Some(brace) = after_at.find('{') {
                    let query = parse_media_query(after_at[..brace].trim());
                    let inner = &after_at[brace + 1..];
                    if let Some(end) = matching_brace(inner) {
                        parse_rules_into(&inner[..end], Some(&query), out);
                        rest = &inner[end + 1..];
                        continue;
                    }
                }
            }
            // `@supports`: apply the inner rules unless we can prove the condition
            // is false (see `supports_condition_holds`). Source order is preserved
            // so a later fallback still wins where it should.
            if let Some(after_at) = rest.strip_prefix("@supports") {
                if let Some(brace) = after_at.find('{') {
                    let cond = after_at[..brace].trim();
                    let inner = &after_at[brace + 1..];
                    if let Some(end) = matching_brace(inner) {
                        if supports_condition_holds(cond) {
                            parse_rules_into(&inner[..end], media, out);
                        }
                        rest = &inner[end + 1..];
                        continue;
                    }
                }
            }
            rest = skip_at_rule(rest);
            continue;
        }
        let Some(brace) = rest.find('{') else {
            break;
        };
        let selectors_text = &rest[..brace];
        let after = &rest[brace + 1..];
        let Some(end) = matching_brace(after) else {
            break;
        };
        let declarations = parse_declaration_block(&after[..end]);
        rest = &after[end + 1..];

        let selectors = parse_selectors(selectors_text);
        if !selectors.is_empty() {
            out.push(Rule {
                selectors,
                declarations,
                media: media.cloned(),
            });
        }
    }
}

/// Parse a `prop: value; …` block (also used for inline `style=` attributes).
///
/// Each declaration carries an `important` flag: a trailing `!important` is
/// stripped from the value *and recorded*, so the cascade can let it override
/// normal declarations (previously the flag was parsed then dropped, so
/// `color: red !important` competed at normal priority — a cascade bug).
pub fn parse_declaration_block(text: &str) -> Vec<(String, String, bool)> {
    let mut decls = Vec::new();
    for chunk in text.split(';') {
        let chunk = chunk.trim();
        if chunk.is_empty() {
            continue;
        }
        if let Some((prop, value)) = chunk.split_once(':') {
            let prop = prop.trim().to_ascii_lowercase();
            let mut value = value.trim().to_string();
            let low = value.to_ascii_lowercase();
            let important = match low.rfind("!important") {
                Some(pos) => {
                    value.truncate(pos);
                    value = value.trim().to_string();
                    true
                }
                None => false,
            };
            if !prop.is_empty() {
                decls.push((prop, value, important));
            }
        }
    }
    decls
}

fn parse_media_query(text: &str) -> MediaQuery {
    let branches = text
        .split(',')
        .map(|branch| {
            let mut feats = Vec::new();
            // Split on `and`. Each part is either a `(feature)` or a bare media
            // type/qualifier. We render as a screen, so only `screen`/`all` match;
            // `print` (and other non-screen types) must NOT — they were vacuously
            // matching, so e.g. Wikipedia's `@media print{a{color:#000!important}}`
            // leaked onto the screen render.
            for part in branch.split(" and ") {
                let part = part.trim().trim_start_matches("only ").trim();
                if let Some(inner) = part.strip_prefix('(').and_then(|p| p.strip_suffix(')')) {
                    let inner = inner.trim();
                    if let Some(fs) = parse_range_media_feature(inner) {
                        feats.extend(fs);
                    } else if let Some(f) = parse_media_feature(inner) {
                        feats.push(f);
                    }
                } else if !part.is_empty() {
                    // A bare media type, optionally negated with `not`.
                    let (ty, negated) = match part.strip_prefix("not ") {
                        Some(rest) => (rest.trim(), true),
                        None => (part, false),
                    };
                    let is_screen =
                        ty.eq_ignore_ascii_case("screen") || ty.eq_ignore_ascii_case("all");
                    // `screen`/`all` match; `not screen` / `print` / others don't.
                    if is_screen == negated {
                        feats.push(MediaFeature::AlwaysFalse);
                    }
                }
            }
            feats
        })
        .collect();
    MediaQuery { branches }
}

/// Media Queries Level 4 range syntax: `(width <= 1000px)`, `(width < 1200px)`,
/// the reversed `(1000px >= width)`, and the double-bounded
/// `(400px <= width <= 1000px)`. Standard on newly-built sites — iana.org alone
/// has 64 such blocks; treating them as unrecognized (AlwaysFalse) silently
/// dropped every responsive tier, so pages rendered their desktop-widest CSS
/// at every viewport. Returns `None` when `text` isn't range syntax (so the
/// plain `name: value` path runs), and `[AlwaysFalse]` for range syntax over a
/// feature we can't evaluate (per spec: unknown → not matching).
fn parse_range_media_feature(text: &str) -> Option<Vec<MediaFeature>> {
    if text.contains(':') || !text.contains(['<', '>', '=']) {
        return None;
    }
    // Tokenize into operands separated by comparison operators.
    let mut ops: Vec<&str> = Vec::new();
    let mut operands: Vec<&str> = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find(['<', '>', '=']) {
        operands.push(rest[..i].trim());
        let len = if rest.as_bytes()[i] != b'=' && rest.as_bytes().get(i + 1) == Some(&b'=') {
            2
        } else {
            1
        };
        ops.push(&rest[i..i + len]);
        rest = &rest[i + len..];
    }
    operands.push(rest.trim());

    // `500px <= width` reads as `width >= 500px`: normalize name-on-left.
    fn flip(op: &str) -> &str {
        match op {
            "<=" => ">=",
            ">=" => "<=",
            "<" => ">",
            ">" => "<",
            other => other,
        }
    }
    // One comparison as Min/Max features (`=` pins both bounds). `<`/`>` are
    // exclusive; the viewport is integer px, so ±1 is exact.
    fn feats(name: &str, op: &str, value: &str) -> Option<Vec<MediaFeature>> {
        let px = media_len_px(value)?;
        let width = match name.trim().to_ascii_lowercase().as_str() {
            "width" => true,
            "height" => false,
            _ => return None,
        };
        let min = |px: u32| {
            if width {
                MediaFeature::MinWidth(px)
            } else {
                MediaFeature::MinHeight(px)
            }
        };
        let max = |px: u32| {
            if width {
                MediaFeature::MaxWidth(px)
            } else {
                MediaFeature::MaxHeight(px)
            }
        };
        Some(match op {
            "<=" => vec![max(px)],
            "<" => vec![max(px.saturating_sub(1))],
            ">=" => vec![min(px)],
            ">" => vec![min(px.saturating_add(1))],
            "=" => vec![min(px), max(px)],
            _ => return None,
        })
    }
    let parsed = match (operands.as_slice(), ops.as_slice()) {
        // `width <= 1000px` / `1000px >= width`
        ([a, b], [op]) => {
            if a.eq_ignore_ascii_case("width") || a.eq_ignore_ascii_case("height") {
                feats(a, op, b)
            } else {
                feats(b, flip(op), a)
            }
        }
        // `400px <= width <= 1000px` (each bound normalized name-on-left).
        ([lo, name, hi], [op1, op2]) => match (feats(name, flip(op1), lo), feats(name, op2, hi)) {
            (Some(mut a), Some(b)) => {
                a.extend(b);
                Some(a)
            }
            _ => None,
        },
        _ => None,
    };
    // Range syntax we couldn't evaluate (unknown feature/unit) is still range
    // syntax: unknown → false, not a fall-through to the `name: value` parser.
    Some(parsed.unwrap_or_else(|| vec![MediaFeature::AlwaysFalse]))
}

/// A `<length>` in a media-feature range, in px. Media-query `em`/`rem` resolve
/// against the initial font size (16px), never the element's.
fn media_len_px(v: &str) -> Option<u32> {
    let v = v.trim().to_ascii_lowercase();
    let num_end = v
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(v.len());
    let n: f32 = v[..num_end].parse().ok()?;
    let px = match v[num_end..].trim() {
        "px" => n,
        "em" | "rem" => n * 16.0,
        "" if n == 0.0 => 0.0,
        _ => return None,
    };
    Some(px.round().max(0.0) as u32)
}

fn parse_media_feature(text: &str) -> Option<MediaFeature> {
    let (name, value) = match text.split_once(':') {
        Some((n, v)) => (n.trim().to_ascii_lowercase(), v.trim().to_ascii_lowercase()),
        None => (text.trim().to_ascii_lowercase(), String::new()),
    };
    let px = || -> Option<u32> {
        let digits: String = value.chars().take_while(|c| c.is_ascii_digit()).collect();
        digits.parse().ok()
    };
    match name.as_str() {
        "min-width" => Some(MediaFeature::MinWidth(px()?)),
        "max-width" => Some(MediaFeature::MaxWidth(px()?)),
        "min-height" => Some(MediaFeature::MinHeight(px()?)),
        "max-height" => Some(MediaFeature::MaxHeight(px()?)),
        "orientation" => match value.as_str() {
            "portrait" => Some(MediaFeature::Portrait),
            "landscape" => Some(MediaFeature::Landscape),
            _ => None,
        },
        // Discrete preference/capability features, evaluated against the fixed
        // desktop persona: light theme, no motion/contrast preference, a mouse
        // (hover + fine pointer), no forced colors. This is the SAME persona JS
        // `matchMedia` reports, so CSS `@media` and script agree. A block the
        // persona doesn't satisfy (e.g. `prefers-color-scheme: dark`) must NOT
        // apply.
        "prefers-color-scheme" => Some(persona(value == "light")),
        "prefers-reduced-motion" => Some(persona(value != "reduce")),
        "prefers-reduced-transparency" => Some(persona(value != "reduce")),
        "prefers-contrast" => Some(persona(value == "no-preference" || value.is_empty())),
        "forced-colors" => Some(persona(value == "none")),
        "inverted-colors" => Some(persona(value == "none")),
        "hover" | "any-hover" => Some(persona(value == "hover")),
        "pointer" | "any-pointer" => Some(persona(value == "fine")),
        // Any other feature is unrecognized → false (CSS spec), rather than
        // being dropped into a vacuously-matching empty branch.
        _ => Some(MediaFeature::AlwaysFalse),
    }
}

fn parse_selectors(text: &str) -> Vec<Selector> {
    // Split at top-level commas only, so the comma inside a functional
    // pseudo-class argument (`:is(a, b)`) doesn't split the selector itself.
    crate::split_top_commas(text)
        .iter()
        .filter_map(|s| {
            let sel = parse_selector(s.trim());
            (!sel.compounds.is_empty()).then_some(sel)
        })
        .collect()
}

fn parse_selector(text: &str) -> Selector {
    let mut compounds = Vec::new();
    let mut pending = Combinator::Descendant;
    // Top-level whitespace split: spaces inside a functional pseudo argument
    // (`:has(> a)`, `:nth-child(2n + 1)`) don't break the compound apart.
    for token in crate::split_top(text) {
        match token.as_str() {
            ">" => pending = Combinator::Child,
            "+" => pending = Combinator::Adjacent,
            "~" => pending = Combinator::General,
            _ => {
                if let Some(mut c) = parse_compound(&token) {
                    // The first compound's combinator is unused; the rest carry
                    // their relation to the compound on their left.
                    c.combinator = if compounds.is_empty() {
                        Combinator::Descendant
                    } else {
                        pending
                    };
                    compounds.push(c);
                    pending = Combinator::Descendant;
                }
            }
        }
    }
    Selector { compounds }
}

fn parse_compound(token: &str) -> Option<Compound> {
    let mut c = Compound::default();
    let chars: Vec<char> = token.chars().collect();
    let mut i = 0;

    // Leading type / universal.
    let start = i;
    while i < chars.len() && !matches!(chars[i], '.' | '#' | ':' | '[') {
        i += 1;
    }
    let head: String = chars[start..i].iter().collect();
    if head == "*" {
        c.universal = true;
    } else if !head.is_empty() {
        c.tag = Some(head.to_ascii_lowercase());
    }

    while i < chars.len() {
        let sep = chars[i];
        i += 1;
        match sep {
            '[' => {
                let s = i;
                while i < chars.len() && chars[i] != ']' {
                    i += 1;
                }
                let body: String = chars[s..i].iter().collect();
                i += usize::from(i < chars.len()); // consume ']'
                if let Some(a) = parse_attr_sel(&body) {
                    c.attrs.push(a);
                }
            }
            ':' => {
                // Skip a second ':' (pseudo-element); we don't match those.
                if i < chars.len() && chars[i] == ':' {
                    i += 1;
                }
                let s = i;
                while i < chars.len() && !matches!(chars[i], '.' | '#' | ':' | '[' | '(') {
                    i += 1;
                }
                let name: String = chars[s..i].iter().collect::<String>().to_ascii_lowercase();
                let mut arg = String::new();
                if i < chars.len() && chars[i] == '(' {
                    i += 1;
                    let s = i;
                    let mut depth = 1;
                    while i < chars.len() && depth > 0 {
                        match chars[i] {
                            '(' => depth += 1,
                            ')' => depth -= 1,
                            _ => {}
                        }
                        if depth > 0 {
                            i += 1;
                        }
                    }
                    arg = chars[s..i].iter().collect();
                    i += usize::from(i < chars.len()); // consume ')'
                }
                apply_pseudo(&mut c, &name, &arg);
            }
            '.' | '#' => {
                let s = i;
                while i < chars.len() && !matches!(chars[i], '.' | '#' | ':' | '[') {
                    i += 1;
                }
                let name: String = chars[s..i].iter().collect();
                if sep == '.' {
                    c.classes.push(name);
                } else {
                    c.id = Some(name);
                }
            }
            _ => {}
        }
    }

    let any = c.universal
        || c.tag.is_some()
        || c.id.is_some()
        || !c.classes.is_empty()
        || !c.attrs.is_empty()
        || !c.pseudos.is_empty()
        || !c.not.is_empty()
        || c.pseudo_element.is_some();
    any.then_some(c)
}

fn apply_pseudo(c: &mut Compound, name: &str, arg: &str) {
    match name {
        "first-child" => c.pseudos.push(Pseudo::FirstChild),
        "last-child" => c.pseudos.push(Pseudo::LastChild),
        "only-child" => c.pseudos.push(Pseudo::OnlyChild),
        "first-of-type" => c.pseudos.push(Pseudo::FirstOfType),
        "last-of-type" => c.pseudos.push(Pseudo::LastOfType),
        "only-of-type" => c.pseudos.push(Pseudo::OnlyOfType),
        "root" => c.pseudos.push(Pseudo::Root),
        "nth-child" => {
            if let Some((a, b)) = parse_an_plus_b(arg) {
                c.pseudos.push(Pseudo::NthChild(a, b));
            }
        }
        "nth-last-child" => {
            if let Some((a, b)) = parse_an_plus_b(arg) {
                c.pseudos.push(Pseudo::NthLastChild(a, b));
            }
        }
        "nth-of-type" => {
            if let Some((a, b)) = parse_an_plus_b(arg) {
                c.pseudos.push(Pseudo::NthOfType(a, b));
            }
        }
        "nth-last-of-type" => {
            if let Some((a, b)) = parse_an_plus_b(arg) {
                c.pseudos.push(Pseudo::NthLastOfType(a, b));
            }
        }
        "not" => {
            // Simple `:not(compound)` — one inner compound (no combinators).
            if let Some(inner) = parse_compound(arg.trim()) {
                c.not.push(inner);
            }
        }
        // Statically answerable state: every hyperlink is unvisited in a fresh
        // sealed head, so `:link`/`:any-link` = "an <a>/<area> with href", and
        // disabled/checked read straight off the attributes (rest state).
        "link" | "any-link" => c.pseudos.push(Pseudo::Link),
        "disabled" => c.pseudos.push(Pseudo::Disabled),
        "enabled" => c.pseudos.push(Pseudo::Enabled),
        "checked" => c.pseudos.push(Pseudo::Checked),
        // `:is(...)`/`:where(...)` over a selector list of simple compounds.
        // The list is forgiving (CSS Selectors 4): arguments we can't parse or
        // evaluate (combinators) are dropped; an empty result matches nothing.
        "is" | "where" => {
            let args = parse_compound_list(arg);
            c.pseudos.push(match (args.is_empty(), name == "is") {
                (true, _) => Pseudo::Never,
                (false, true) => Pseudo::Is(args),
                (false, false) => Pseudo::Where(args),
            });
        }
        // `:has(...)` with single-compound arguments and an optional leading
        // `>`; both forms check direct children (see `Pseudo::HasChild`).
        "has" => {
            let args = parse_has_args(arg);
            c.pseudos.push(if args.is_empty() {
                Pseudo::Never
            } else {
                Pseudo::HasChild(args)
            });
        }
        // Interaction state has no static answer; force a non-match so we never
        // wrongly apply (e.g.) :hover styles at rest. `:visited` is genuinely
        // empty (fresh profile), matching Chrome with no history.
        "hover" | "focus" | "active" | "visited" | "focus-within" | "focus-visible" => {
            c.pseudos.push(Pseudo::Never)
        }
        // The renderable pseudo-elements (either colon form): mark the
        // compound so the cascade can generate the box; Selector::matches
        // refuses these for the real element.
        "before" => c.pseudo_element = Some(PseudoElement::Before),
        "after" => c.pseudo_element = Some(PseudoElement::After),
        // Any OTHER pseudo — an unrendered pseudo-element (`:marker`,
        // `:placeholder`, ...) or an unrecognized class — must make the selector
        // match NOTHING. Ignoring it instead degraded
        // `.x:before{position:absolute;top:-1px;background:...}` to `.x{...}`,
        // absolutely positioning the real element to the viewport top and
        // painting the pseudo's decoration band across the page (measured on
        // mozilla.org's `.m24-c-transition:before` staircase bands).
        _ => c.pseudos.push(Pseudo::Never),
    }
}

/// Parse a `:is()`/`:where()` argument list: top-level-comma-separated simple
/// compounds. An argument with top-level combinators/whitespace (not
/// evaluable against a single element) or that fails to parse is dropped.
fn parse_compound_list(arg: &str) -> Vec<Compound> {
    crate::split_top_commas(arg)
        .iter()
        .filter_map(|part| {
            let part = part.trim();
            (!has_top_level_structure(part))
                .then(|| parse_compound(part))
                .flatten()
        })
        .collect()
}

/// Parse `:has()` arguments: each a single compound with an optional leading
/// `>` combinator. Descendant/sibling arguments are dropped (under-match).
fn parse_has_args(arg: &str) -> Vec<Compound> {
    crate::split_top_commas(arg)
        .iter()
        .filter_map(|part| {
            let part = part.trim();
            let rest = match part.strip_prefix('>') {
                Some(r) => r.trim_start(),
                None => part,
            };
            (!has_top_level_structure(rest))
                .then(|| parse_compound(rest))
                .flatten()
        })
        .collect()
}

/// Whether `s` has top-level (outside parentheses) whitespace or a combinator —
/// i.e. it is more than one simple compound.
fn has_top_level_structure(s: &str) -> bool {
    let mut depth = 0i32;
    for ch in s.chars() {
        match ch {
            '(' => depth += 1,
            ')' => depth = (depth - 1).max(0),
            c if depth == 0 && (c.is_whitespace() || matches!(c, '>' | '+' | '~')) => {
                return true;
            }
            _ => {}
        }
    }
    false
}

/// Parse the `An+B` microsyntax: `odd`, `even`, `3`, `2n`, `2n+1`, `-n+3`, `n`.
fn parse_an_plus_b(arg: &str) -> Option<(i32, i32)> {
    let s = arg.trim().to_ascii_lowercase();
    match s.as_str() {
        "odd" => return Some((2, 1)),
        "even" => return Some((2, 0)),
        _ => {}
    }
    if let Ok(b) = s.parse::<i32>() {
        return Some((0, b));
    }
    let npos = s.find('n')?;
    let a_part = &s[..npos];
    let a = match a_part {
        "" | "+" => 1,
        "-" => -1,
        _ => a_part.parse().ok()?,
    };
    let b_part = s[npos + 1..].replace(' ', "");
    let b = if b_part.is_empty() {
        0
    } else {
        b_part.parse().ok()?
    };
    Some((a, b))
}

fn parse_attr_sel(body: &str) -> Option<AttrSel> {
    let body = body.trim();
    for (sym, op) in [
        ("~=", AttrOp::Include),
        ("|=", AttrOp::Dash),
        ("^=", AttrOp::Prefix),
        ("$=", AttrOp::Suffix),
        ("*=", AttrOp::Substr),
        ("=", AttrOp::Eq),
    ] {
        if let Some((name, value)) = body.split_once(sym) {
            return Some(AttrSel {
                name: name.trim().to_ascii_lowercase(),
                op,
                value: unquote(value.trim()),
            });
        }
    }
    if body.is_empty() {
        return None;
    }
    Some(AttrSel {
        name: body.to_ascii_lowercase(),
        op: AttrOp::Exists,
        value: String::new(),
    })
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    let bytes = s.as_bytes();
    if bytes.len() >= 2
        && (bytes[0] == b'"' || bytes[0] == b'\'')
        && bytes[bytes.len() - 1] == bytes[0]
    {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

fn strip_comments(css: &str) -> String {
    let mut out = String::with_capacity(css.len());
    let mut rest = css;
    while let Some(start) = rest.find("/*") {
        out.push_str(&rest[..start]);
        match rest[start + 2..].find("*/") {
            Some(end) => rest = &rest[start + 2 + end + 2..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Index of the `}` matching the `{` that precedes `s` (handles nesting).
fn matching_brace(s: &str) -> Option<usize> {
    let mut depth = 0i32;
    for (i, ch) in s.char_indices() {
        match ch {
            '{' => depth += 1,
            '}' if depth == 0 => return Some(i),
            '}' => depth -= 1,
            _ => {}
        }
    }
    None
}

fn skip_at_rule(rest: &str) -> &str {
    let semi = rest.find(';');
    let brace = rest.find('{');
    match (semi, brace) {
        (Some(s), Some(b)) if s < b => &rest[s + 1..],
        (Some(s), None) => &rest[s + 1..],
        (_, Some(b)) => {
            let after = &rest[b + 1..];
            matching_brace(after).map_or("", |e| &after[e + 1..])
        }
        (None, None) => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sref(tag: &str, id: Option<&str>, classes: &[&str], attrs: &[(&str, &str)]) -> SiblingRef {
        SiblingRef {
            tag: tag.to_string(),
            id: id.map(str::to_string),
            classes: classes.iter().map(|s| s.to_string()).collect(),
            attrs: attrs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            children: Rc::from([]),
        }
    }

    fn with_children(mut r: SiblingRef, kids: Vec<SiblingRef>) -> SiblingRef {
        r.children = kids.into();
        r
    }

    /// Build a path where each level is a single only-child (no siblings).
    fn chain(refs: Vec<SiblingRef>) -> Vec<ElemRef> {
        refs.into_iter()
            .map(|r| ElemRef {
                siblings: Rc::from(vec![r]),
                index: 0,
            })
            .collect()
    }

    fn matches(css: &str, path: &[ElemRef]) -> bool {
        parse_stylesheet(css).rules[0].matches(path).is_some()
    }

    #[test]
    fn child_vs_descendant_combinator() {
        // div > p : matches only a direct child.
        let direct = chain(vec![sref("div", None, &[], &[]), sref("p", None, &[], &[])]);
        let nested = chain(vec![
            sref("div", None, &[], &[]),
            sref("span", None, &[], &[]),
            sref("p", None, &[], &[]),
        ]);
        assert!(matches("div > p { x: y }", &direct));
        assert!(
            !matches("div > p { x: y }", &nested),
            "child is not descendant"
        );
        assert!(
            matches("div p { x: y }", &nested),
            "descendant still matches"
        );
    }

    #[test]
    fn sibling_combinators() {
        // Parent with three element children: h2, p, p.
        let sibs: Rc<[SiblingRef]> = Rc::from(vec![
            sref("h2", None, &[], &[]),
            sref("p", None, &["a"], &[]),
            sref("p", None, &["b"], &[]),
        ]);
        let parent = ElemRef {
            siblings: Rc::from(vec![sref("div", None, &[], &[])]),
            index: 0,
        };
        // Path to the SECOND p (.b): div > {h2,p.a,p.b}[2].
        let path = vec![
            parent,
            ElemRef {
                siblings: sibs.clone(),
                index: 2,
            },
        ];
        assert!(matches("h2 ~ p { x: y }", &path), "general sibling");
        assert!(matches(".a + .b { x: y }", &path), "adjacent sibling");
        assert!(
            !matches("h2 + p { x: y }", &path),
            ".b is not adjacent to h2"
        );
    }

    #[test]
    fn attribute_selectors() {
        let path = chain(vec![sref(
            "input",
            None,
            &[],
            &[("type", "text"), ("href", "https://x")],
        )]);
        assert!(matches("[type] { x: y }", &path));
        assert!(matches("input[type=text] { x: y }", &path));
        assert!(matches("[href^=\"https\"] { x: y }", &path));
        assert!(!matches("[type=checkbox] { x: y }", &path));
    }

    #[test]
    fn structural_pseudos_and_not() {
        let sibs: Rc<[SiblingRef]> = Rc::from(vec![
            sref("li", None, &[], &[]),
            sref("li", None, &["x"], &[]),
            sref("li", None, &[], &[]),
        ]);
        let at = |i: usize| {
            vec![ElemRef {
                siblings: sibs.clone(),
                index: i,
            }]
        };
        assert!(matches("li:first-child { a: b }", &at(0)));
        assert!(matches("li:last-child { a: b }", &at(2)));
        assert!(matches("li:nth-child(2) { a: b }", &at(1)));
        assert!(matches("li:nth-child(odd) { a: b }", &at(2)));
        assert!(!matches("li:nth-child(odd) { a: b }", &at(1)));
        assert!(matches("li:not(.x) { a: b }", &at(0)));
        assert!(!matches("li:not(.x) { a: b }", &at(1)));
    }

    #[test]
    fn of_type_pseudos_count_only_same_tag_siblings() {
        // Level: h2, p, p, span, p  → the `*-of-type` pseudos count only the `p`s.
        let sibs: Rc<[SiblingRef]> = Rc::from(vec![
            sref("h2", None, &[], &[]),
            sref("p", None, &[], &[]), // 1st p
            sref("p", None, &[], &[]), // 2nd p
            sref("span", None, &[], &[]),
            sref("p", None, &[], &[]), // 3rd (last) p
        ]);
        let at = |i: usize| {
            vec![ElemRef {
                siblings: sibs.clone(),
                index: i,
            }]
        };
        // The first p is at sibling index 1 but is `:first-of-type` (not :first-child).
        assert!(matches("p:first-of-type { a: b }", &at(1)));
        assert!(!matches("p:first-child { a: b }", &at(1)));
        // The last p is index 4 and `:last-of-type`.
        assert!(matches("p:last-of-type { a: b }", &at(4)));
        // nth-of-type counts among p's: the 2nd p is at index 2.
        assert!(matches("p:nth-of-type(2) { a: b }", &at(2)));
        assert!(!matches("p:nth-of-type(2) { a: b }", &at(4)));
        // The lone span is :only-of-type.
        assert!(matches("span:only-of-type { a: b }", &at(3)));
        assert!(!matches("p:only-of-type { a: b }", &at(1)));
        // Counting from the end: the last p (index 4) is nth-last-of-type(1),
        // and the 2nd p (index 2) is nth-last-of-type(2). Among all siblings the
        // last element (index 4) is nth-last-child(1).
        assert!(matches("p:nth-last-of-type(1) { a: b }", &at(4)));
        assert!(matches("p:nth-last-of-type(2) { a: b }", &at(2)));
        assert!(!matches("p:nth-last-of-type(1) { a: b }", &at(1)));
        assert!(matches("p:nth-last-child(1) { a: b }", &at(4)));
        assert!(!matches("p:nth-last-child(1) { a: b }", &at(2)));
    }

    #[test]
    fn state_pseudos_never_match_statically() {
        let path = chain(vec![sref("a", None, &[], &[("href", "/x")])]);
        assert!(!matches("a:hover { a: b }", &path));
        assert!(!matches("a:visited { a: b }", &path));
        assert!(!matches("a:focus { a: b }", &path));
    }

    #[test]
    fn link_pseudo_matches_hyperlinks_with_href() {
        // Every hyperlink is unvisited in a fresh head, so `a:link` (how a large
        // share of sites style links) must match an <a href> — dropping it left
        // links UA-blue on styled pages (the iana regression).
        let with_href = chain(vec![sref("a", None, &[], &[("href", "/x")])]);
        let no_href = chain(vec![sref("a", None, &[], &[])]);
        assert!(matches("a:link { color: green }", &with_href));
        assert!(matches("a:any-link { color: green }", &with_href));
        assert!(
            !matches("a:link { color: green }", &no_href),
            "a placeholder <a> without href is not a link"
        );
        // The classic pair applies through its :link branch.
        assert!(matches("a:link, a:visited { color: green }", &with_href));
        // A non-anchor with href is not a hyperlink.
        let div = chain(vec![sref("div", None, &[], &[("href", "/x")])]);
        assert!(!matches("div:link { color: green }", &div));
    }

    #[test]
    fn form_state_pseudos_read_attributes() {
        let disabled = chain(vec![sref("button", None, &[], &[("disabled", "")])]);
        let enabled = chain(vec![sref("button", None, &[], &[])]);
        assert!(matches("button:disabled { opacity: 0.5 }", &disabled));
        assert!(!matches("button:disabled { opacity: 0.5 }", &enabled));
        assert!(matches("button:enabled { color: red }", &enabled));
        assert!(!matches("button:enabled { color: red }", &disabled));
        // :enabled applies only to form controls, not arbitrary elements.
        let div = chain(vec![sref("div", None, &[], &[])]);
        assert!(!matches("div:enabled { color: red }", &div));

        let checked = chain(vec![sref("input", None, &[], &[("checked", "")])]);
        let unchecked = chain(vec![sref("input", None, &[], &[])]);
        assert!(matches("input:checked { outline: x }", &checked));
        assert!(!matches("input:checked { outline: x }", &unchecked));
        let selected = chain(vec![sref("option", None, &[], &[("selected", "")])]);
        assert!(matches("option:checked { font-weight: bold }", &selected));
    }

    #[test]
    fn pseudo_element_selectors_never_match_the_real_element() {
        // `.x:before { position:absolute; background:… }` styles a PSEUDO
        // element we don't render. Ignoring the `:before` degraded the selector
        // to `.x{…}` — absolutely positioning the real element to the viewport
        // top and painting its decoration band across the page (measured on
        // mozilla.org's `.m24-c-transition:before`). Both colon forms must
        // match nothing, as must unrecognized pseudo-classes.
        let div = chain(vec![sref("div", None, &["x"], &[])]);
        assert!(!matches(".x:before { background: red }", &div));
        assert!(!matches(".x::before { background: red }", &div));
        assert!(!matches(".x::after { background: red }", &div));
        assert!(!matches(".x:some-future-pseudo { color: red }", &div));
        // The plain selector still matches, and a GROUP still applies through
        // its valid member.
        assert!(matches(".x { background: red }", &div));
        assert!(matches(".y:before, .x { background: red }", &div));
    }

    #[test]
    fn unknown_pseudos_never_match() {
        // A rule guarded by a pseudo we can't evaluate must not apply to the
        // element itself (Chrome drops such rules entirely).
        let path = chain(vec![sref("p", None, &[], &[])]);
        assert!(!matches("p:target { a: b }", &path));
        assert!(!matches("p::first-line { a: b }", &path));
        // ...while `:not(<never-matching>)` stays vacuously true.
        assert!(matches("p:not(:target) { a: b }", &path));
    }

    #[test]
    fn is_and_where_match_any_argument_compound() {
        let at = |classes: &[&str]| chain(vec![sref("p", None, classes, &[])]);
        assert!(matches("p:is(.a, .b) { x: y }", &at(&["b"])));
        assert!(!matches("p:is(.a, .b) { x: y }", &at(&["c"])));
        assert!(matches(":where(.a, p) { x: y }", &at(&[])));
        // The list is forgiving: an argument we can't evaluate (combinators)
        // is dropped while the rest still match…
        assert!(matches("p:is(div span, .b) { x: y }", &at(&["b"])));
        // …and with no usable argument the pseudo never matches.
        assert!(!matches("p:is(div span) { x: y }", &at(&["b"])));
        // A top-level comma inside :is() must not split the selector list.
        let sheet = parse_stylesheet("p:is(.a, .b), h1 { x: y }");
        assert_eq!(sheet.rules[0].selectors.len(), 2);
        assert!(sheet.rules[0]
            .matches(&chain(vec![sref("h1", None, &[], &[])]))
            .is_some());
    }

    #[test]
    fn is_takes_max_argument_specificity_where_takes_none() {
        let spec = |css: &str| parse_stylesheet(css).rules[0].selectors[0].specificity();
        assert_eq!(
            spec("p:is(#a, .b) { x: y }"),
            (1, 0, 1),
            ":is contributes its most specific argument"
        );
        assert_eq!(
            spec("p:where(#a, .b) { x: y }"),
            (0, 0, 1),
            ":where contributes zero"
        );
        assert_eq!(
            spec("div:has(.b) { x: y }"),
            (0, 1, 1),
            ":has contributes its most specific argument"
        );
    }

    #[test]
    fn has_matches_direct_children_only() {
        let a = sref("a", None, &[], &[("href", "#")]);
        let div_direct = with_children(sref("div", None, &[], &[]), vec![a.clone()]);
        let span_with_a = with_children(sref("span", None, &[], &[]), vec![a.clone()]);
        let div_nested = with_children(sref("div", None, &[], &[]), vec![span_with_a]);
        assert!(matches(
            "div:has(> a) { x: y }",
            &chain(vec![div_direct.clone()])
        ));
        assert!(matches(
            "div:has(a[href]) { x: y }",
            &chain(vec![div_direct.clone()])
        ));
        assert!(!matches(
            "div:has(> a) { x: y }",
            &chain(vec![div_nested.clone()])
        ));
        // Documented subset: Chrome's `:has(a)` also matches via a grandchild;
        // `SiblingRef` carries one level of children, so we under-match here
        // (never over-match).
        assert!(!matches("div:has(a) { x: y }", &chain(vec![div_nested])));
        // No matching child, no match; a descendant argument is unsupported
        // and never matches.
        assert!(!matches(
            "div:has(> p) { x: y }",
            &chain(vec![div_direct.clone()])
        ));
        assert!(!matches(
            "div:has(span a) { x: y }",
            &chain(vec![div_direct])
        ));
    }

    #[test]
    fn has_evaluates_structural_pseudos_of_children() {
        let li = |cls: &[&str]| sref("li", None, cls, &[]);
        let ul = with_children(sref("ul", None, &[], &[]), vec![li(&[]), li(&["x"])]);
        // The second child is :nth-child(2) — and `2n + 1` style spaces inside
        // the argument survive the (now paren-aware) selector tokenizer.
        assert!(matches(
            "ul:has(> li:nth-child(2).x) { a: b }",
            &chain(vec![ul.clone()])
        ));
        assert!(matches(
            "ul:has(> li:nth-child(2n + 1)) { a: b }",
            &chain(vec![ul.clone()])
        ));
        assert!(!matches(
            "ul:has(> li:nth-child(3)) { a: b }",
            &chain(vec![ul])
        ));
    }

    #[test]
    fn media_query_gating() {
        let sheet = parse_stylesheet("@media (max-width: 600px) { p { color: red } }");
        let rule = &sheet.rules[0];
        assert!(rule.applies(MediaContext {
            width: 480,
            height: 800
        }));
        assert!(!rule.applies(MediaContext {
            width: 1200,
            height: 800
        }));
    }

    #[test]
    fn media_query_range_syntax() {
        // MQ4 range syntax — iana.org alone has 64 such blocks; treating them
        // as unrecognized dropped every responsive tier, so pages rendered
        // their desktop-widest CSS at every viewport.
        let at = |css: &str, width: u32| {
            parse_stylesheet(css).rules[0].applies(MediaContext { width, height: 700 })
        };
        let css = "@media (width <= 1000px) { p { color: red } }";
        assert!(at(css, 1000), "inclusive bound");
        assert!(!at(css, 1001));
        let css = "@media (width < 1200px) { p { color: red } }";
        assert!(at(css, 1199), "exclusive bound");
        assert!(!at(css, 1200));
        let css = "@media (width >= 800px) { p { color: red } }";
        assert!(at(css, 800));
        assert!(!at(css, 799));
        // Reversed operand order.
        let css = "@media (1000px >= width) { p { color: red } }";
        assert!(at(css, 1000));
        assert!(!at(css, 1001));
        // Double-bounded.
        let css = "@media (400px <= width <= 1000px) { p { color: red } }";
        assert!(at(css, 400));
        assert!(at(css, 1000));
        assert!(!at(css, 399));
        assert!(!at(css, 1001));
        // Range syntax over a feature we can't evaluate: false, not vacuous.
        assert!(!at("@media (aspect-ratio > 1) { p { color: red } }", 1000));
        // Combined with `screen and`.
        let css = "@media screen and (width <= 1000px) { p { color: red } }";
        assert!(at(css, 900));
        assert!(!at(css, 1100));
    }

    #[test]
    fn media_preference_features_track_the_light_desktop_persona() {
        let ctx = MediaContext {
            width: 1000,
            height: 700,
        };
        let applies = |css: &str| parse_stylesheet(css).rules[0].applies(ctx);
        // A dark-scheme block must NOT apply on the light persona. Regression:
        // the unrecognized feature was dropped, leaving an empty branch that
        // vacuously matched, so dark colors overrode the light ones everywhere.
        assert!(!applies(
            "@media (prefers-color-scheme: dark) { p { color: red } }"
        ));
        assert!(!applies(
            "@media only screen and (prefers-color-scheme: dark) { p { color: red } }"
        ));
        // The matching light-scheme block does apply.
        assert!(applies(
            "@media (prefers-color-scheme: light) { p { color: red } }"
        ));
        // Reduced-motion / forced-colors blocks the persona doesn't request.
        assert!(!applies(
            "@media (prefers-reduced-motion: reduce) { p { color: red } }"
        ));
        assert!(!applies(
            "@media (forced-colors: active) { p { color: red } }"
        ));
        // Desktop capabilities the persona has.
        assert!(applies(
            "@media (hover: hover) and (pointer: fine) { p { color: red } }"
        ));
        // A genuinely unknown feature evaluates to false, not a vacuous match.
        assert!(!applies(
            "@media (some-future-feature: 3) { p { color: red } }"
        ));
        // A width branch combined with an unmet dark scheme still fails.
        assert!(!applies(
            "@media (min-width: 100px) and (prefers-color-scheme: dark) { p { color: red } }"
        ));
    }

    #[test]
    fn print_and_non_screen_media_types_do_not_match_the_screen() {
        let ctx = MediaContext {
            width: 1000,
            height: 700,
        };
        let applies = |css: &str| parse_stylesheet(css).rules[0].applies(ctx);
        // Regression: bare media types were skipped, leaving an empty branch that
        // vacuously matched, so `@media print` styles leaked onto the screen
        // render (e.g. Wikipedia's `a{color:#000!important}`).
        assert!(!applies("@media print { a { color: #000 } }"));
        assert!(!applies("@media handheld { a { color: red } }"));
        assert!(!applies(
            "@media print and (min-width: 100px) { a { color: red } }"
        ));
        // `screen` / `all` still match, with or without features.
        assert!(applies("@media screen { a { color: red } }"));
        assert!(applies("@media all { a { color: red } }"));
        assert!(applies(
            "@media only screen and (min-width: 100px) { a { color: red } }"
        ));
        // Negation: `not print` matches a screen; `not screen` does not.
        assert!(applies("@media not print { a { color: red } }"));
        assert!(!applies("@media not screen { a { color: red } }"));
    }

    #[test]
    fn specificity_orders_id_class_type() {
        let sheet = parse_stylesheet("#id .c[a] p:first-child { x: y }");
        // 1 id, (1 class + 1 attr + 1 pseudo) = 3, 1 type.
        assert_eq!(sheet.rules[0].selectors[0].specificity(), (1, 3, 1));
    }
}

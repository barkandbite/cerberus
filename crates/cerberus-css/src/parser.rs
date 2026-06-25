//! CSS parsing + selector matching + specificity. Bootstrapped; no dependencies.
//!
//! Supported selectors: universal `*`, type, `.class`, `#id`, attribute
//! selectors (`[a]`, `[a=v]`, `~= |= ^= $= *=`), structural pseudo-classes
//! (`:first-child`, `:last-child`, `:only-child`, `:nth-child(an+b)`, `:not(…)`,
//! `:root`), grouping `,`, and the descendant / child (`>`) / adjacent-sibling
//! (`+`) / general-sibling (`~`) combinators. State pseudo-classes (`:hover`,
//! `:focus`, `:active`, `:visited`, `:link`) parse but never match in the static
//! cascade. `@media` blocks are parsed and gated on the viewport; other `@`-rules
//! are skipped.

use std::collections::HashMap;
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

#[derive(Clone, Debug)]
enum Pseudo {
    FirstChild,
    LastChild,
    OnlyChild,
    NthChild(i32, i32), // a, b  for an+b (1-based position)
    Root,
    Never, // :hover/:focus/:active/:visited/:link — no static match
}

/// Which renderable pseudo-element a selector targets, if any. We generate boxes
/// only for `::before`/`::after` (their `content` is collected in a separate
/// cascade pass); every other pseudo-element stays unmatchable (a `Pseudo::Never`
/// on its compound). A selector tagged with a pseudo-element is excluded from the
/// normal element cascade so its declarations never leak onto the host element.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PseudoEl {
    #[default]
    None,
    Before,
    After,
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
    /// A `::before`/`::after` marker on this compound. Lifted onto the owning
    /// `Selector` (it is only meaningful on the subject compound); a marker on a
    /// non-subject compound is an invalid selector forced to never match.
    pseudo_el: PseudoEl,
}

impl Compound {
    fn specificity(&self) -> Specificity {
        let mut s = (
            u32::from(self.id.is_some()),
            self.classes.len() as u32 + self.attrs.len() as u32 + self.pseudos.len() as u32,
            u32::from(self.tag.is_some()),
        );
        for n in &self.not {
            let inner = n.specificity();
            s = (s.0 + inner.0, s.1 + inner.1, s.2 + inner.2);
        }
        s
    }

    /// Match this compound against `el` at sibling position `index` of `total`.
    fn matches(&self, el: &SiblingRef, index: usize, total: usize) -> bool {
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
            if !pseudo_matches(p, el, index, total) {
                return false;
            }
        }
        // :not(...) — none of the inner simple compounds may match.
        if self.not.iter().any(|n| n.matches(el, index, total)) {
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

fn pseudo_matches(p: &Pseudo, el: &SiblingRef, index: usize, total: usize) -> bool {
    let pos = index as i32 + 1; // 1-based
    match p {
        Pseudo::FirstChild => index == 0,
        Pseudo::LastChild => index + 1 == total,
        Pseudo::OnlyChild => total == 1,
        Pseudo::NthChild(a, b) => nth_matches(*a, *b, pos),
        Pseudo::Root => el.tag == "html",
        Pseudo::Never => false,
    }
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
    /// The renderable pseudo-element this selector targets (`::before`/`::after`),
    /// if any. When set, the selector's `compounds` match the *host* element, but
    /// the selector is excluded from the normal element cascade and only feeds the
    /// pseudo-content pass ([`Rule::matches_pseudo`]).
    pseudo: PseudoEl,
}

impl Selector {
    fn specificity(&self) -> Specificity {
        let mut s = self.compounds.iter().fold((0, 0, 0), |a, c| {
            let s = c.specificity();
            (a.0 + s.0, a.1 + s.1, a.2 + s.2)
        });
        // A pseudo-element contributes one to the type/element component.
        if self.pseudo != PseudoEl::None {
            s.2 += 1;
        }
        s
    }

    /// Match against an ancestor path (root … element); the element is last.
    fn matches(&self, path: &[ElemRef]) -> bool {
        if self.compounds.is_empty() || path.is_empty() {
            return false;
        }
        let last = path.len() - 1;
        self.match_at(self.compounds.len() - 1, path, last, path[last].index)
    }

    /// The bucket key for this selector's *subject* (rightmost compound): the most
    /// selective of id > first class > tag, else universal. A necessary condition
    /// for the selector to match, so indexing on it never drops a real match
    /// (the full [`matches`](Self::matches) still runs on the candidates).
    fn subject_key(&self) -> SubjectKey {
        match self.compounds.last() {
            Some(c) if c.id.is_some() => SubjectKey::Id(c.id.clone().unwrap()),
            Some(c) if !c.classes.is_empty() => SubjectKey::Class(c.classes[0].clone()),
            Some(c) if c.tag.is_some() => SubjectKey::Tag(c.tag.clone().unwrap()),
            _ => SubjectKey::Universal,
        }
    }

    /// Recursive, backtracking match: compound `ci` against the element at
    /// path level `pi`, sibling index `sib`.
    fn match_at(&self, ci: usize, path: &[ElemRef], pi: usize, sib: usize) -> bool {
        let level = &path[pi];
        let total = level.siblings.len();
        if !self.compounds[ci].matches(&level.siblings[sib], sib, total) {
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
    }
}

/// A rule: a group of selectors, declarations, and the `@media` it lives in.
#[derive(Clone, Debug)]
pub struct Rule {
    pub selectors: Vec<Selector>,
    pub declarations: Vec<(String, String)>,
    pub media: Option<MediaQuery>,
}

impl Rule {
    /// The highest specificity among the rule's *element* selectors matching
    /// `path`, if any. Pseudo-element selectors (`::before`/`::after`) are excluded
    /// so their declarations never apply to the real host element — they are
    /// collected separately by [`matches_pseudo`](Self::matches_pseudo).
    pub fn matches(&self, path: &[ElemRef]) -> Option<Specificity> {
        self.selectors
            .iter()
            .filter(|s| s.pseudo == PseudoEl::None && s.matches(path))
            .map(Selector::specificity)
            .max()
    }

    /// The highest specificity among this rule's `kind` (`::before`/`::after`)
    /// selectors whose host matches `path`, if any — the pseudo-content pass.
    pub fn matches_pseudo(&self, path: &[ElemRef], kind: PseudoEl) -> Option<Specificity> {
        self.selectors
            .iter()
            .filter(|s| s.pseudo == kind && s.matches(path))
            .map(Selector::specificity)
            .max()
    }

    /// Whether any selector targets a renderable pseudo-element, so the cascade
    /// can skip the (otherwise per-element) pseudo-content pass for sheets that
    /// declare no `::before`/`::after`.
    pub fn has_pseudo(&self) -> bool {
        self.selectors.iter().any(|s| s.pseudo != PseudoEl::None)
    }

    /// Whether this rule applies under `ctx` (no `@media`, or it matches).
    pub fn applies(&self, ctx: MediaContext) -> bool {
        self.media.as_ref().is_none_or(|m| m.matches(ctx))
    }
}

/// A parsed stylesheet.
#[derive(Clone, Debug, Default)]
pub struct Stylesheet {
    pub rules: Vec<Rule>,
}

/// The bucket a selector's subject keys into.
enum SubjectKey {
    Id(String),
    Class(String),
    Tag(String),
    Universal,
}

/// A selector-subject index over a stylesheet's rules, so the cascade tests only
/// rules whose subject can match a given element (by id/class/tag) plus the small
/// universal bucket — turning per-element matching from O(all rules) into roughly
/// O(rules keyed to this element). Rebuilt once per stylesheet, not per element.
#[derive(Debug, Default)]
pub struct RuleIndex {
    by_id: HashMap<String, Vec<usize>>,
    by_class: HashMap<String, Vec<usize>>,
    by_tag: HashMap<String, Vec<usize>>,
    /// Rules whose subject is `*` / attribute- or pseudo-only — always candidates.
    universal: Vec<usize>,
}

impl RuleIndex {
    /// Index `sheet`'s rules by each selector's subject key. A rule lands in every
    /// bucket any of its selectors keys into (deduped), so `.a, #b` is found by an
    /// element with class `a` *or* id `b`.
    pub fn build(sheet: &Stylesheet) -> Self {
        let mut idx = RuleIndex::default();
        for (i, rule) in sheet.rules.iter().enumerate() {
            for sel in &rule.selectors {
                let bucket = match sel.subject_key() {
                    SubjectKey::Id(s) => idx.by_id.entry(s).or_default(),
                    SubjectKey::Class(s) => idx.by_class.entry(s).or_default(),
                    SubjectKey::Tag(s) => idx.by_tag.entry(s).or_default(),
                    SubjectKey::Universal => &mut idx.universal,
                };
                if bucket.last() != Some(&i) {
                    bucket.push(i); // selectors of one rule are visited in order
                }
            }
        }
        idx
    }

    /// Candidate rule indices for `el`, in ascending source order (so the caller
    /// keeps the cascade's source-order tiebreak). Deduped across buckets.
    pub fn candidates(&self, el: &SiblingRef) -> Vec<usize> {
        let mut out: Vec<usize> = Vec::new();
        if let Some(id) = &el.id {
            if let Some(v) = self.by_id.get(id) {
                out.extend_from_slice(v);
            }
        }
        for c in &el.classes {
            if let Some(v) = self.by_class.get(c) {
                out.extend_from_slice(v);
            }
        }
        if let Some(v) = self.by_tag.get(&el.tag) {
            out.extend_from_slice(v);
        }
        out.extend_from_slice(&self.universal);
        out.sort_unstable();
        out.dedup();
        out
    }
}

/// Parse a full stylesheet.
pub fn parse_stylesheet(css: &str) -> Stylesheet {
    let css = strip_comments(css);
    let mut rules = Vec::new();
    parse_rules_into(&css, None, &mut rules);
    Stylesheet { rules }
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
            // `@supports`: we don't evaluate the feature condition (we can't probe
            // support the way a full engine does); apply the inner rules, which
            // are overwhelmingly safe progressive enhancements, preserving source
            // order so a later fallback still wins where it should.
            if let Some(after_at) = rest.strip_prefix("@supports") {
                if let Some(brace) = after_at.find('{') {
                    let inner = &after_at[brace + 1..];
                    if let Some(end) = matching_brace(inner) {
                        parse_rules_into(&inner[..end], media, out);
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
pub fn parse_declaration_block(text: &str) -> Vec<(String, String)> {
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
            if let Some(pos) = low.rfind("!important") {
                value.truncate(pos);
                value = value.trim().to_string();
            }
            if !prop.is_empty() {
                decls.push((prop, value));
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
            // Split on `and`; ignore a leading media type (screen/all/print) and
            // `only`/`not` qualifiers (best-effort).
            for part in branch.split(" and ") {
                let part = part.trim().trim_start_matches("only ").trim();
                if let Some(inner) = part.strip_prefix('(').and_then(|p| p.strip_suffix(')')) {
                    if let Some(f) = parse_media_feature(inner.trim()) {
                        feats.push(f);
                    }
                }
            }
            feats
        })
        .collect();
    MediaQuery { branches }
}

fn parse_media_feature(text: &str) -> Option<MediaFeature> {
    let (name, value) = match text.split_once(':') {
        Some((n, v)) => (n.trim().to_ascii_lowercase(), v.trim().to_ascii_lowercase()),
        None => (text.trim().to_ascii_lowercase(), String::new()),
    };
    match name.as_str() {
        "min-width" => Some(MediaFeature::MinWidth(eval_media_px(&value)?)),
        "max-width" => Some(MediaFeature::MaxWidth(eval_media_px(&value)?)),
        "min-height" => Some(MediaFeature::MinHeight(eval_media_px(&value)?)),
        "max-height" => Some(MediaFeature::MaxHeight(eval_media_px(&value)?)),
        "orientation" => match value.as_str() {
            "portrait" => Some(MediaFeature::Portrait),
            "landscape" => Some(MediaFeature::Landscape),
            _ => None,
        },
        _ => None,
    }
}

/// A media-query `<length>` → px: `Npx`, `Nem`/`Nrem` (×16), a bare number, or a
/// `calc(A ± B)` of those (CSS requires spaces around the `+`/`-`). Sites pin
/// breakpoints with `max-width: calc(640px - 1px)`; without `calc`, the feature
/// failed to parse and the whole query vacuously matched (ADR-0054).
fn eval_media_px(value: &str) -> Option<u32> {
    let v = value.trim();
    if let Some(inner) = v.strip_prefix("calc(").and_then(|s| s.strip_suffix(')')) {
        let inner = inner.trim();
        for (op, subtract) in [(" - ", true), (" + ", false)] {
            if let Some(i) = inner.find(op) {
                let a = media_len_px(&inner[..i])?;
                let b = media_len_px(&inner[i + op.len()..])?;
                let r = if subtract { a - b } else { a + b };
                return Some(r.max(0.0).round() as u32);
            }
        }
        return media_len_px(inner).map(|r| r.max(0.0).round() as u32);
    }
    media_len_px(v).map(|r| r.max(0.0).round() as u32)
}

fn media_len_px(t: &str) -> Option<f64> {
    let t = t.trim();
    // `rem` before `em` (the latter is a suffix of the former).
    for (suffix, mul) in [("px", 1.0), ("rem", 16.0), ("em", 16.0)] {
        if let Some(n) = t.strip_suffix(suffix) {
            return n.trim().parse::<f64>().ok().map(|v| v * mul);
        }
    }
    t.parse::<f64>().ok()
}

fn parse_selectors(text: &str) -> Vec<Selector> {
    text.split(',')
        .filter_map(|s| {
            let sel = parse_selector(s.trim());
            (!sel.compounds.is_empty()).then_some(sel)
        })
        .collect()
}

fn parse_selector(text: &str) -> Selector {
    let mut compounds = Vec::new();
    let mut pending = Combinator::Descendant;
    for token in text.split_whitespace() {
        match token {
            ">" => pending = Combinator::Child,
            "+" => pending = Combinator::Adjacent,
            "~" => pending = Combinator::General,
            _ => {
                if let Some(mut c) = parse_compound(token) {
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
    // A pseudo-element is only meaningful on the subject (rightmost) compound;
    // lift it onto the selector. A marker on any earlier compound (`a::before b`)
    // is invalid — force that compound to never match so it can't leak.
    let pseudo = compounds.last().map(|c| c.pseudo_el).unwrap_or_default();
    let n = compounds.len();
    for (i, c) in compounds.iter_mut().enumerate() {
        if i + 1 < n && c.pseudo_el != PseudoEl::None {
            c.pseudos.push(Pseudo::Never);
            c.pseudo_el = PseudoEl::None;
        }
    }
    Selector { compounds, pseudo }
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
                // A second ':' marks a pseudo-*element* (`::before`).
                let double_colon = i < chars.len() && chars[i] == ':';
                if double_colon {
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
                // `::before`/`::after` are the pseudo-elements we render: keep the
                // host compound matchable and tag the side, so a separate pass can
                // attach their generated `content` (the selector is still excluded
                // from the normal element cascade — see `Selector::pseudo`).
                // Every *other* pseudo-element targets a generated box we don't
                // create, so it must never match a real element — otherwise its
                // declarations leak onto that element (e.g. `p::first-line{...}`).
                // `::x` and the legacy `:before/:after/:first-line/:first-letter`
                // are pseudo-elements; other `:x` are pseudo-classes.
                if name == "before" || name == "after" {
                    c.pseudo_el = if name == "before" {
                        PseudoEl::Before
                    } else {
                        PseudoEl::After
                    };
                } else if double_colon || is_pseudo_element(&name) {
                    c.pseudos.push(Pseudo::Never);
                } else {
                    apply_pseudo(&mut c, &name, &arg);
                }
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
        || !c.not.is_empty();
    any.then_some(c)
}

fn apply_pseudo(c: &mut Compound, name: &str, arg: &str) {
    match name {
        "first-child" => c.pseudos.push(Pseudo::FirstChild),
        "last-child" => c.pseudos.push(Pseudo::LastChild),
        "only-child" => c.pseudos.push(Pseudo::OnlyChild),
        "root" => c.pseudos.push(Pseudo::Root),
        "nth-child" => {
            if let Some((a, b)) = parse_an_plus_b(arg) {
                c.pseudos.push(Pseudo::NthChild(a, b));
            }
        }
        "not" => {
            // Simple `:not(compound)` — one inner compound (no combinators).
            if let Some(inner) = parse_compound(arg.trim()) {
                c.not.push(inner);
            }
        }
        // State pseudo-classes have no static answer; force a non-match so we
        // never wrongly apply (e.g.) :hover styles at rest.
        "hover" | "focus" | "active" | "visited" | "link" | "focus-within" | "focus-visible"
        | "checked" | "disabled" | "enabled" => c.pseudos.push(Pseudo::Never),
        _ => {} // unknown pseudo — ignore (matches nothing extra)
    }
}

/// Whether a (single-colon) pseudo name denotes a pseudo-*element* rather than a
/// pseudo-class. `::`-prefixed names are always pseudo-elements; these are the
/// legacy single-colon forms plus the common `::`-only ones, so a selector ending
/// in one never matches a real element (we don't box pseudo-elements).
fn is_pseudo_element(name: &str) -> bool {
    matches!(
        name,
        "before"
            | "after"
            | "first-line"
            | "first-letter"
            | "placeholder"
            | "marker"
            | "selection"
            | "backdrop"
            | "file-selector-button"
    )
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
        }
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
    fn pseudo_elements_do_not_match_the_element() {
        let p = chain(vec![sref("p", None, &[], &[])]);
        // Pseudo-elements target a generated box we don't create, so they must not
        // match the real element — otherwise their declarations leak onto it (the
        // `p::before { width: 120pt }` that was sizing every paragraph).
        assert!(!matches("p::before { width: 120pt }", &p));
        assert!(!matches("p::after { x: y }", &p));
        assert!(
            !matches("p:before { x: y }", &p),
            "legacy single-colon ::before"
        );
        assert!(!matches("p::first-line { x: y }", &p));
        assert!(!matches("p:first-letter { x: y }", &p));
        assert!(!matches("p::marker { x: y }", &p));
        assert!(!matches("p::selection { x: y }", &p));
        // Pseudo-*classes* still match the element.
        assert!(matches("p:first-child { x: y }", &p));
        assert!(matches("p { x: y }", &p));
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
    fn state_pseudos_never_match_statically() {
        let path = chain(vec![sref("a", None, &[], &[])]);
        assert!(!matches("a:hover { a: b }", &path));
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
    fn specificity_orders_id_class_type() {
        let sheet = parse_stylesheet("#id .c[a] p:first-child { x: y }");
        // 1 id, (1 class + 1 attr + 1 pseudo) = 3, 1 type.
        assert_eq!(sheet.rules[0].selectors[0].specificity(), (1, 3, 1));
    }
}

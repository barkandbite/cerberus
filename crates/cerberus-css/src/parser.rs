//! CSS parsing + selector matching + specificity. Bootstrapped; no dependencies.
//!
//! Supported selectors: universal `*`, type, `.class`, `#id`, attribute
//! selectors (`[a]`, `[a=v]`, `~= |= ^= $= *=`), structural pseudo-classes
//! (`:first-child`, `:last-child`, `:only-child`, `:nth-child(an+b)`,
//! `:nth-last-child(an+b)`, the `*-of-type` family (`:first-of-type`,
//! `:last-of-type`, `:only-of-type`, `:nth-of-type(an+b)`,
//! `:nth-last-of-type(an+b)`), `:not(…)`,
//! `:root`), grouping `,`, and the descendant / child (`>`) / adjacent-sibling
//! (`+`) / general-sibling (`~`) combinators. State pseudo-classes (`:hover`,
//! `:focus`, `:active`, `:visited`, `:link`) parse but never match in the static
//! cascade. `@media` blocks are parsed and gated on the viewport; other `@`-rules
//! are skipped.

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
    Never, // :hover/:focus/:active/:visited/:link — no static match
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
}

impl Selector {
    fn specificity(&self) -> Specificity {
        self.compounds.iter().fold((0, 0, 0), |a, c| {
            let s = c.specificity();
            (a.0 + s.0, a.1 + s.1, a.2 + s.2)
        })
    }

    /// Match against an ancestor path (root … element); the element is last.
    fn matches(&self, path: &[ElemRef]) -> bool {
        if self.compounds.is_empty() || path.is_empty() {
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
}

/// A parsed stylesheet.
#[derive(Clone, Debug, Default)]
pub struct Stylesheet {
    pub rules: Vec<Rule>,
}

/// Parse a full stylesheet.
pub fn parse_stylesheet(css: &str) -> Stylesheet {
    let css = strip_comments(css);
    let mut rules = Vec::new();
    parse_rules_into(&css, None, &mut rules);
    Stylesheet { rules }
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
        _ => None,
    }
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
        || !c.not.is_empty();
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
        // State pseudo-classes have no static answer; force a non-match so we
        // never wrongly apply (e.g.) :hover styles at rest.
        "hover" | "focus" | "active" | "visited" | "link" | "focus-within" | "focus-visible"
        | "checked" | "disabled" | "enabled" => c.pseudos.push(Pseudo::Never),
        _ => {} // unknown pseudo — ignore (matches nothing extra)
    }
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

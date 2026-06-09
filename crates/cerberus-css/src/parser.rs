//! CSS parsing: a stylesheet of rules (selector groups + declarations), plus
//! selector matching and specificity. Bootstrapped; no dependencies.
//!
//! Supported selectors: universal `*`, type, `.class`, `#id`, grouping `,`, and
//! the descendant combinator (whitespace). Child/sibling combinators are parsed
//! but treated as descendant; pseudo-classes and attribute selectors are
//! tolerated and ignored. `@`-rules (e.g. `@media`, `@keyframes`) are skipped.

/// `(ids, classes, types)` specificity, compared as a tuple.
pub type Specificity = (u32, u32, u32);

/// A DOM element reduced to what selectors match against.
pub struct ElemRef {
    pub tag: String,
    pub id: Option<String>,
    pub classes: Vec<String>,
}

#[derive(Clone, Debug, Default)]
struct Compound {
    universal: bool,
    tag: Option<String>,
    id: Option<String>,
    classes: Vec<String>,
}

impl Compound {
    fn specificity(&self) -> Specificity {
        (
            u32::from(self.id.is_some()),
            self.classes.len() as u32,
            u32::from(self.tag.is_some()),
        )
    }

    fn matches(&self, el: &ElemRef) -> bool {
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
        self.classes.iter().all(|c| el.classes.contains(c))
    }
}

/// A descendant chain of compound selectors (left to right).
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
        let mut ci = self.compounds.len();
        let mut pi = path.len();

        // The rightmost compound must match the element itself.
        if !self.compounds[ci - 1].matches(&path[pi - 1]) {
            return false;
        }
        ci -= 1;
        pi -= 1;

        // Each remaining compound must match some ancestor, in order.
        while ci > 0 {
            ci -= 1;
            let comp = &self.compounds[ci];
            let mut found = false;
            while pi > 0 {
                pi -= 1;
                if comp.matches(&path[pi]) {
                    found = true;
                    break;
                }
            }
            if !found {
                return false;
            }
        }
        true
    }
}

/// A rule: a group of selectors and a block of declarations.
#[derive(Clone, Debug)]
pub struct Rule {
    pub selectors: Vec<Selector>,
    pub declarations: Vec<(String, String)>,
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
    let mut rest = css.as_str();

    loop {
        rest = rest.trim_start();
        if rest.is_empty() {
            break;
        }
        if rest.starts_with('@') {
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
            rules.push(Rule {
                selectors,
                declarations,
            });
        }
    }
    Stylesheet { rules }
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
    for token in text.split_whitespace() {
        if token == ">" || token == "+" || token == "~" {
            continue; // combinators treated as descendant
        }
        if let Some(c) = parse_compound(token) {
            compounds.push(c);
        }
    }
    Selector { compounds }
}

fn parse_compound(token: &str) -> Option<Compound> {
    let b = token.as_bytes();
    let mut c = Compound::default();
    let mut i = 0;

    // Leading type / universal.
    let start = i;
    while i < b.len() && !matches!(b[i], b'.' | b'#' | b':' | b'[') {
        i += 1;
    }
    let head = &token[start..i];
    if head == "*" {
        c.universal = true;
    } else if !head.is_empty() {
        c.tag = Some(head.to_ascii_lowercase());
    }

    while i < b.len() {
        let sep = b[i];
        i += 1;
        if sep == b'[' {
            while i < b.len() && b[i] != b']' {
                i += 1;
            }
            i += usize::from(i < b.len()); // consume ']'
            continue;
        }
        let s = i;
        while i < b.len() && !matches!(b[i], b'.' | b'#' | b':' | b'[') {
            i += 1;
        }
        let name = &token[s..i];
        match sep {
            b'.' => c.classes.push(name.to_string()),
            b'#' => c.id = Some(name.to_string()),
            _ => {} // ':' pseudo-class/element — ignored
        }
    }

    let any = c.universal || c.tag.is_some() || c.id.is_some() || !c.classes.is_empty();
    any.then_some(c)
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

    fn el(tag: &str, id: Option<&str>, classes: &[&str]) -> ElemRef {
        ElemRef {
            tag: tag.to_string(),
            id: id.map(str::to_string),
            classes: classes.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn parses_rules_and_skips_at_rules() {
        let sheet = parse_stylesheet(
            "/* c */ @media x { p { color: red } } a, .b { color: blue; font-size: 12px }",
        );
        assert_eq!(sheet.rules.len(), 1, "the @media block is skipped");
        assert_eq!(sheet.rules[0].declarations.len(), 2);
    }

    #[test]
    fn descendant_and_specificity() {
        let sheet = parse_stylesheet("nav a.x { color: red }");
        let rule = &sheet.rules[0];
        let path = vec![
            el("nav", None, &[]),
            el("span", None, &[]),
            el("a", None, &["x"]),
        ];
        assert_eq!(rule.matches(&path), Some((0, 1, 2)));
        let no = vec![el("a", None, &["x"])];
        assert_eq!(rule.matches(&no), None, "needs a nav ancestor");
    }

    #[test]
    fn id_and_class_selectors() {
        let sheet = parse_stylesheet("#main .item { color: red }");
        let rule = &sheet.rules[0];
        let path = vec![el("div", Some("main"), &[]), el("li", None, &["item"])];
        assert_eq!(rule.matches(&path), Some((1, 1, 0)));
    }
}

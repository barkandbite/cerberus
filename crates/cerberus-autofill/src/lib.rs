//! Per-identity autofill (ADR-0022).
//!
//! A [`Profile`] bundles the three things an identity fills: a login, an
//! address, and a card. [`fill_plan`] detects a page's form fields by heuristics
//! (input `type`, the `autocomplete` token, and `name`/`id`/`placeholder`
//! patterns) and returns the `(NodeId, value)` pairs to set — keyed to the live
//! DOM so the app/mirror layer can apply them with `set_node_value`, **filling
//! only** (the submit is a normal user/mirrored click, never automated).
//!
//! This crate is pure data + detection; persistence (the encrypted vault) and
//! the `Action::Fill` wiring live in the app and mirror layers.

use cerberus_dom::{Document, NodeId, NodeRef};

/// Login credentials.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Login {
    pub username: String,
    pub password: String,
}

/// A postal/contact address.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Address {
    pub full_name: String,
    pub line1: String,
    pub line2: String,
    pub city: String,
    pub region: String,
    pub postal: String,
    pub country: String,
    pub phone: String,
    pub email: String,
}

/// A payment card. CVV is stored per the owner's explicit choice (vault-sealed
/// at rest in the app layer).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Card {
    pub holder: String,
    pub number: String,
    pub exp_month: String,
    pub exp_year: String,
    pub cvv: String,
}

/// One identity's autofill data: login + address + card.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Profile {
    pub login: Login,
    pub address: Address,
    pub card: Card,
}

/// Which kind of value a detected form field wants.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldKind {
    Username,
    Password,
    Email,
    FullName,
    AddressLine1,
    AddressLine2,
    City,
    Region,
    Postal,
    Country,
    Phone,
    CardNumber,
    CardExp,
    CardExpMonth,
    CardExpYear,
    CardCvv,
    CardHolder,
}

/// What a profile fills, restricted to a category — so a "fill login" gesture
/// only touches credential fields, "fill address" only address fields, etc.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FillKind {
    Login,
    Address,
    Payment,
    All,
}

impl FieldKind {
    fn category(self) -> FillKind {
        match self {
            FieldKind::Username | FieldKind::Password => FillKind::Login,
            FieldKind::CardNumber
            | FieldKind::CardExp
            | FieldKind::CardExpMonth
            | FieldKind::CardExpYear
            | FieldKind::CardCvv
            | FieldKind::CardHolder => FillKind::Payment,
            _ => FillKind::Address,
        }
    }
}

/// The fill value for a detected field, or `None` to skip it.
pub fn value_for(kind: FieldKind, profile: &Profile) -> Option<String> {
    let a = &profile.address;
    let c = &profile.card;
    let v = match kind {
        FieldKind::Username => &profile.login.username,
        FieldKind::Password => &profile.login.password,
        FieldKind::Email => &a.email,
        FieldKind::FullName => &a.full_name,
        FieldKind::AddressLine1 => &a.line1,
        FieldKind::AddressLine2 => &a.line2,
        FieldKind::City => &a.city,
        FieldKind::Region => &a.region,
        FieldKind::Postal => &a.postal,
        FieldKind::Country => &a.country,
        FieldKind::Phone => &a.phone,
        FieldKind::CardNumber => &c.number,
        FieldKind::CardExpMonth => &c.exp_month,
        FieldKind::CardExpYear => &c.exp_year,
        FieldKind::CardCvv => &c.cvv,
        FieldKind::CardHolder => &c.holder,
        FieldKind::CardExp => {
            return (!c.exp_month.is_empty() && !c.exp_year.is_empty())
                .then(|| format!("{}/{}", c.exp_month, two_digit_year(&c.exp_year)));
        }
    };
    (!v.is_empty()).then(|| v.clone())
}

fn two_digit_year(y: &str) -> String {
    if y.len() == 4 {
        y[2..].to_string()
    } else {
        y.to_string()
    }
}

/// Classify a single fillable control by its attributes. `None` = not a field
/// we autofill (e.g. a submit button or an unrecognized input).
pub fn classify(field: NodeRef<'_>) -> Option<FieldKind> {
    let tag = field.tag();
    if tag != "input" && tag != "textarea" {
        return None;
    }
    let typ = field.attr("type").unwrap_or("text").to_ascii_lowercase();
    if matches!(
        typ.as_str(),
        "hidden" | "submit" | "button" | "reset" | "checkbox" | "radio" | "file" | "image"
    ) {
        return None;
    }
    let ac = field
        .attr("autocomplete")
        .unwrap_or("")
        .to_ascii_lowercase();
    let hints = format!(
        "{} {} {}",
        field.attr("name").unwrap_or(""),
        field.attr("id").unwrap_or(""),
        field.attr("placeholder").unwrap_or("")
    )
    .to_ascii_lowercase();

    // Strongest signals first: input type, then the autocomplete token.
    if typ == "password" {
        return Some(FieldKind::Password);
    }
    if typ == "email" {
        return Some(FieldKind::Email);
    }
    if typ == "tel" {
        return Some(FieldKind::Phone);
    }
    if let Some(kind) = from_autocomplete(&ac) {
        return Some(kind);
    }
    from_hints(&hints)
}

fn from_autocomplete(ac: &str) -> Option<FieldKind> {
    // The token may be space-prefixed (e.g. "shipping postal-code"); match the
    // last word.
    let token = ac.split_whitespace().last().unwrap_or("");
    Some(match token {
        "username" => FieldKind::Username,
        "current-password" | "new-password" | "password" => FieldKind::Password,
        "email" => FieldKind::Email,
        "name" => FieldKind::FullName,
        "tel" => FieldKind::Phone,
        "street-address" | "address-line1" => FieldKind::AddressLine1,
        "address-line2" => FieldKind::AddressLine2,
        "address-level2" => FieldKind::City,
        "address-level1" => FieldKind::Region,
        "postal-code" => FieldKind::Postal,
        "country" | "country-name" => FieldKind::Country,
        "cc-number" => FieldKind::CardNumber,
        "cc-exp" => FieldKind::CardExp,
        "cc-exp-month" => FieldKind::CardExpMonth,
        "cc-exp-year" => FieldKind::CardExpYear,
        "cc-csc" => FieldKind::CardCvv,
        "cc-name" | "cc-given-name" | "cc-family-name" => FieldKind::CardHolder,
        _ => return None,
    })
}

fn from_hints(h: &str) -> Option<FieldKind> {
    let has = |needle: &str| h.contains(needle);
    if has("password") || has("passwd") {
        Some(FieldKind::Password)
    } else if has("email") || has("e-mail") {
        Some(FieldKind::Email)
    } else if has("user") || has("login") {
        Some(FieldKind::Username)
    } else if (has("card") && has("number")) || has("cardnum") || has("ccnum") {
        Some(FieldKind::CardNumber)
    } else if has("cvv") || has("cvc") || has("csc") || has("security code") {
        Some(FieldKind::CardCvv)
    } else if has("exp") {
        Some(FieldKind::CardExp)
    } else if has("zip") || has("postal") {
        Some(FieldKind::Postal)
    } else if has("city") || has("town") {
        Some(FieldKind::City)
    } else if has("state") || has("province") || has("region") {
        Some(FieldKind::Region)
    } else if has("country") {
        Some(FieldKind::Country)
    } else if has("phone") || has("mobile") || has("tel") {
        Some(FieldKind::Phone)
    } else if has("address") || has("street") {
        Some(FieldKind::AddressLine1)
    } else if has("name") {
        Some(FieldKind::FullName)
    } else {
        None
    }
}

/// Detect the fillable fields in `doc` and return the `(NodeId, value)` pairs to
/// set for `kind` from `profile`. Empty values and out-of-category fields are
/// skipped; submit is never included (fill-only).
pub fn fill_plan(doc: &Document, profile: &Profile, kind: FillKind) -> Vec<(NodeId, String)> {
    let mut plan = Vec::new();
    collect(doc.root(), profile, kind, &mut plan);
    plan
}

fn collect(node: NodeRef<'_>, profile: &Profile, kind: FillKind, out: &mut Vec<(NodeId, String)>) {
    if let Some(fk) = classify(node) {
        let in_scope = kind == FillKind::All || fk.category() == kind;
        if in_scope {
            if let Some(value) = value_for(fk, profile) {
                out.push((node.id(), value));
            }
        }
    }
    for child in node.children() {
        collect(child, profile, kind, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cerberus_dom::parse_html;

    fn profile() -> Profile {
        Profile {
            login: Login {
                username: "ada".into(),
                password: "s3cret".into(),
            },
            address: Address {
                full_name: "Ada Lovelace".into(),
                line1: "1 Analytical Way".into(),
                city: "London".into(),
                postal: "EC1".into(),
                email: "ada@x.test".into(),
                ..Address::default()
            },
            card: Card {
                holder: "ADA LOVELACE".into(),
                number: "4111111111111111".into(),
                exp_month: "04".into(),
                exp_year: "2030".into(),
                cvv: "123".into(),
            },
        }
    }

    fn value_at(plan: &[(NodeId, String)], doc: &Document, id: &str) -> Option<String> {
        fn find(n: NodeRef<'_>, id: &str) -> Option<NodeId> {
            if n.attr("id") == Some(id) {
                return Some(n.id());
            }
            n.children().find_map(|c| find(c, id))
        }
        let nid = find(doc.root(), id)?;
        plan.iter().find(|(n, _)| *n == nid).map(|(_, v)| v.clone())
    }

    #[test]
    fn detects_by_type_autocomplete_and_hints() {
        let html = "<form>\
            <input id=\"u\" name=\"user\">\
            <input id=\"p\" type=\"password\">\
            <input id=\"e\" type=\"email\">\
            <input id=\"cc\" autocomplete=\"cc-number\">\
            <input id=\"zip\" placeholder=\"ZIP code\">\
            <input id=\"go\" type=\"submit\">\
            </form>";
        let doc = parse_html(html);
        let plan = fill_plan(&doc, &profile(), FillKind::All);
        assert_eq!(value_at(&plan, &doc, "u").as_deref(), Some("ada"));
        assert_eq!(value_at(&plan, &doc, "p").as_deref(), Some("s3cret"));
        assert_eq!(value_at(&plan, &doc, "e").as_deref(), Some("ada@x.test"));
        assert_eq!(
            value_at(&plan, &doc, "cc").as_deref(),
            Some("4111111111111111")
        );
        assert_eq!(value_at(&plan, &doc, "zip").as_deref(), Some("EC1"));
        // The submit button is never filled.
        assert_eq!(value_at(&plan, &doc, "go"), None);
    }

    #[test]
    fn fill_kind_scopes_the_plan() {
        let html = "<input id=\"u\" name=\"username\"><input id=\"cc\" autocomplete=\"cc-number\">";
        let doc = parse_html(html);
        let login = fill_plan(&doc, &profile(), FillKind::Login);
        assert!(value_at(&login, &doc, "u").is_some());
        assert!(
            value_at(&login, &doc, "cc").is_none(),
            "login fill skips the card"
        );
        let pay = fill_plan(&doc, &profile(), FillKind::Payment);
        assert!(value_at(&pay, &doc, "u").is_none());
        assert!(value_at(&pay, &doc, "cc").is_some());
    }

    #[test]
    fn card_exp_is_composed_and_empty_values_skip() {
        let html = "<input id=\"x\" autocomplete=\"cc-exp\"><input id=\"l2\" autocomplete=\"address-line2\">";
        let doc = parse_html(html);
        let plan = fill_plan(&doc, &profile(), FillKind::All);
        assert_eq!(value_at(&plan, &doc, "x").as_deref(), Some("04/30"));
        // address line2 is empty in the profile -> not in the plan.
        assert_eq!(value_at(&plan, &doc, "l2"), None);
    }
}

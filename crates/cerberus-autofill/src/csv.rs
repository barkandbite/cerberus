//! A small, dependency-free CSV codec for a *table* of autofill [`Profile`]s
//! (ADR-0030).
//!
//! Each row is `(identity-label, Profile)`. The point is bulk setup: fill a
//! template once and import many identities at a stroke, instead of running
//! `profile --set` per person. Columns are mapped by **header name**, so column
//! order is free and unknown columns are ignored; the only required column is
//! `identity`.
//!
//! The delimiter is configurable (and auto-detected on import) because the data
//! — addresses especially — is full of commas; the owner prefers `:`. Fields are
//! quoted RFC-4180-style (wrap in `"`, double interior `"`) so any delimiter is
//! lossless regardless of the data.

use crate::{Address, Card, Login, Profile};

/// The CSV columns, in order: the identity label, then the 16 profile fields in
/// [`Profile::to_bytes`] order. The field names match the `profile --set` keys.
pub const CSV_HEADERS: [&str; 17] = [
    "identity",
    "login.username",
    "login.password",
    "address.full_name",
    "address.line1",
    "address.line2",
    "address.city",
    "address.region",
    "address.postal",
    "address.country",
    "address.phone",
    "address.email",
    "card.holder",
    "card.number",
    "card.exp_month",
    "card.exp_year",
    "card.cvv",
];

/// Delimiters auto-detection considers, best-first. `:` is the owner's default.
const CANDIDATE_DELIMS: [char; 5] = [':', ',', ';', '\t', '|'];

/// The 17 cells of a row, in [`CSV_HEADERS`] order.
fn row_cells(label: &str, p: &Profile) -> [String; 17] {
    [
        label.to_string(),
        p.login.username.clone(),
        p.login.password.clone(),
        p.address.full_name.clone(),
        p.address.line1.clone(),
        p.address.line2.clone(),
        p.address.city.clone(),
        p.address.region.clone(),
        p.address.postal.clone(),
        p.address.country.clone(),
        p.address.phone.clone(),
        p.address.email.clone(),
        p.card.holder.clone(),
        p.card.number.clone(),
        p.card.exp_month.clone(),
        p.card.exp_year.clone(),
        p.card.cvv.clone(),
    ]
}

/// Serialize `(label, profile)` rows to CSV text using `delim`.
pub fn profiles_to_csv(rows: &[(String, Profile)], delim: char) -> String {
    let mut out = String::new();
    write_row(&mut out, CSV_HEADERS.iter().map(|h| h.to_string()), delim);
    for (label, p) in rows {
        write_row(&mut out, row_cells(label, p), delim);
    }
    out
}

/// A no-frills template: the header row plus two illustrative example rows to
/// show exactly what goes where. Replace the examples with real identities.
pub fn csv_template(delim: char) -> String {
    let example = |label: &str, user: &str, name: &str| {
        (
            label.to_string(),
            Profile {
                login: Login {
                    username: user.to_string(),
                    password: "change-me".to_string(),
                },
                address: Address {
                    full_name: name.to_string(),
                    line1: "123 Example St".to_string(),
                    city: "Springfield".to_string(),
                    region: "IL".to_string(),
                    postal: "62704".to_string(),
                    country: "US".to_string(),
                    phone: "555-0100".to_string(),
                    email: user.to_string(),
                    ..Address::default()
                },
                card: Card::default(),
            },
        )
    };
    profiles_to_csv(
        &[
            example("work", "a.worker@example.com", "Alex Worker"),
            example("personal", "alex@example.com", "Alex Person"),
        ],
        delim,
    )
}

/// Parse CSV `text` into `(label, profile)` rows. The delimiter is auto-detected
/// from the header. Columns are matched by name (order-free); unknown columns are
/// ignored and absent ones default empty. The `identity` column is required and
/// each row's identity must be non-empty.
pub fn profiles_from_csv(text: &str) -> Result<Vec<(String, Profile)>, String> {
    let delim = detect_delimiter(text).ok_or_else(|| {
        "could not detect the delimiter or recognize the header row (expected an \
         'identity' column)"
            .to_string()
    })?;
    let mut records = parse_records(text, delim).into_iter();
    let header = records
        .next()
        .ok_or_else(|| "the file is empty".to_string())?;

    // header name (lowercased) -> column index.
    let index_of = |name: &str| {
        header
            .iter()
            .position(|h| h.trim().eq_ignore_ascii_case(name))
    };
    let id_col = index_of("identity")
        .ok_or_else(|| "missing required 'identity' column in the header".to_string())?;
    // Profile field columns (skip the "identity" header at position 0).
    let field_cols: Vec<Option<usize>> = CSV_HEADERS[1..].iter().map(|h| index_of(h)).collect();

    let mut out = Vec::new();
    for (i, rec) in records.enumerate() {
        if rec.iter().all(|c| c.trim().is_empty()) {
            continue; // blank line
        }
        let cell = |col: Option<usize>| col.and_then(|c| rec.get(c)).cloned().unwrap_or_default();
        let label = cell(Some(id_col)).trim().to_string();
        if label.is_empty() {
            return Err(format!("row {} has an empty 'identity'", i + 2));
        }
        let mut f = field_cols.iter().map(|c| cell(*c));
        let mut next = || f.next().unwrap_or_default();
        out.push((
            label,
            Profile {
                login: Login {
                    username: next(),
                    password: next(),
                },
                address: Address {
                    full_name: next(),
                    line1: next(),
                    line2: next(),
                    city: next(),
                    region: next(),
                    postal: next(),
                    country: next(),
                    phone: next(),
                    email: next(),
                },
                card: Card {
                    holder: next(),
                    number: next(),
                    exp_month: next(),
                    exp_year: next(),
                    cvv: next(),
                },
            },
        ));
    }
    Ok(out)
}

/// Pick the delimiter whose header row best matches [`CSV_HEADERS`] (most
/// recognized names, requiring at least `identity` + one more), best-first.
fn detect_delimiter(text: &str) -> Option<char> {
    let first_line = text.lines().find(|l| !l.trim().is_empty())?;
    let mut best: Option<(usize, char)> = None;
    for &d in &CANDIDATE_DELIMS {
        let fields = parse_line(first_line, d);
        let known = fields
            .iter()
            .filter(|f| CSV_HEADERS.iter().any(|h| f.trim().eq_ignore_ascii_case(h)))
            .count();
        let has_identity = fields
            .iter()
            .any(|f| f.trim().eq_ignore_ascii_case("identity"));
        if has_identity && known >= 2 && best.is_none_or(|(b, _)| known > b) {
            best = Some((known, d));
        }
    }
    best.map(|(_, d)| d)
}

/// Quote a field if it contains the delimiter, a quote, or a newline.
fn write_field(out: &mut String, s: &str, delim: char) {
    if s.contains(delim) || s.contains('"') || s.contains('\n') || s.contains('\r') {
        out.push('"');
        out.push_str(&s.replace('"', "\"\""));
        out.push('"');
    } else {
        out.push_str(s);
    }
}

fn write_row(out: &mut String, cells: impl IntoIterator<Item = String>, delim: char) {
    let mut first = true;
    for cell in cells {
        if !first {
            out.push(delim);
        }
        write_field(out, &cell, delim);
        first = false;
    }
    out.push('\n');
}

/// Split one physical line on `delim`, honoring double-quoted fields (used for
/// delimiter detection on the header, which never spans lines).
fn parse_line(line: &str, delim: char) -> Vec<String> {
    parse_records(line, delim)
        .into_iter()
        .next()
        .unwrap_or_default()
}

/// Parse CSV text into records (rows of fields), honoring RFC-4180 quoting:
/// `"`-wrapped fields may contain the delimiter, newlines, and `""` escapes.
fn parse_records(text: &str, delim: char) -> Vec<Vec<String>> {
    let mut records = Vec::new();
    let mut record = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut pending = false; // a field/record is in progress
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        pending = true;
        if in_quotes {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    field.push('"');
                    chars.next();
                } else {
                    in_quotes = false;
                }
            } else {
                field.push(c);
            }
        } else if c == '"' {
            in_quotes = true;
        } else if c == delim {
            record.push(std::mem::take(&mut field));
        } else if c == '\n' {
            record.push(std::mem::take(&mut field));
            records.push(std::mem::take(&mut record));
            pending = false;
        } else if c != '\r' {
            field.push(c);
        }
    }
    if pending {
        record.push(field);
        records.push(record);
    }
    records
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<(String, Profile)> {
        vec![
            (
                "work".to_string(),
                Profile {
                    login: Login {
                        username: "ada".into(),
                        password: "p:w,d\"x".into(), // delimiter + comma + quote
                    },
                    address: Address {
                        full_name: "Ada Lovelace".into(),
                        line1: "1 Analytical Way, Apt 2".into(), // comma
                        city: "London".into(),
                        ..Address::default()
                    },
                    card: Card::default(),
                },
            ),
            (
                "personal".to_string(),
                Profile {
                    login: Login {
                        username: "ada@home".into(),
                        password: "hunter2".into(),
                    },
                    ..Profile::default()
                },
            ),
        ]
    }

    #[test]
    fn round_trips_through_colon_csv() {
        let rows = sample();
        let csv = profiles_to_csv(&rows, ':');
        let back = profiles_from_csv(&csv).expect("parse");
        assert_eq!(rows, back);
    }

    #[test]
    fn autodetects_delimiter_and_quoting_is_lossless() {
        for delim in [':', ',', ';', '\t', '|'] {
            let rows = sample();
            let csv = profiles_to_csv(&rows, delim);
            let back = profiles_from_csv(&csv).unwrap_or_else(|e| panic!("delim {delim:?}: {e}"));
            assert_eq!(rows, back, "delim {delim:?} did not round-trip");
        }
    }

    #[test]
    fn columns_are_matched_by_name_not_position() {
        // Reordered header, extra unknown column, missing card columns.
        let text = "address.city:identity:note:login.username\n\
                    London:work:ignore me:ada\n";
        let rows = profiles_from_csv(text).expect("parse");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "work");
        assert_eq!(rows[0].1.login.username, "ada");
        assert_eq!(rows[0].1.address.city, "London");
        assert_eq!(rows[0].1.card.number, ""); // absent column -> empty
    }

    #[test]
    fn template_parses_to_two_example_rows() {
        let rows = profiles_from_csv(&csv_template(':')).expect("template parses");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, "work");
        assert_eq!(rows[1].0, "personal");
    }

    #[test]
    fn blank_lines_are_skipped_and_missing_identity_errors() {
        let ok = "identity:login.username\nwork:ada\n\n  \npersonal:bob\n";
        assert_eq!(profiles_from_csv(ok).unwrap().len(), 2);

        let no_id = "login.username:login.password\nada:pw\n";
        assert!(profiles_from_csv(no_id).is_err());

        let empty_id = "identity:login.username\n:ada\n";
        assert!(profiles_from_csv(empty_id).is_err());
    }
}

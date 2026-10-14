//! Keeps the configuration reference in step with the code: every key `mammoth.toml` accepts is
//! documented in `docs/user/configuration.md` and set in the annotated example, and every key the
//! reference calls available is one the code accepts.
//!
//! The keys come from the parser itself, not a hand-kept list: every table rejects unknown keys,
//! and serde's error for an unknown key lists the ones it expects.

// Helpers panic on unexpected values, like the tests that call them.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;

use mammoth_cache::config::Config;

const REFERENCE: &str = include_str!("../../../docs/user/configuration.md");
const EXAMPLE: &str = include_str!("../../../docs/user/mammoth.example.toml");

/// Stands for any name in a table of named entries, such as `auth.users.<name>`.
const NAME: &str = "<name>";

/// A key no table accepts.
const PROBE: &str = "zz_probe";

/// The error `text` gets from the parser.
fn parse_error(text: &str) -> String {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mammoth.toml");
    std::fs::write(&path, text).unwrap();
    match Config::load(&path) {
        Ok(_) => panic!("{text:?} parsed"),
        Err(e) => e.to_string(),
    }
}

/// The keys in backticks in serde's "expected `a`", "expected `a` or `b`" or "expected one of
/// `a`, `b`, `c`".
fn expected_keys(error: &str) -> Option<Vec<String>> {
    let rest = error
        .split_once(&format!("unknown field `{PROBE}`, expected "))?
        .1;
    let list = rest.lines().next().unwrap();
    Some(
        list.split('`')
            .skip(1)
            .step_by(2)
            .map(str::to_owned)
            .collect(),
    )
}

/// Every key the parser accepts, as dotted paths, found by putting an unknown key in each table.
fn accepted_keys() -> BTreeSet<String> {
    let mut keys = BTreeSet::new();
    walk(&[], &mut keys);
    keys
}

/// A TOML table header for `path`, quoting each part so `<name>` is a valid key.
fn header(path: &[String]) -> String {
    let parts: Vec<String> = path.iter().map(|p| format!("{p:?}")).collect();
    format!("[{}]\n", parts.join("."))
}

fn walk(table: &[String], keys: &mut BTreeSet<String>) {
    let start = if table.is_empty() {
        String::new()
    } else {
        header(table)
    };
    let error = parse_error(&format!("{start}{PROBE} = 1\n"));
    let fields = expected_keys(&error)
        .unwrap_or_else(|| panic!("[{}] is not a table: {error}", table.join(".")));
    for field in fields {
        let mut path = table.to_vec();
        path.push(field);
        let error = parse_error(&format!("{}{PROBE} = 1\n", header(&path)));
        if expected_keys(&error).is_some() {
            walk(&path, keys);
        } else if error.contains("expected struct") {
            // A table of named entries: the probe was taken as an entry's name.
            path.push(NAME.to_owned());
            walk(&path, keys);
        } else {
            keys.insert(path.join("."));
        }
    }
}

/// A key the reference documents, with its status.
#[derive(Debug)]
struct Documented {
    key: String,
    status: String,
}

/// The rows of the reference's key tables. Each table sits under a heading naming its section,
/// such as ``### `[server]` `` or `### Top level`, and has the key in its first column.
fn documented_keys() -> Vec<Documented> {
    let mut section: Option<String> = None;
    let mut status_column = None;
    let mut keys = Vec::new();
    for line in REFERENCE.lines() {
        if line.starts_with("## ") {
            section = None;
            continue;
        }
        if let Some(heading) = line.strip_prefix("### ") {
            section = if heading == "Top level" {
                Some(String::new())
            } else {
                heading
                    .strip_prefix("`[")
                    .and_then(|h| h.split_once("]`"))
                    .map(|(name, _)| format!("{name}."))
            };
            status_column = None;
            continue;
        }
        let Some(prefix) = &section else { continue };
        if !line.starts_with('|') {
            continue;
        }
        let cells: Vec<&str> = line.trim_matches('|').split('|').map(str::trim).collect();
        if cells[0] == "Key" {
            status_column = cells.iter().position(|c| *c == "Status");
            continue;
        }
        let Some(key) = cells[0].strip_prefix('`').and_then(|k| k.strip_suffix('`')) else {
            continue;
        };
        let column = status_column.unwrap_or_else(|| panic!("the table for {key} has no Status"));
        keys.push(Documented {
            key: format!("{prefix}{key}"),
            status: cells[column].to_owned(),
        });
    }
    keys
}

/// Every key set in the annotated example, as dotted paths, with named entries as [`NAME`].
fn example_keys() -> BTreeSet<String> {
    fn collect(table: &toml::Table, path: &str, named: bool, keys: &mut BTreeSet<String>) {
        for (key, value) in table {
            let key = if named { NAME } else { key.as_str() };
            let full = if path.is_empty() {
                key.to_owned()
            } else {
                format!("{path}.{key}")
            };
            match value {
                // `auth.users` is the only table of named entries.
                toml::Value::Table(t) => collect(t, &full, full == "auth.users", keys),
                _ => {
                    keys.insert(full);
                }
            }
        }
    }
    let mut keys = BTreeSet::new();
    collect(&EXAMPLE.parse().unwrap(), "", false, &mut keys);
    keys
}

#[test]
fn the_parser_is_introspectable() {
    let keys = accepted_keys();
    for key in [
        "version",
        "server.idle_timeout",
        "auth.users.<name>.token_hashes",
    ] {
        assert!(keys.contains(key), "{key} missing from {keys:?}");
    }
}

#[test]
fn every_key_is_documented() {
    let documented: BTreeSet<String> = documented_keys().into_iter().map(|d| d.key).collect();
    let missing: Vec<_> = accepted_keys().difference(&documented).cloned().collect();
    assert!(
        missing.is_empty(),
        "docs/user/configuration.md doesn't document {missing:?}"
    );
}

#[test]
fn documented_keys_match_the_code() {
    let accepted = accepted_keys();
    for Documented { key, status } in documented_keys() {
        match status.as_str() {
            "Available" | "Not enforced" => assert!(
                accepted.contains(&key),
                "{key} is documented as {status} but mammoth.toml doesn't accept it"
            ),
            "Planned" => assert!(
                !accepted.contains(&key),
                "{key} is documented as Planned but mammoth.toml accepts it; mark it Available"
            ),
            _ => panic!("{key} has unknown status {status:?}"),
        }
    }
}

#[test]
fn the_example_sets_every_key_and_is_valid() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mammoth.toml");
    std::fs::write(&path, EXAMPLE).unwrap();
    Config::load(&path).unwrap();

    assert_eq!(
        example_keys(),
        accepted_keys(),
        "docs/user/mammoth.example.toml should set every key once"
    );
}

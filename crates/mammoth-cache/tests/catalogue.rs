//! The error catalogue (`api::error::Reason`) matches its documentation: the table in design doc
//! 0017, the reason enum in `api/openapi/common.yaml` and the table in the user guide.

// Helpers panic on a malformed doc, like the tests that call them.
#![allow(clippy::expect_used)]

use std::collections::BTreeSet;

use mammoth_cache::{
    api::error::Reason,
    http::error::{aip_status, http_status},
};

const DESIGN: &str = include_str!("../../../docs/design/0017-errors.md");
const COMMON: &str = include_str!("../../../api/openapi/common.yaml");
const USER: &str = include_str!("../../../docs/user/errors.md");

/// The rows of the catalogue table: reason, code, and the HTTP column as written.
fn table() -> Vec<(String, String, String)> {
    let section = DESIGN
        .split("### Catalogue")
        .nth(1)
        .and_then(|rest| rest.split("\n## ").next())
        .expect("a Catalogue section");
    rows(section)
}

/// The rows of the user guide's table, in the same shape.
fn user_table() -> Vec<(String, String, String)> {
    let section = USER.split("## Reasons").nth(1).expect("a Reasons section");
    rows(section)
}

/// Table rows whose first cell is a reason in backticks: reason, code, and the third column.
fn rows(section: &str) -> Vec<(String, String, String)> {
    section
        .lines()
        .filter_map(|line| {
            let cells: Vec<&str> = line.split('|').map(str::trim).collect();
            // Rows that start a reason; continuation rows have an empty first cell.
            let name = cells
                .get(1)?
                .trim_matches('*')
                .strip_prefix('`')?
                .strip_suffix('`')?;
            if name.starts_with(char::is_lowercase) {
                return None;
            }
            let code = cells.get(2)?.split('`').nth(1)?;
            Some((
                name.to_owned(),
                code.to_owned(),
                (*cells.get(3)?).to_owned(),
            ))
        })
        .collect()
}

/// The values of `ErrorInfo.reason`'s `enum` in `common.yaml`.
fn openapi_enum() -> BTreeSet<String> {
    let reason = COMMON
        .split("    ErrorInfo:")
        .nth(1)
        .and_then(|rest| rest.split("        reason:").nth(1))
        .expect("ErrorInfo.reason");
    let list = reason
        .split("          enum:\n")
        .nth(1)
        .expect("an enum on ErrorInfo.reason");
    list.lines()
        .map_while(|line| line.trim().strip_prefix("- "))
        .map(str::to_owned)
        .collect()
}

#[test]
fn every_reason_is_documented_once() {
    let names: BTreeSet<String> = Reason::ALL.iter().map(|r| r.as_str().to_owned()).collect();
    assert_eq!(names.len(), Reason::ALL.len(), "two reasons share a string");

    let rows = table();
    let documented: BTreeSet<String> = rows.iter().map(|(name, _, _)| name.clone()).collect();
    assert_eq!(
        rows.len(),
        documented.len(),
        "a reason has two rows in 0017"
    );
    assert_eq!(
        documented, names,
        "the 0017 catalogue table and Reason differ"
    );
    assert_eq!(openapi_enum(), names, "common.yaml and Reason differ");
}

#[test]
fn the_user_guide_lists_every_reason_once() {
    let names: BTreeSet<String> = Reason::ALL.iter().map(|r| r.as_str().to_owned()).collect();
    let rows = user_table();
    let documented: BTreeSet<String> = rows.iter().map(|(name, _, _)| name.clone()).collect();
    assert_eq!(
        rows.len(),
        documented.len(),
        "a reason has two rows in errors.md"
    );
    assert_eq!(documented, names, "docs/user/errors.md and Reason differ");
}

#[test]
fn the_tables_give_each_reason_its_code_and_status() {
    // The user guide leaves HTTP blank for a reason only gRPC sends.
    let user = user_table()
        .into_iter()
        .filter(|(_, _, http)| !http.is_empty());
    for (name, code, http) in table().into_iter().chain(user) {
        let reason = *Reason::ALL
            .iter()
            .find(|r| r.as_str() == name)
            .unwrap_or_else(|| panic!("{name} is not a Reason"));
        assert_eq!(reason.code().as_str(), code, "{name}");
        let sent = http_status(reason);
        if let Some(status) = http.strip_suffix('*') {
            // A protocol override: the doc says which status, unless it is kept as sent.
            if status != "4xx" {
                assert_eq!(sent.as_str(), status, "{name}");
                assert_ne!(sent, aip_status(reason.code()), "{name} is not an override");
            }
        } else {
            assert_eq!(sent.as_str(), http, "{name}");
            assert_eq!(sent, aip_status(reason.code()), "{name} is an override");
        }
    }
}

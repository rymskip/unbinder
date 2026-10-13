//! References from one document into another.

use serde_json::{Value, json};
use unbinder::{Documents, Error, Options};

const ROOT: &str = "https://example.com/schemas/root.json";

/// What dereferencing `url` gives once `entries` are loaded: the output and
/// the references left in place.
fn dereferenced(
    entries: &[(&str, Value)],
    url: &str,
    options: &Options,
) -> Result<(Value, Vec<String>), Error> {
    let mut documents = Documents::new();
    for (name, document) in entries {
        documents.insert(name, document.clone())?;
    }
    let shared = documents.dereference(url, options)?;
    Ok((shared.to_value(), shared.circular().to_vec()))
}

fn output(entries: &[(&str, Value)]) -> Result<Value, Error> {
    dereferenced(entries, ROOT, &Options::default()).map(|(schema, _)| schema)
}

#[test]
fn inlines_a_reference_into_another_document() {
    assert_eq!(
        output(&[
            (
                ROOT,
                json!({ "name": { "$ref": "customer.json#/definitions/name" } })
            ),
            (
                "https://example.com/schemas/customer.json",
                json!({ "definitions": { "name": { "type": "string" } } })
            ),
        ]),
        Ok(json!({ "name": { "type": "string" } }))
    );
}

#[test]
fn inlines_a_whole_document() {
    assert_eq!(
        output(&[
            (ROOT, json!({ "money": { "$ref": "money.json" } })),
            (
                "https://example.com/schemas/money.json",
                json!({ "type": "number" })
            ),
        ]),
        Ok(json!({ "money": { "type": "number" } }))
    );
}

#[test]
fn references_inside_a_document_are_relative_to_it() {
    assert_eq!(
        output(&[
            (
                ROOT,
                json!({ "thing": { "$ref": "../shared/things.json#/thing" } })
            ),
            (
                "https://example.com/shared/things.json",
                json!({
                    "thing": {
                        "inner": { "$ref": "#/leaf" },
                        "other": { "$ref": "more/other.json" }
                    },
                    "leaf": { "type": "string" }
                })
            ),
            (
                "https://example.com/shared/more/other.json",
                json!({ "type": "boolean" })
            ),
        ]),
        Ok(json!({
            "thing": { "inner": { "type": "string" }, "other": { "type": "boolean" } }
        }))
    );
}

#[test]
fn two_spellings_of_a_url_name_one_document() {
    assert_eq!(
        output(&[
            (
                ROOT,
                json!({
                    "first": { "$ref": "./a/../money.json" },
                    "second": { "$ref": "HTTPS://EXAMPLE.com/schemas/money.json#" }
                })
            ),
            (
                "https://example.com/schemas/money.json",
                json!({ "type": "number" })
            ),
        ]),
        Ok(json!({ "first": { "type": "number" }, "second": { "type": "number" } }))
    );
}

#[test]
fn a_pointer_passes_through_a_reference_into_another_document() {
    assert_eq!(
        output(&[
            (
                ROOT,
                json!({
                    "alias": { "$ref": "things.json#/thing" },
                    "inner": { "$ref": "#/alias/inner" }
                })
            ),
            (
                "https://example.com/schemas/things.json",
                json!({ "thing": { "inner": { "type": "number" } } })
            ),
        ])
        .map(|schema| schema.get("inner").cloned()),
        Ok(Some(json!({ "type": "number" })))
    );
}

/// A reference left in place stays as it was written in its own document.
#[test]
fn a_cycle_between_documents_is_expanded_one_level() {
    assert_eq!(
        dereferenced(
            &[
                (
                    ROOT,
                    json!({ "node": { "next": { "$ref": "other.json#/node" } } })
                ),
                (
                    "https://example.com/schemas/other.json",
                    json!({ "node": { "next": { "$ref": "root.json#/node" } } })
                ),
            ],
            ROOT,
            &Options::default()
        ),
        Ok((
            json!({ "node": { "next": { "next": { "$ref": "root.json#/node" } } } }),
            vec!["root.json#/node".to_owned()]
        ))
    );
}

#[test]
fn says_which_documents_are_still_to_load() {
    let mut documents = Documents::new();
    let inserted = documents.insert(
        ROOT,
        json!({
            "a": { "$ref": "a.json#/x" },
            "b": { "$ref": "nested/b.json" },
            "again": { "$ref": "a.json#/y" },
            "own": { "$ref": "#/a" }
        }),
    );

    assert_eq!(inserted, Ok(()));
    assert_eq!(
        documents.missing(),
        [
            "https://example.com/schemas/a.json",
            "https://example.com/schemas/nested/b.json"
        ]
    );
}

#[test]
fn reports_a_document_that_is_not_loaded() {
    assert_eq!(
        output(&[(ROOT, json!({ "name": { "$ref": "customer.json#/name" } }))]),
        Err(Error::MissingDocument {
            url: "https://example.com/schemas/customer.json".to_owned()
        })
    );
}

#[test]
fn reports_a_url_that_cannot_name_a_document() {
    assert_eq!(
        output(&[("schemas/root.json", json!({}))]),
        Err(Error::InvalidUrl {
            url: "schemas/root.json".to_owned()
        })
    );
    assert_eq!(
        output(&[(
            ROOT,
            json!({ "bad": { "$ref": "http://[not a host/x.json" } })
        )]),
        Err(Error::InvalidUrl {
            url: "http://[not a host/x.json".to_owned()
        })
    );
}

#[test]
fn limits_how_many_documents_the_references_lead_through() {
    let entries: Vec<(String, Value)> = (0..6)
        .zip(1..7)
        .map(|(index, next)| {
            (
                format!("https://example.com/chain/{index}.json"),
                json!({ "next": { "$ref": format!("{next}.json") } }),
            )
        })
        .chain([("https://example.com/chain/6.json".to_owned(), json!({}))])
        .collect();
    let entries: Vec<(&str, Value)> = entries
        .iter()
        .map(|(url, document)| (url.as_str(), document.clone()))
        .collect();
    let options = Options {
        max_documents: 3,
        ..Options::default()
    };

    assert_eq!(
        dereferenced(&entries, "https://example.com/chain/0.json", &options),
        Err(Error::DocumentLimitExceeded { limit: 3 })
    );
    assert_eq!(
        dereferenced(
            &entries,
            "https://example.com/chain/0.json",
            &Options::default()
        )
        .map(|(_, circular)| circular),
        Ok(Vec::new())
    );
}

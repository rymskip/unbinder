//! Bundling documents into one whose references all point within it.

use serde_json::{Value, json};
use unbinder::{
    Documents, Error, Hooks, InternalPolicy, Options, Place, Step, bundle_schema,
    bundle_schema_with_hooks,
};

const ROOT: &str = "https://example.com/schemas/root.json";
const SHARED: &str = "https://example.com/schemas/shared.json";

/// The root and shared documents, loaded.
fn documents(root: Value, shared: Value) -> Result<Documents, Error> {
    let mut documents = Documents::new();
    documents.insert(ROOT, root)?;
    documents.insert(SHARED, shared)?;
    Ok(documents)
}

fn bundled(root: Value, shared: Value) -> Result<Value, Error> {
    documents(root, shared)?.bundle(ROOT, &Options::default())
}

/// The value at `path` in a bundle.
fn at(bundle: &Result<Value, Error>, path: &str) -> Option<Value> {
    match bundle {
        Ok(bundle) => bundle.pointer(path).cloned(),
        Err(_) => None,
    }
}

#[test]
fn writes_a_value_of_another_document_once_and_points_the_rest_at_it() {
    assert_eq!(
        bundled(
            json!({
                "a": { "$ref": "shared.json#/definitions" },
                "b": { "$ref": "shared.json#/definitions/name" },
                "c": { "$ref": "shared.json#/definitions" }
            }),
            json!({ "definitions": { "name": { "type": "string" } } })
        ),
        Ok(json!({
            "a": { "name": { "type": "string" } },
            "b": { "$ref": "#/a/name" },
            "c": { "$ref": "#/a" }
        }))
    );
}

#[test]
fn prefers_a_place_under_definitions() {
    assert_eq!(
        bundled(
            json!({
                "properties": { "name": { "$ref": "shared.json#/definitions/name" } },
                "definitions": { "name": { "$ref": "shared.json#/definitions/name" } }
            }),
            json!({ "definitions": { "name": { "type": "string" } } })
        ),
        Ok(json!({
            "properties": { "name": { "$ref": "#/definitions/name" } },
            "definitions": { "name": { "type": "string" } }
        }))
    );
}

#[test]
fn points_a_reference_back_into_a_value_already_written() {
    assert_eq!(
        bundled(
            json!({ "node": { "$ref": "shared.json" } }),
            json!({ "properties": { "next": { "$ref": "#" } } })
        ),
        Ok(json!({ "node": { "properties": { "next": { "$ref": "#/node" } } } }))
    );
}

#[test]
fn merges_keywords_next_to_a_reference_into_its_target() {
    assert_eq!(
        bundled(
            json!({ "name": { "$ref": "shared.json#/name", "description": "Who" } }),
            json!({ "name": { "type": "string", "description": "A name" } })
        ),
        Ok(json!({ "name": { "description": "Who", "type": "string" } }))
    );
}

#[test]
fn points_a_reference_within_the_document_straight_at_its_target() {
    let schema = json!({
        "name": { "$ref": "#/alias" },
        "alias": { "$ref": "#/definitions/name" },
        "deep": { "$ref": "#/holder/inner" },
        "holder": { "$ref": "#/definitions/holder" },
        "definitions": {
            "name": { "type": "string" },
            "holder": { "inner": { "type": "integer" } }
        }
    });

    let bundle = bundle_schema(&schema, &Options::default());

    assert_eq!(
        at(&bundle, "/name"),
        Some(json!({ "$ref": "#/definitions/name" }))
    );
    assert_eq!(
        at(&bundle, "/alias"),
        Some(json!({ "$ref": "#/definitions/name" }))
    );
    assert_eq!(
        at(&bundle, "/deep"),
        Some(json!({ "$ref": "#/definitions/holder/inner" }))
    );
    assert_eq!(
        at(&bundle, "/holder"),
        Some(json!({ "$ref": "#/definitions/holder" }))
    );
}

#[test]
fn leaves_references_within_the_document_as_written_on_request() {
    let schema = json!({
        "name": { "$ref": "#/alias" },
        "alias": { "$ref": "#/definitions/name" },
        "definitions": { "name": { "type": "string" } }
    });
    let options = Options {
        internal: InternalPolicy::Written,
        ..Options::default()
    };

    assert_eq!(bundle_schema(&schema, &options), Ok(schema));
}

#[test]
fn a_reference_to_another_document_needs_the_document() {
    let schema = json!({ "name": { "$ref": "shared.json#/name" } });

    assert_eq!(
        bundle_schema(&schema, &Options::default()),
        Err(Error::ExternalReference {
            reference: "shared.json#/name".to_owned()
        })
    );
}

/// Hooks that leave `examples` alone and note every member with a `$ref`.
#[derive(Default)]
struct Noting {
    looked_through: Vec<String>,
}

impl Hooks for Noting {
    fn excluded(&mut self, place: &Place<'_, '_>) -> Result<bool, Error> {
        Ok(place.path().last() == Some(&Step::Name("examples")))
    }

    fn bundled(&mut self, place: &Place<'_, '_>) {
        self.looked_through.push(place.source().to_string());
    }
}

#[test]
fn tells_the_hooks_what_it_looked_through_and_leaves_out_what_they_exclude() {
    let schema = json!({
        "name": { "$ref": "#/alias" },
        "alias": { "$ref": "#/definitions/name" },
        "examples": [{ "$ref": "#/alias" }],
        "definitions": { "name": { "type": "string" } }
    });
    let mut hooks = Noting::default();

    let bundle = bundle_schema_with_hooks(&schema, &Options::default(), &mut hooks);

    assert_eq!(at(&bundle, "/examples"), schema.get("examples").cloned());
    assert_eq!(hooks.looked_through, ["#/name", "#/alias"]);
}

#[test]
fn looks_values_up_across_the_documents() {
    let loaded = documents(
        json!({ "customer": { "$ref": "shared.json" } }),
        json!({ "properties": { "name": { "type": "string" } } }),
    );
    let lookup = |reference: &str| {
        loaded
            .as_ref()
            .map_err(Clone::clone)
            .and_then(|documents| documents.lookup(reference, &Options::default()))
    };

    assert_eq!(loaded.as_ref().map(Documents::urls), Ok(vec![ROOT, SHARED]));
    assert_eq!(
        lookup(&format!("{ROOT}#/customer/properties/name")),
        Ok(json!({ "type": "string" }))
    );
    assert_eq!(
        lookup(&format!("{ROOT}#/customer")),
        Ok(json!({ "properties": { "name": { "type": "string" } } }))
    );
    assert!(matches!(
        lookup(&format!("{ROOT}#/nowhere")),
        Err(Error::UnresolvedReference { .. })
    ));
}

#[test]
fn sets_a_value_and_adds_what_is_missing_on_the_way() {
    let mut documents = Documents::new();
    let written = [
        documents.insert(ROOT, json!({ "list": [1] })),
        documents.set(&format!("{ROOT}#/a/b"), json!({ "$ref": "shared.json" })),
        documents.set(&format!("{ROOT}#/list/-"), json!(2)),
        documents.set(&format!("{ROOT}#/list/0"), json!(0)),
    ];

    assert_eq!(written, [Ok(()), Ok(()), Ok(()), Ok(())]);
    assert_eq!(
        documents.document(ROOT),
        Some(&json!({ "list": [0, 2], "a": { "b": { "$ref": "shared.json" } } }))
    );
    assert_eq!(documents.missing(), [SHARED]);
    assert!(matches!(
        documents.set(&format!("{ROOT}#/list/0/x"), json!(1)),
        Err(Error::UnresolvedReference { .. })
    ));
    assert!(matches!(
        documents.set(&format!("{ROOT}#/list/7"), json!(1)),
        Err(Error::UnresolvedReference { .. })
    ));
    assert_eq!(
        documents.set("https://example.com/none.json#/a", json!(1)),
        Err(Error::MissingDocument {
            url: "https://example.com/none.json".to_owned()
        })
    );
}

#[test]
fn a_bundle_cannot_take_more_than_the_size_limit() {
    let options = Options {
        max_bytes: 2_000,
        ..Options::default()
    };
    let large = json!({ "padding": "x".repeat(1_000) });
    let loaded = documents(
        json!({
            "a": { "$ref": "shared.json#/one" },
            "b": { "$ref": "shared.json#/two" },
            "c": { "$ref": "shared.json#/three" }
        }),
        json!({ "one": large.clone(), "two": large.clone(), "three": large }),
    );

    assert_eq!(
        loaded.and_then(|documents| documents.bundle(ROOT, &options)),
        Err(Error::SizeLimitExceeded { limit: 2_000 })
    );
}

//! Resources and names that ids set up inside a document.

use serde_json::{Value, json};
use unbinder::{Documents, Error, Options, dereference_schema};

/// What dereferencing `schema` leaves at its member `name`.
fn member(schema: &Value, name: &str) -> Result<Option<Value>, Error> {
    dereference_schema(schema).map(|dereferenced| dereferenced.schema.get(name).cloned())
}

#[test]
fn an_id_on_the_root_names_the_document() {
    let schema = json!({
        "$id": "https://example.com/schemas/root.json",
        "leaf": { "type": "string" },
        "absolute": { "$ref": "https://example.com/schemas/root.json#/leaf" },
        "relative": { "$ref": "root.json#/leaf" }
    });

    assert_eq!(
        member(&schema, "absolute"),
        Ok(Some(json!({ "type": "string" })))
    );
    assert_eq!(
        member(&schema, "relative"),
        Ok(Some(json!({ "type": "string" })))
    );
}

#[test]
fn an_id_makes_a_part_of_a_document_a_resource() {
    let schema = json!({
        "$id": "https://example.com/schemas/root.json",
        "$defs": {
            "address": {
                "$id": "address.json",
                "properties": { "city": { "$ref": "#/$defs/text" } },
                "$defs": { "text": { "type": "string" } }
            },
            "text": { "type": "number" }
        },
        "home": { "$ref": "address.json" },
        "city": { "$ref": "https://example.com/schemas/address.json#/properties/city" }
    });

    // A pointer inside the resource starts at the resource, so the city is
    // the resource's own text and not the document's.
    assert_eq!(
        member(&schema, "city"),
        Ok(Some(json!({ "type": "string" })))
    );
    assert_eq!(
        member(&schema, "home")
            .map(|home| { home.and_then(|home| home.pointer("/properties/city").cloned()) }),
        Ok(Some(json!({ "type": "string" })))
    );
}

#[test]
fn an_id_can_name_a_value_by_a_fragment() {
    let schema = json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "$id": "https://example.com/root.json",
        "definitions": { "count": { "$id": "#count", "type": "integer" } },
        "total": { "$ref": "#count" }
    });

    assert_eq!(
        member(&schema, "total"),
        Ok(Some(json!({ "$id": "#count", "type": "integer" })))
    );
}

#[test]
fn draft_four_writes_an_id_without_the_dollar() {
    let schema = json!({
        "$schema": "http://json-schema.org/draft-04/schema#",
        "id": "http://example.com/root.json",
        "definitions": { "name": { "id": "name.json", "type": "string" } },
        "first": { "$ref": "name.json" }
    });

    assert_eq!(
        member(&schema, "first"),
        Ok(Some(json!({ "id": "name.json", "type": "string" })))
    );
}

/// As in json-schema-ref-parser, ids are followed only in a document whose
/// root has one or names a draft that defines them.
#[test]
fn ids_are_not_followed_in_a_document_that_does_not_ask_for_it() {
    let schema = json!({
        "definitions": { "name": { "$id": "https://example.com/name.json", "type": "string" } },
        "first": { "$ref": "https://example.com/name.json" }
    });

    assert_eq!(
        dereference_schema(&schema),
        Err(Error::ExternalReference {
            reference: "https://example.com/name.json".to_owned()
        })
    );
}

#[test]
fn a_reference_can_point_at_a_resource_inside_another_document() {
    let mut documents = Documents::new();
    let inserted = [
        (
            "https://example.com/root.json",
            json!({ "name": { "$ref": "https://example.com/ids/name" } }),
        ),
        (
            "https://example.com/bundle.json",
            json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "$defs": { "name": { "$id": "https://example.com/ids/name", "type": "string" } }
            }),
        ),
    ]
    .map(|(url, document)| documents.insert(url, document));

    assert_eq!(inserted, [Ok(()), Ok(())]);
    assert_eq!(documents.missing(), Vec::<String>::new());
    assert_eq!(
        documents
            .dereference("https://example.com/root.json", &Options::default())
            .map(|shared| shared.to_value()),
        Ok(json!({
            "name": { "$id": "https://example.com/ids/name", "type": "string" }
        }))
    );
}

#[test]
fn a_resource_inside_a_document_is_not_reported_missing() {
    let mut documents = Documents::new();
    let inserted = documents.insert(
        "https://example.com/root.json",
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$defs": { "name": { "$id": "name.json", "type": "string" } },
            "inside": { "$ref": "name.json" },
            "outside": { "$ref": "other.json" }
        }),
    );

    assert_eq!(inserted, Ok(()));
    assert_eq!(documents.missing(), ["https://example.com/other.json"]);
}

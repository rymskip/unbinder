//! What a run tells its hooks, what they can change, and which references
//! the options leave as written.

use serde_json::{Value, json};
use unbinder::{
    Documents, Error, ExternalPolicy, Hooks, LocalPolicy, Options, Place, PointerError, Step,
    dereference_schema_with, dereference_schema_with_hooks,
};

/// Hooks that write down what they are told, and leave out what is under
/// any name in `skip`.
#[derive(Default)]
struct Recorder {
    skip: Vec<&'static str>,
    continuing: bool,
    dereferenced: Vec<(String, String)>,
    circular: Vec<String>,
    failed: Vec<(String, Error)>,
}

/// A path written as a JSON Pointer.
fn pointer(path: &[Step<'_>]) -> String {
    path.iter()
        .map(|step| match step {
            Step::Name(name) => format!("/{name}"),
            Step::Index(position) => format!("/{position}"),
        })
        .collect()
}

impl Hooks for Recorder {
    fn excluded(&mut self, place: &Place<'_, '_>) -> Result<bool, Error> {
        Ok(place
            .path()
            .iter()
            .any(|step| matches!(step, Step::Name(name) if self.skip.contains(name))))
    }

    fn circular(&mut self, place: &Place<'_, '_>) {
        self.circular.push(place.source().to_string());
    }

    fn dereferenced(&mut self, place: &Place<'_, '_>) {
        let reference = place
            .value()
            .get("$ref")
            .and_then(Value::as_str)
            .unwrap_or_default();
        self.dereferenced
            .push((reference.to_owned(), pointer(place.path())));
    }

    fn failed(&mut self, place: &Place<'_, '_>, error: Error) -> Result<(), Error> {
        if !self.continuing {
            return Err(error);
        }
        self.failed.push((place.source().to_string(), error));
        Ok(())
    }
}

/// Dereferences `schema` with `hooks`, and hands them back.
fn run(schema: &Value, mut hooks: Recorder) -> (Result<Value, Error>, Recorder) {
    let output = dereference_schema_with_hooks(schema, &Options::default(), &mut hooks)
        .map(|shared| shared.to_value());
    (output, hooks)
}

/// The value at `path` in what dereferencing gave.
fn at(output: &Result<Value, Error>, path: &str) -> Option<Value> {
    match output {
        Ok(output) => output.pointer(path).cloned(),
        Err(_) => None,
    }
}

#[test]
fn reports_each_reference_it_replaces_where_it_stood() {
    let schema = json!({
        "a": { "$ref": "#/d" },
        "list": [{ "$ref": "#/d" }],
        "d": { "type": "string" }
    });

    let (output, hooks) = run(&schema, Recorder::default());

    assert!(output.is_ok());
    assert_eq!(
        hooks.dereferenced,
        [
            ("#/d".to_owned(), "/a".to_owned()),
            ("#/d".to_owned(), "/list/0".to_owned())
        ]
    );
}

#[test]
fn reports_where_a_reference_that_closes_a_cycle_is_written() {
    let schema = json!({
        "definitions": {
            "node": { "properties": { "next": { "$ref": "#/definitions/node" } } }
        },
        "list": { "$ref": "#/definitions/node" }
    });

    let (output, hooks) = run(&schema, Recorder::default());

    assert!(output.is_ok());
    assert_eq!(hooks.circular, ["#/definitions/node/properties/next"]);
}

#[test]
fn leaves_what_the_hooks_exclude_as_written() {
    let schema = json!({
        "name": { "$ref": "#/definitions/name" },
        "examples": [{ "$ref": "#/definitions/name" }, { "$ref": "#/nowhere" }],
        "definitions": { "name": { "type": "string" } }
    });

    let (output, _) = run(
        &schema,
        Recorder {
            skip: vec!["examples"],
            ..Recorder::default()
        },
    );

    assert_eq!(at(&output, "/examples"), schema.get("examples").cloned());
    assert_eq!(at(&output, "/name"), Some(json!({ "type": "string" })));
}

/// Leaves the value at `/a` alone once it is the target.
struct TargetAtA;

impl Hooks for TargetAtA {
    fn excluded(&mut self, place: &Place<'_, '_>) -> Result<bool, Error> {
        Ok(place.path() == [Step::Name("a")] && place.value().get("$ref").is_none())
    }
}

#[test]
fn a_target_excluded_where_a_reference_first_reaches_it_stays_as_written() {
    let schema = json!({
        "a": { "$ref": "#/definitions/node" },
        "b": { "$ref": "#/definitions/node" },
        "definitions": { "node": { "inner": { "$ref": "#/definitions/leaf" } }, "leaf": true }
    });

    let output = dereference_schema_with_hooks(&schema, &Options::default(), &mut TargetAtA)
        .map(|shared| shared.to_value());

    let written = Some(json!({ "inner": { "$ref": "#/definitions/leaf" } }));
    assert_eq!(at(&output, "/a"), written);
    assert_eq!(at(&output, "/b"), written);
    assert_eq!(at(&output, "/definitions/node"), written);
}

#[test]
fn carries_on_past_a_reference_that_does_not_resolve_when_told_to() {
    let schema = json!({ "a": { "$ref": "#/missing" }, "b": { "$ref": "#/c" }, "c": 1 });

    let (output, hooks) = run(
        &schema,
        Recorder {
            continuing: true,
            ..Recorder::default()
        },
    );

    assert_eq!(output, Ok(json!({ "a": null, "b": 1, "c": 1 })));
    assert_eq!(
        hooks.failed,
        [(
            "#/a".to_owned(),
            Error::UnresolvedReference {
                reference: "#/missing".to_owned(),
                cause: PointerError::MissingKey {
                    key: "missing".to_owned()
                }
            }
        )]
    );
}

#[test]
fn stops_at_a_reference_that_does_not_resolve_by_default() {
    let schema = json!({ "a": { "$ref": "#/missing" } });

    assert!(matches!(
        run(&schema, Recorder::default()).0,
        Err(Error::UnresolvedReference { .. })
    ));
}

/// Allows a few values, then says the time is up.
struct Budget(usize);

impl Hooks for Budget {
    fn proceed(&mut self) -> Result<(), Error> {
        self.0 = self
            .0
            .checked_sub(1)
            .ok_or(Error::TimeLimitExceeded { limit: 5 })?;
        Ok(())
    }
}

#[test]
fn a_hook_can_stop_the_run() {
    let schema = json!({ "a": [1, 2, 3, 4, 5, 6, 7, 8] });
    let finished = |budget| {
        dereference_schema_with_hooks(&schema, &Options::default(), &mut Budget(budget))
            .map(|shared| shared.to_value())
    };

    assert_eq!(finished(3), Err(Error::TimeLimitExceeded { limit: 5 }));
    assert_eq!(finished(20), Ok(schema.clone()));
}

#[test]
fn leaves_references_to_other_documents_as_written_on_request() {
    let schema = json!({
        "remote": { "$ref": "other.json#/x" },
        "local": { "$ref": "#/d" },
        "d": 1
    });
    let options = Options {
        external: ExternalPolicy::Ignore,
        ..Options::default()
    };

    assert_eq!(
        dereference_schema_with(&schema, &options).map(|dereferenced| dereferenced.schema),
        Ok(json!({ "remote": { "$ref": "other.json#/x" }, "local": 1, "d": 1 }))
    );
}

#[test]
fn follows_only_pointers_within_a_document_on_request() {
    let schema = json!({
        "$id": "https://example.com/root.json",
        "named": { "$ref": "#name" },
        "empty": { "$ref": "" },
        "escaped": { "$ref": "#%2Fd" },
        "d": { "$id": "#name", "type": "string" }
    });
    let options = Options {
        local: LocalPolicy::Pointers,
        ..Options::default()
    };

    let output = dereference_schema_with(&schema, &options).map(|dereferenced| dereferenced.schema);

    assert_eq!(at(&output, "/named"), Some(json!({ "$ref": "#name" })));
    assert_eq!(at(&output, "/empty"), Some(json!({ "$ref": "" })));
    assert_eq!(
        at(&output, "/escaped"),
        Some(json!({ "$id": "#name", "type": "string" }))
    );
}

#[test]
fn resolves_references_to_other_documents_against_the_base_it_is_given() {
    let mut documents = Documents::new();
    let inserted = [
        documents.insert(
            "https://example.com/schemas/root.json",
            json!({ "name": { "$ref": "shared/name.json" } }),
        ),
        documents.insert(
            "https://example.com/base/shared/name.json",
            json!({ "type": "string" }),
        ),
    ];
    let options = Options {
        external_base: Some("https://example.com/base/".to_owned()),
        ..Options::default()
    };

    assert_eq!(inserted, [Ok(()), Ok(())]);
    assert_eq!(
        documents
            .dereference("https://example.com/schemas/root.json", &options)
            .map(|shared| shared.to_value()),
        Ok(json!({ "name": { "type": "string" } }))
    );
}

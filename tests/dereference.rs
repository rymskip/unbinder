//! Behaviour of `dereference_schema` on hand-written documents.

use core::iter::once;
use std::thread::{Builder, JoinHandle};

use serde_json::{Map, Value, json};
use unbinder::{
    CircularPolicy, Dereferenced, Error, Options, PointerError, SiblingMerge, Stats, TrueTarget,
    dereference_schema, dereference_schema_shared, dereference_schema_with,
};

/// The result that holds `schema` and the kept references `circular`.
fn inlined(schema: Value, circular: &[&str]) -> Dereferenced {
    Dereferenced {
        schema,
        circular: circular
            .iter()
            .map(|reference| (*reference).to_owned())
            .collect(),
    }
}

/// What dereferencing `schema` leaves at its member `name`.
fn member(schema: &Value, name: &str) -> Result<Option<Value>, Error> {
    dereference_schema(schema).map(|dereferenced| dereferenced.schema.get(name).cloned())
}

fn unresolved(reference: &str, cause: PointerError) -> Error {
    Error::UnresolvedReference {
        reference: reference.to_owned(),
        cause,
    }
}

/// The result for a document whose only reference is `reference`.
fn target_of(reference: &str) -> Result<Option<Value>, Error> {
    member(
        &json!({
            "definitions": { "name": { "type": "string" } },
            "items": [{ "type": "string" }, { "type": "number" }],
            "target": { "$ref": reference }
        }),
        "target",
    )
}

fn names(count: usize) -> Vec<String> {
    (0..count).map(|index| format!("d{index}")).collect()
}

fn reference_to(name: &str) -> Value {
    json!({ "$ref": format!("#/{name}") })
}

/// The members of a document in which each of `count` definitions is `link`
/// applied to a reference to the next one, ending in one that holds `end`.
fn chain(count: usize, link: impl Fn(Value) -> Value, end: Value) -> Map<String, Value> {
    let names = names(count);
    let last = "end".to_owned();
    names
        .iter()
        .zip(names.iter().skip(1).chain(once(&last)))
        .map(|(name, next)| (name.clone(), link(reference_to(next))))
        .chain(once((last.clone(), end)))
        .collect()
}

#[test]
fn inlines_a_reference() {
    let schema = json!({
        "properties": { "name": { "$ref": "#/definitions/name" } },
        "definitions": { "name": { "type": "string" } }
    });

    assert_eq!(
        dereference_schema(&schema),
        Ok(inlined(
            json!({
                "properties": { "name": { "type": "string" } },
                "definitions": { "name": { "type": "string" } }
            }),
            &[]
        ))
    );
}

#[test]
fn inlines_references_inside_an_inlined_target() {
    let schema = json!({
        "a": { "$ref": "#/b" },
        "b": { "wrap": [{ "$ref": "#/c" }] },
        "c": { "type": "integer" }
    });

    assert_eq!(
        dereference_schema(&schema),
        Ok(inlined(
            json!({
                "a": { "wrap": [{ "type": "integer" }] },
                "b": { "wrap": [{ "type": "integer" }] },
                "c": { "type": "integer" }
            }),
            &[]
        ))
    );
}

#[test]
fn follows_a_reference_to_a_reference() {
    let schema = json!({
        "a": { "$ref": "#/b" },
        "b": { "$ref": "#/c" },
        "c": { "type": "null" }
    });

    assert_eq!(
        dereference_schema(&schema),
        Ok(inlined(
            json!({ "a": { "type": "null" }, "b": { "type": "null" }, "c": { "type": "null" } }),
            &[]
        ))
    );
}

#[test]
fn inlines_a_target_that_is_not_an_object() {
    let schema = json!({
        "flag": { "$ref": "#/definitions/anything" },
        "definitions": { "anything": true }
    });

    assert_eq!(member(&schema, "flag"), Ok(Some(json!(true))));
}

#[test]
fn resolves_array_indices() {
    assert_eq!(
        target_of("#/items/1"),
        Ok(Some(json!({ "type": "number" })))
    );
}

#[test]
fn resolves_an_empty_member_name() {
    let schema = json!({ "": { "type": "string" }, "target": { "$ref": "#/" } });

    assert_eq!(
        member(&schema, "target"),
        Ok(Some(json!({ "type": "string" })))
    );
}

#[test]
fn unescapes_pointer_tokens() {
    let schema = json!({
        "definitions": { "a/b~c": { "type": "string" } },
        "target": { "$ref": "#/definitions/a~1b~0c" }
    });

    assert_eq!(
        member(&schema, "target"),
        Ok(Some(json!({ "type": "string" })))
    );
}

#[test]
fn unescapes_tokens_that_hold_text_outside_ascii() {
    let schema = json!({
        "definitions": { "café/crème~": { "type": "string" } },
        "target": { "$ref": "#/definitions/café~1crème~0" }
    });

    assert_eq!(
        member(&schema, "target"),
        Ok(Some(json!({ "type": "string" })))
    );
}

#[test]
fn decodes_percent_encoding_before_unescaping() {
    let schema = json!({
        "definitions": {
            "foo bar é": { "type": "string" },
            "a/b": { "type": "number" }
        },
        "spaced": { "$ref": "#/definitions/foo%20bar%20%C3%A9" },
        "escaped": { "$ref": "#/definitions/a%7E1b" }
    });

    assert_eq!(
        member(&schema, "spaced"),
        Ok(Some(json!({ "type": "string" })))
    );
    assert_eq!(
        member(&schema, "escaped"),
        Ok(Some(json!({ "type": "number" })))
    );
}

#[test]
fn merges_sibling_keywords_over_the_target() {
    let schema = json!({
        "definitions": {
            "name": { "type": "string", "description": "A name" },
            "length": { "type": "integer" }
        },
        "first": {
            "$ref": "#/definitions/name",
            "description": "First name",
            "properties": { "length": { "$ref": "#/definitions/length" } }
        }
    });

    assert_eq!(
        member(&schema, "first"),
        Ok(Some(json!({
            "type": "string",
            "description": "First name",
            "properties": { "length": { "type": "integer" } }
        })))
    );
}

#[test]
fn merges_sibling_objects_into_the_objects_of_the_target() {
    let schema = json!({
        "base": {
            "type": "object",
            "properties": { "id": { "type": "integer" } },
            "required": ["id"]
        },
        "named": {
            "$ref": "#/base",
            "properties": { "name": { "type": "string" } },
            "required": ["name"]
        }
    });

    assert_eq!(
        member(&schema, "named"),
        Ok(Some(json!({
            "type": "object",
            "properties": { "id": { "type": "integer" }, "name": { "type": "string" } },
            "required": ["name"]
        })))
    );
}

#[test]
fn replaces_the_objects_of_the_target_on_request() {
    let schema = json!({
        "base": { "type": "object", "properties": { "id": { "type": "integer" } } },
        "named": { "$ref": "#/base", "properties": { "name": { "type": "string" } } }
    });
    let options = Options {
        siblings: SiblingMerge::Shallow,
        ..Options::default()
    };

    assert_eq!(
        dereference_schema_with(&schema, &options)
            .map(|dereferenced| dereferenced.schema.get("named").cloned()),
        Ok(Some(json!({
            "type": "object",
            "properties": { "name": { "type": "string" } }
        })))
    );
}

/// The keywords next to a reference apply along with its target, so they
/// are all that is left next to a target that accepts everything, and
/// nothing passes a target that accepts nothing.
#[test]
fn keeps_the_keywords_next_to_a_reference_to_a_boolean_schema() {
    let schema = json!({
        "definitions": { "anything": true, "nothing": false },
        "limited": { "$ref": "#/definitions/anything", "maxLength": 5 },
        "never": { "$ref": "#/definitions/nothing", "maxLength": 5 }
    });

    assert_eq!(
        member(&schema, "limited"),
        Ok(Some(json!({ "maxLength": 5 })))
    );
    assert_eq!(member(&schema, "never"), Ok(Some(json!(false))));
}

#[test]
fn keeps_the_keywords_next_to_a_reference_to_a_reference() {
    let schema = json!({
        "real": { "type": "string" },
        "alias": { "$ref": "#/real" },
        "limited": { "$ref": "#/alias", "maxLength": 5 }
    });

    assert_eq!(
        member(&schema, "limited"),
        Ok(Some(json!({ "type": "string", "maxLength": 5 })))
    );
}

#[test]
fn ignores_a_ref_member_that_is_not_a_string() {
    let schema = json!({ "properties": { "$ref": { "type": "string" } } });

    assert_eq!(
        dereference_schema(&schema),
        Ok(inlined(schema.clone(), &[]))
    );
}

#[test]
fn expands_a_recursive_definition_one_level() {
    let schema = json!({
        "definitions": {
            "node": { "properties": { "next": { "$ref": "#/definitions/node" } } }
        },
        "list": { "$ref": "#/definitions/node" }
    });

    assert_eq!(
        dereference_schema(&schema),
        Ok(inlined(
            json!({
                "definitions": {
                    "node": {
                        "properties": {
                            "next": { "properties": { "next": { "$ref": "#/definitions/node" } } }
                        }
                    }
                },
                "list": { "properties": { "next": { "$ref": "#/definitions/node" } } }
            }),
            &["#/definitions/node"]
        ))
    );
}

#[test]
fn keeps_references_between_members_of_a_cycle() {
    let schema = json!({
        "a": { "properties": { "b": { "$ref": "#/b" } } },
        "b": { "properties": { "a": { "$ref": "#/a" } } },
        "entry": { "$ref": "#/a" }
    });

    assert_eq!(
        dereference_schema(&schema),
        Ok(inlined(
            json!({
                "a": { "properties": { "b": { "properties": { "a": { "$ref": "#/a" } } } } },
                "b": { "properties": { "a": { "properties": { "b": { "$ref": "#/b" } } } } },
                "entry": { "properties": { "b": { "$ref": "#/b" } } }
            }),
            &["#/a", "#/b"]
        ))
    );
}

#[test]
fn keeps_a_reference_to_itself() {
    let schema = json!({ "a": { "$ref": "#/a" } });

    assert_eq!(
        dereference_schema(&schema),
        Ok(inlined(schema.clone(), &["#/a"]))
    );
}

#[test]
fn expands_a_reference_to_the_document_root_one_level() {
    let schema = json!({ "type": "object", "properties": { "self": { "$ref": "#" } } });

    assert_eq!(
        dereference_schema(&schema),
        Ok(inlined(
            json!({
                "type": "object",
                "properties": {
                    "self": { "type": "object", "properties": { "self": { "$ref": "#" } } }
                }
            }),
            &["#"]
        ))
    );
}

#[test]
fn keeps_the_siblings_of_a_kept_reference() {
    let schema = json!({
        "node": { "next": { "$ref": "#/node", "description": "The next node" } }
    });

    assert_eq!(
        dereference_schema(&schema),
        Ok(inlined(
            json!({
                "node": {
                    "next": {
                        "next": { "$ref": "#/node", "description": "The next node" },
                        "description": "The next node"
                    }
                }
            }),
            &["#/node"]
        ))
    );
}

/// Following every path through twelve definitions that all refer to one
/// another would visit them in every order.
#[test]
fn a_dense_cycle_stays_small() {
    let names = names(12);
    let schema: Value = names
        .iter()
        .map(|name| {
            let others: Map<String, Value> = names
                .iter()
                .filter(|other| *other != name)
                .map(|other| (other.clone(), reference_to(other)))
                .collect();
            (name.clone(), Value::Object(others))
        })
        .collect::<Map<String, Value>>()
        .into();
    let options = Options {
        max_bytes: 1_000_000,
        ..Options::default()
    };

    assert_eq!(
        dereference_schema_with(&schema, &options).map(|dereferenced| dereferenced.circular.len()),
        Ok(12)
    );
}

#[test]
fn a_long_cycle_does_not_exhaust_the_stack() {
    let names = names(5_000);
    let schema: Value = names
        .iter()
        .zip(names.iter().cycle().skip(1))
        .map(|(name, next)| (name.clone(), reference_to(next)))
        .collect::<Map<String, Value>>()
        .into();

    assert_eq!(
        dereference_schema(&schema).map(|dereferenced| dereferenced.circular.len()),
        Ok(5_000)
    );
}

#[test]
fn rejects_a_cycle_on_request() {
    let schema = json!({
        "definitions": {
            "node": { "properties": { "next": { "$ref": "#/definitions/node" } } }
        }
    });
    let options = Options {
        circular: CircularPolicy::Reject,
        ..Options::default()
    };

    assert_eq!(
        dereference_schema_with(&schema, &options),
        Err(Error::CircularReference {
            reference: "#/definitions/node".to_owned()
        })
    );
}

#[test]
fn rejecting_cycles_accepts_a_document_without_one() {
    let schema = json!({ "a": { "$ref": "#/b" }, "b": { "type": "string" } });
    let options = Options {
        circular: CircularPolicy::Reject,
        ..Options::default()
    };

    assert_eq!(
        dereference_schema_with(&schema, &options),
        Ok(inlined(
            json!({ "a": { "type": "string" }, "b": { "type": "string" } }),
            &[]
        ))
    );
}

#[test]
fn rejects_an_external_reference() {
    for reference in ["other.json#/definitions/name", "https://example.com/schema"] {
        assert_eq!(
            target_of(reference),
            Err(Error::ExternalReference {
                reference: reference.to_owned()
            })
        );
    }
}

#[test]
fn drops_the_keywords_next_to_a_reference_to_true_on_request() {
    let schema = json!({
        "anything": true,
        "bounded": { "$ref": "#/anything", "maxLength": 5 }
    });
    let options = Options {
        true_target: TrueTarget::Replace,
        ..Options::default()
    };

    assert_eq!(
        dereference_schema_with(&schema, &options),
        Ok(inlined(json!({ "anything": true, "bounded": true }), &[]))
    );
}

#[test]
fn rejects_a_reference_that_would_become_a_new_reference() {
    let schema = json!({
        "holder": { "$ref": { "$ref": "#/elsewhere" } },
        "elsewhere": "https://example.com/unresolved.json"
    });

    for circular in [
        CircularPolicy::Link,
        CircularPolicy::Expand,
        CircularPolicy::Ignore,
        CircularPolicy::Reject,
    ] {
        let options = Options {
            circular,
            ..Options::default()
        };

        assert_eq!(
            dereference_schema_with(&schema, &options),
            Err(Error::ReferenceToReference {
                reference: "#/elsewhere".to_owned()
            })
        );
    }
}

#[test]
fn inlines_an_object_under_the_name_of_a_reference() {
    let schema = json!({
        "properties": { "$ref": { "$ref": "#/definitions/uri" } },
        "definitions": { "uri": { "type": "string", "format": "uri-reference" } }
    });

    assert_eq!(
        member(&schema, "properties"),
        Ok(Some(
            json!({ "$ref": { "type": "string", "format": "uri-reference" } })
        ))
    );
}

#[test]
fn reports_a_pointer_without_a_leading_slash() {
    assert_eq!(
        target_of("#definitions/name"),
        Err(unresolved(
            "#definitions/name",
            PointerError::MissingLeadingSlash
        ))
    );
}

#[test]
fn reports_a_missing_member() {
    assert_eq!(
        target_of("#/definitions/missing"),
        Err(unresolved(
            "#/definitions/missing",
            PointerError::MissingKey {
                key: "missing".to_owned()
            }
        ))
    );
}

#[test]
fn reports_an_index_past_the_end() {
    assert_eq!(
        target_of("#/items/2"),
        Err(unresolved(
            "#/items/2",
            PointerError::IndexOutOfBounds {
                index: 2,
                length: 2
            }
        ))
    );
}

#[test]
fn reports_a_token_that_is_not_an_index() {
    for token in ["01", "+1", "-", "one", "", "99999999999999999999999999"] {
        let reference = format!("#/items/{token}");
        assert_eq!(
            target_of(&reference),
            Err(unresolved(
                &reference,
                PointerError::InvalidIndex {
                    token: token.to_owned()
                }
            ))
        );
    }
}

#[test]
fn reports_a_pointer_that_continues_past_a_leaf() {
    assert_eq!(
        target_of("#/definitions/name/type/deeper"),
        Err(unresolved(
            "#/definitions/name/type/deeper",
            PointerError::NotAContainer {
                token: "deeper".to_owned()
            }
        ))
    );
}

#[test]
fn reports_an_invalid_escape() {
    for token in ["a~2b", "a~"] {
        let reference = format!("#/definitions/{token}");
        assert_eq!(
            target_of(&reference),
            Err(unresolved(
                &reference,
                PointerError::InvalidEscape {
                    token: token.to_owned()
                }
            ))
        );
    }
}

#[test]
fn reports_percent_encoding_that_is_not_utf8() {
    assert_eq!(
        target_of("#/definitions/%FF"),
        Err(unresolved(
            "#/definitions/%FF",
            PointerError::InvalidPercentEncoding
        ))
    );
}

#[test]
fn a_pointer_passes_through_a_reference() {
    let schema = json!({
        "definitions": { "user": { "properties": { "name": { "type": "string" } } } },
        "properties": { "user": { "$ref": "#/definitions/user" } },
        "name": { "$ref": "#/properties/user/properties/name" }
    });

    assert_eq!(
        member(&schema, "name"),
        Ok(Some(json!({ "type": "string" })))
    );
}

#[test]
fn a_pointer_passes_through_a_chain_of_references() {
    let schema = json!({
        "a": { "$ref": "#/b" },
        "b": { "$ref": "#/c" },
        "c": { "leaf": { "type": "boolean" } },
        "target": { "$ref": "#/a/leaf" }
    });

    assert_eq!(
        member(&schema, "target"),
        Ok(Some(json!({ "type": "boolean" })))
    );
}

/// A pointer sees a reference as it is once its target is merged into it, so
/// the reference's own members come before the target's.
#[test]
fn a_pointer_finds_the_own_members_of_a_reference_first() {
    let schema = json!({
        "base": { "title": { "const": "base" }, "kind": { "const": "kind" } },
        "extended": { "$ref": "#/base", "title": { "const": "own" } },
        "title": { "$ref": "#/extended/title" },
        "kind": { "$ref": "#/extended/kind" }
    });

    assert_eq!(
        member(&schema, "title"),
        Ok(Some(json!({ "const": "own" })))
    );
    assert_eq!(
        member(&schema, "kind"),
        Ok(Some(json!({ "const": "kind" })))
    );
}

#[test]
fn a_pointer_may_leave_the_slashes_of_a_name_unescaped() {
    let schema = json!({
        "paths": { "/users/{id}": { "get": { "summary": "One user" } } },
        "escaped": { "$ref": "#/paths/~1users~1{id}/get" },
        "unescaped": { "$ref": "#/paths//users/{id}/get" }
    });

    assert_eq!(
        member(&schema, "escaped"),
        Ok(Some(json!({ "summary": "One user" })))
    );
    assert_eq!(
        member(&schema, "unescaped"),
        Ok(Some(json!({ "summary": "One user" })))
    );
}

#[test]
fn a_pointer_reads_a_slash_as_a_separator_first() {
    let schema = json!({
        "a": { "b": { "const": "nested" } },
        "a/b": { "const": "joined" },
        "target": { "$ref": "#/a/b" }
    });

    assert_eq!(
        member(&schema, "target"),
        Ok(Some(json!({ "const": "nested" })))
    );
}

#[test]
fn a_pointer_takes_the_longest_name_its_tokens_spell() {
    let schema = json!({
        "names": {
            "x/y": { "z": { "const": "short" } },
            "x/y/z": { "const": "long" },
            "x/y/z/w": { "const": "longer than the pointer" }
        },
        "target": { "$ref": "#/names/x/y/z" },
        "past": { "$ref": "#/names/x/y/z/const" }
    });

    assert_eq!(
        member(&schema, "target"),
        Ok(Some(json!({ "const": "long" })))
    );
    assert_eq!(member(&schema, "past"), Ok(Some(json!("long"))));
}

#[test]
fn a_pointer_through_a_reference_may_leave_slashes_unescaped() {
    let schema = json!({
        "base": { "a/b": { "const": "base" } },
        "extended": { "$ref": "#/base", "c/d": { "const": "own" } },
        "own": { "$ref": "#/extended/c/d" },
        "inherited": { "$ref": "#/extended/a/b" }
    });

    assert_eq!(member(&schema, "own"), Ok(Some(json!({ "const": "own" }))));
    assert_eq!(
        member(&schema, "inherited"),
        Ok(Some(json!({ "const": "base" })))
    );
}

#[test]
fn reports_the_first_token_no_name_starts_with() {
    let schema = json!({
        "names": { "x/y": {} },
        "target": { "$ref": "#/names/x/z" }
    });

    assert_eq!(
        member(&schema, "target"),
        Err(unresolved(
            "#/names/x/z",
            PointerError::MissingKey {
                key: "x".to_owned()
            }
        ))
    );
}

#[test]
fn reports_a_pointer_through_a_reference_to_itself() {
    let schema = json!({ "a": { "$ref": "#/a" }, "b": { "$ref": "#/a/x" } });

    assert_eq!(
        dereference_schema(&schema),
        Err(unresolved("#/a/x", PointerError::ReferenceLoop))
    );
}

#[test]
fn reports_pointers_that_pass_through_each_other() {
    let schema = json!({ "a": { "$ref": "#/b/x" }, "b": { "$ref": "#/a/x" } });

    assert!(matches!(
        dereference_schema(&schema),
        Err(Error::UnresolvedReference {
            cause: PointerError::ReferenceLoop,
            ..
        })
    ));
}

#[test]
fn a_pointer_passes_through_a_limited_number_of_references() {
    let schema: Value = chain(200, |next| next, json!({ "leaf": true }))
        .into_iter()
        .chain(once(("use".to_owned(), json!({ "$ref": "#/d0/leaf" }))))
        .collect::<Map<String, Value>>()
        .into();

    assert_eq!(
        dereference_schema(&schema),
        Err(Error::DepthLimitExceeded {
            limit: Options::DEFAULT_MAX_DEPTH
        })
    );
}

/// A document of `count` references where resolving each one's pointer
/// means resolving the next one's first.
fn waiting_pointers(count: usize) -> Value {
    let names = names(count);
    let end = "end".to_owned();
    let nested = (0..count).fold(json!({ "type": "null" }), |inner, _| {
        Value::Object(once(("leaf".to_owned(), inner)).collect())
    });
    names
        .iter()
        .zip(names.iter().skip(1).chain(once(&end)))
        .map(|(name, next)| (name.clone(), json!({ "$ref": format!("#/{next}/leaf") })))
        .chain(once((end.clone(), nested)))
        .collect::<Map<String, Value>>()
        .into()
}

#[test]
fn a_pointer_waits_for_the_references_it_passes_through() {
    assert_eq!(
        member(&waiting_pointers(3), "d0"),
        Ok(Some(json!({ "type": "null" })))
    );
}

/// Two thousand pointers that each wait on the next are resolved without
/// recursion. The output then nests too deeply, which is the error.
#[test]
fn pointers_that_wait_on_one_another_do_not_exhaust_the_stack() {
    assert_eq!(
        dereference_schema(&waiting_pointers(2_000)),
        Err(Error::DepthLimitExceeded {
            limit: Options::DEFAULT_MAX_DEPTH
        })
    );
}

#[test]
fn ignoring_cycles_leaves_every_reference_that_leads_to_one() {
    let schema = json!({
        "node": { "properties": { "next": { "$ref": "#/node" } } },
        "list": { "$ref": "#/node" },
        "wrapper": { "item": { "$ref": "#/node" } },
        "wrapped": { "$ref": "#/wrapper" },
        "plain": { "type": "string" },
        "name": { "$ref": "#/plain" }
    });
    let options = Options {
        circular: CircularPolicy::Ignore,
        ..Options::default()
    };

    assert_eq!(
        dereference_schema_with(&schema, &options),
        Ok(inlined(
            json!({
                "node": { "properties": { "next": { "$ref": "#/node" } } },
                "list": { "$ref": "#/node" },
                "wrapper": { "item": { "$ref": "#/node" } },
                "wrapped": { "$ref": "#/wrapper" },
                "plain": { "type": "string" },
                "name": { "type": "string" }
            }),
            &["#/node", "#/wrapper"]
        ))
    );
}

#[test]
fn enforces_the_depth_limit() {
    let schema = json!([[[1]]]);
    let fits = Options {
        max_depth: 4,
        ..Options::default()
    };
    let too_shallow = Options {
        max_depth: 3,
        ..Options::default()
    };

    assert_eq!(
        dereference_schema_with(&schema, &fits),
        Ok(inlined(schema.clone(), &[]))
    );
    assert_eq!(
        dereference_schema_with(&schema, &too_shallow),
        Err(Error::DepthLimitExceeded { limit: 3 })
    );
}

/// A value takes its own size inside its parent, so an array of two numbers
/// costs three values.
#[test]
fn enforces_the_size_limit() {
    const FITS: usize = 3 * size_of::<Value>();
    const TOO_SMALL: usize = FITS - 1;
    let schema = json!([1, 2]);
    let fits = Options {
        max_bytes: FITS,
        ..Options::default()
    };
    let too_small = Options {
        max_bytes: TOO_SMALL,
        ..Options::default()
    };

    assert_eq!(
        dereference_schema_with(&schema, &fits),
        Ok(inlined(schema.clone(), &[]))
    );
    assert_eq!(
        dereference_schema_with(&schema, &too_small),
        Err(Error::SizeLimitExceeded { limit: TOO_SMALL })
    );
}

/// A long string costs its length each time it is inlined, however few
/// values the output holds.
#[test]
fn counts_the_text_it_copies_against_the_size_limit() {
    let schema: Value = names(100)
        .into_iter()
        .map(|name| (name, reference_to("text")))
        .chain(once((
            "text".to_owned(),
            Value::String("x".repeat(100_000)),
        )))
        .collect::<Map<String, Value>>()
        .into();
    let options = Options {
        max_bytes: 1_000_000,
        ..Options::default()
    };

    assert_eq!(
        dereference_schema_with(&schema, &options),
        Err(Error::SizeLimitExceeded { limit: 1_000_000 })
    );
}

/// Inlining a target copies the targets nested inside it, so a nested target
/// that refers back to the outer one is part of the outer one's cycle.
#[test]
fn a_target_nested_in_a_cycle_belongs_to_it() {
    let schema = json!({
        "outer": { "inner": { "back": { "$ref": "#/outer" } } },
        "use": { "$ref": "#/outer/inner" }
    });

    assert_eq!(
        dereference_schema(&schema),
        Ok(inlined(
            json!({
                "outer": {
                    "inner": { "back": { "inner": { "back": { "$ref": "#/outer" } } } }
                },
                "use": { "back": { "$ref": "#/outer" } }
            }),
            &["#/outer"]
        ))
    );
}

/// Two thousand targets, each nested in the one before, with a reference to
/// every one of them.
#[test]
fn deeply_nested_targets_do_not_exhaust_the_stack() {
    let depth = 2_000;
    let nested = (0..depth).fold(json!({ "leaf": true }), |inner, _| {
        Value::Object(once(("a".to_owned(), inner)).collect())
    });
    let uses = (0..depth)
        .scan(String::from("#/nested"), |pointer, _| {
            pointer.push_str("/a");
            Some(json!({ "$ref": pointer.clone() }))
        })
        .collect();
    let schema: Value = [
        ("nested".to_owned(), nested),
        ("uses".to_owned(), Value::Array(uses)),
    ]
    .into_iter()
    .collect::<Map<String, Value>>()
    .into();
    let options = Options {
        max_depth: 3,
        ..Options::default()
    };

    assert_eq!(
        dereference_schema_with(&schema, &options),
        Err(Error::DepthLimitExceeded { limit: 3 })
    );
}

#[test]
fn a_deeply_nested_document_hits_the_depth_limit() {
    let schema = (0..2_000).fold(Value::Null, |nested, _| Value::Array(vec![nested]));

    assert_eq!(
        dereference_schema(&schema),
        Err(Error::DepthLimitExceeded {
            limit: Options::DEFAULT_MAX_DEPTH
        })
    );
}

#[test]
fn a_long_chain_of_references_hits_the_depth_limit() {
    let schema = Value::Object(chain(5_000, |next| next, json!({ "type": "null" })));

    assert_eq!(
        dereference_schema(&schema),
        Err(Error::DepthLimitExceeded {
            limit: Options::DEFAULT_MAX_DEPTH
        })
    );
}

/// A WebAssembly module gets a stack of one mebibyte by default.
#[test]
fn the_default_depth_limit_fits_a_small_stack() {
    let schema = Value::Object(chain(
        5_000,
        |next| json!({ "next": [next] }),
        json!({ "type": "null" }),
    ));

    let outcome = Builder::new()
        .stack_size(1024 * 1024)
        .spawn(move || dereference_schema(&schema))
        .map(JoinHandle::join);

    assert!(matches!(
        outcome,
        Ok(Ok(Err(Error::DepthLimitExceeded {
            limit: Options::DEFAULT_MAX_DEPTH
        })))
    ));
}

/// Thirty definitions that each hold the next one twice describe an output
/// of a billion values.
#[test]
fn an_output_that_doubles_at_every_level_hits_the_node_limit() {
    let schema = Value::Object(chain(30, |next| json!([next.clone(), next]), json!("leaf")));
    let options = Options {
        max_bytes: 1_000_000,
        ..Options::default()
    };

    assert_eq!(
        dereference_schema_with(&schema, &options),
        Err(Error::SizeLimitExceeded { limit: 1_000_000 })
    );
}

/// Sixty definitions that each hold the next one twice would fill every
/// disk there is. The shared form holds each once, so measuring the output
/// takes no longer than reading the document.
#[test]
fn an_output_too_large_to_build_is_measured_without_building_it() {
    let schema = Value::Object(chain(60, |next| json!([next.clone(), next]), json!("leaf")));
    let options = Options {
        max_depth: 512,
        ..Options::default()
    };

    assert_eq!(
        dereference_schema_with(&schema, &options),
        Err(Error::SizeLimitExceeded {
            limit: Options::DEFAULT_MAX_BYTES
        })
    );
}

/// Merging a sibling object into a target means looking its name up among
/// the target's members, which the size limit also bounds.
#[test]
fn bounds_the_lookups_that_merging_takes() {
    let wide: Map<String, Value> = (0..2_000)
        .map(|member| (format!("member{member:04}"), json!({ "$ref": "#/leaf" })))
        .collect();
    let schema: Value = names(2_000)
        .into_iter()
        .map(|name| {
            (
                name,
                json!({ "$ref": "#/wide", "member0000": { "title": "Own" } }),
            )
        })
        .chain([
            ("wide".to_owned(), Value::Object(wide)),
            ("leaf".to_owned(), json!({ "type": "string" })),
        ])
        .collect::<Map<String, Value>>()
        .into();
    let options = Options {
        max_bytes: 1_000_000,
        ..Options::default()
    };

    assert_eq!(
        dereference_schema_shared(&schema, &options).map(|shared| shared.circular().len()),
        Err(Error::SizeLimitExceeded { limit: 1_000_000 })
    );
}

#[test]
fn the_shared_form_writes_the_same_output() {
    let schema = json!({
        "definitions": {
            "name": { "type": "string", "description": "A name" },
            "node": { "properties": { "next": { "$ref": "#/definitions/node" } } }
        },
        "first": { "$ref": "#/definitions/name" },
        "second": { "$ref": "#/definitions/name", "description": "Second", "minLength": 1 },
        "list": { "$ref": "#/definitions/node" }
    });

    let shared = dereference_schema_shared(&schema, &Options::default());
    let tree = dereference_schema(&schema);

    assert_eq!(
        shared
            .as_ref()
            .map(|shared| (shared.to_value(), shared.circular().to_vec())),
        tree.as_ref()
            .map(|tree| (tree.schema.clone(), tree.circular.clone()))
    );
    assert_eq!(
        shared.map(|shared| serde_json::to_string(&shared).map_err(|error| error.to_string())),
        tree.map(|tree| serde_json::to_string(&tree.schema).map_err(|error| error.to_string()))
    );
}

#[test]
fn counts_containers_and_references() {
    let schema = json!({
        "properties": {
            "tags": { "type": "array", "items": { "$ref": "#/definitions/tag" } },
            "$ref": { "type": "string" }
        },
        "required": ["tags", []],
        "definitions": { "tag": { "type": "string" } }
    });

    assert_eq!(
        Stats::of(&schema),
        Stats {
            objects: 7,
            arrays: 2,
            references: 1
        }
    );
}

#[test]
fn errors_say_which_reference_failed_and_why() {
    assert_eq!(
        unresolved(
            "#/definitions/missing",
            PointerError::MissingKey {
                key: "missing".to_owned()
            }
        )
        .to_string(),
        "cannot resolve reference `#/definitions/missing`: no member named `missing`"
    );
    assert_eq!(
        Error::DepthLimitExceeded { limit: 128 }.to_string(),
        "depth limit of 128 levels exceeded"
    );
    assert_eq!(
        Error::ExternalReference {
            reference: "other.json".to_owned()
        }
        .to_string(),
        "external reference `other.json` is not supported"
    );
}

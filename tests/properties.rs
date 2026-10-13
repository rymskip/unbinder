//! Properties that hold for every document.

extern crate alloc;

use alloc::collections::BTreeSet;

use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use proptest::collection::{btree_map, vec};
use proptest::prelude::{Just, Strategy, any, prop_assert, prop_assert_eq, prop_oneof, proptest};
use serde_json::{Map, Value, json};
use unbinder::{
    CircularPolicy, Dereferenced, Error, Options, Stats, dereference_schema,
    dereference_schema_shared, dereference_schema_with,
};

fn scalar() -> impl Strategy<Value = Value> {
    prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::from),
        any::<i64>().prop_map(Value::from),
        ".*".prop_map(Value::from),
    ]
}

/// Documents in which no object has a `$ref` member.
fn document_without_references() -> impl Strategy<Value = Value> {
    scalar().prop_recursive(4, 64, 6, |inner| {
        prop_oneof![
            vec(inner.clone(), 0..6).prop_map(Value::from),
            btree_map("[a-z]{0,5}", inner, 0..6)
                .prop_map(|members| Value::Object(members.into_iter().collect())),
        ]
    })
}

/// Documents whose strings are mostly references, valid and not, placed
/// under names those references can reach.
fn document_full_of_references() -> impl Strategy<Value = Value> {
    let pointer = prop_oneof![
        Just("#"),
        Just("#/a"),
        Just("#/b/a"),
        Just("#/items/0"),
        Just("#/items/1/b"),
        Just("#/a/$ref"),
        Just("#/~"),
        Just("#/%FF"),
        Just("#a"),
        Just("other.json"),
    ];
    let name = prop_oneof![Just("a"), Just("b"), Just("items"), Just("$ref")];
    pointer
        .prop_map(Value::from)
        .prop_recursive(5, 48, 4, move |inner| {
            prop_oneof![
                vec(inner.clone(), 0..4).prop_map(Value::from),
                btree_map(name.clone(), inner, 0..4).prop_map(|members| {
                    Value::Object(
                        members
                            .into_iter()
                            .map(|(name, member)| (name.to_owned(), member))
                            .collect(),
                    )
                }),
            ]
        })
}

/// For each definition, the definitions it refers to.
fn reference_graph() -> impl Strategy<Value = Vec<Vec<usize>>> {
    (1_usize..=6).prop_flat_map(|count| vec(vec(0..count, 0..=3), count))
}

fn reference_to(definition: usize) -> String {
    format!("#/definitions/d{definition}")
}

fn document_of(graph: &[Vec<usize>]) -> Value {
    let definitions: Map<String, Value> = graph
        .iter()
        .enumerate()
        .map(|(definition, successors)| {
            let properties: Map<String, Value> = successors
                .iter()
                .enumerate()
                .map(|(slot, successor)| {
                    (
                        format!("p{slot}"),
                        json!({ "$ref": reference_to(*successor) }),
                    )
                })
                .collect();
            (
                format!("d{definition}"),
                json!({ "properties": properties }),
            )
        })
        .collect();
    json!({ "definitions": definitions })
}

/// Every pair of definitions where the first leads to the second, found by
/// closing the graph under transitivity.
fn reachability(graph: &[Vec<usize>]) -> BTreeSet<(usize, usize)> {
    let mut reachable: BTreeSet<(usize, usize)> = graph
        .iter()
        .enumerate()
        .flat_map(|(from, successors)| successors.iter().map(move |to| (from, *to)))
        .collect();
    loop {
        let extended: BTreeSet<(usize, usize)> = reachable
            .iter()
            .flat_map(|(from, via)| {
                reachable
                    .iter()
                    .filter(move |(start, _)| start == via)
                    .map(move |(_, to)| (*from, *to))
            })
            .chain(reachable.iter().copied())
            .collect();
        if extended == reachable {
            return reachable;
        }
        reachable = extended;
    }
}

/// References to the definitions that can reach themselves.
fn references_on_a_cycle(graph: &[Vec<usize>]) -> Vec<String> {
    let reachable = reachability(graph);
    (0..graph.len())
        .filter(|definition| reachable.contains(&(*definition, *definition)))
        .map(reference_to)
        .collect()
}

/// The references in the document to definitions that are on a cycle or can
/// reach one.
fn references_that_lead_to_a_cycle(graph: &[Vec<usize>]) -> Vec<String> {
    let reachable = reachability(graph);
    (0..graph.len())
        .filter(|definition| graph.iter().flatten().any(|used| used == definition))
        .filter(|definition| {
            reachable
                .iter()
                .any(|(from, to)| from == definition && reachable.contains(&(*to, *to)))
        })
        .map(reference_to)
        .collect()
}

fn references_in(value: &Value) -> BTreeSet<&str> {
    match value {
        Value::Object(members) => members
            .values()
            .flat_map(references_in)
            .chain(members.get("$ref").and_then(Value::as_str))
            .collect(),
        Value::Array(items) => items.iter().flat_map(references_in).collect(),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => BTreeSet::new(),
    }
}

proptest! {
    #[test]
    fn a_document_without_references_is_unchanged(document in document_without_references()) {
        prop_assert_eq!(
            dereference_schema(&document),
            Ok(Dereferenced { schema: document.clone(), circular: Vec::new() })
        );
    }

    #[test]
    fn any_member_name_can_be_referenced(name in any::<String>()) {
        let token = name.replace('~', "~0").replace('/', "~1");
        let reference = format!(
            "#/definitions/{}",
            utf8_percent_encode(&token, NON_ALPHANUMERIC)
        );
        let definitions: Map<String, Value> = [(name, json!({ "type": "string" }))].into_iter().collect();
        let schema = json!({ "definitions": definitions, "target": { "$ref": reference } });

        let dereferenced = dereference_schema(&schema)?;

        prop_assert_eq!(dereferenced.schema.get("target"), Some(&json!({ "type": "string" })));
    }

    #[test]
    fn a_name_can_be_referenced_with_its_slashes_unescaped(
        parts in vec("[a-z~ %{}]{0,4}", 2..6),
    ) {
        let name = parts.join("/");
        let reference = format!(
            "#/definitions/{}",
            parts
                .iter()
                .map(|part| utf8_percent_encode(&part.replace('~', "~0"), NON_ALPHANUMERIC).to_string())
                .collect::<Vec<_>>()
                .join("/")
        );
        let definitions: Map<String, Value> = [(name, json!({ "type": "string" }))].into_iter().collect();
        let schema = json!({ "definitions": definitions, "target": { "$ref": reference } });

        let dereferenced = dereference_schema(&schema)?;

        prop_assert_eq!(dereferenced.schema.get("target"), Some(&json!({ "type": "string" })));
    }

    #[test]
    fn exactly_the_references_on_a_cycle_are_kept(graph in reference_graph()) {
        let expected = references_on_a_cycle(&graph);

        let dereferenced = dereference_schema(&document_of(&graph))?;

        prop_assert_eq!(
            references_in(&dereferenced.schema),
            expected.iter().map(String::as_str).collect::<BTreeSet<_>>()
        );
        prop_assert_eq!(Stats::of(&dereferenced.schema).references == 0, expected.is_empty());
        prop_assert_eq!(dereferenced.circular, expected);
    }

    #[test]
    fn a_cycle_is_rejected_exactly_when_one_exists(graph in reference_graph()) {
        let expected = references_on_a_cycle(&graph);
        let options = Options { circular: CircularPolicy::Reject, ..Options::default() };

        match dereference_schema_with(&document_of(&graph), &options) {
            Ok(dereferenced) => {
                prop_assert!(expected.is_empty());
                prop_assert!(references_in(&dereferenced.schema).is_empty());
            }
            Err(Error::CircularReference { reference }) => prop_assert!(expected.contains(&reference)),
            Err(
                unexpected @ (Error::ExternalReference { .. }
                | Error::UnresolvedReference { .. }
                | Error::InvalidUrl { .. }
                | Error::MissingDocument { .. }
                | Error::ReferenceToReference { .. }
                | Error::DocumentLimitExceeded { .. }
                | Error::DepthLimitExceeded { .. }
                | Error::SizeLimitExceeded { .. }),
            ) => return Err(unexpected.into()),
        }
    }

    #[test]
    fn ignoring_cycles_leaves_exactly_the_references_that_lead_to_one(graph in reference_graph()) {
        let expected = references_that_lead_to_a_cycle(&graph);
        let options = Options { circular: CircularPolicy::Ignore, ..Options::default() };

        let dereferenced = dereference_schema_with(&document_of(&graph), &options)?;

        prop_assert_eq!(
            references_in(&dereferenced.schema),
            expected.iter().map(String::as_str).collect::<BTreeSet<_>>()
        );
        prop_assert_eq!(&dereferenced.circular, &expected);
        prop_assert_eq!(dereference_schema_with(&dereferenced.schema, &options), Ok(dereferenced.clone()));
    }

    #[test]
    fn hostile_references_fail_cleanly_or_leave_only_reported_cycles(
        document in document_full_of_references()
    ) {
        let options = Options { max_depth: 64, max_bytes: 1_000_000, ..Options::default() };

        let outcome = dereference_schema_with(&document, &options);

        prop_assert_eq!(&outcome, &dereference_schema_with(&document, &options));
        if let Ok(dereferenced) = outcome {
            let reported: BTreeSet<&str> = dereferenced.circular.iter().map(String::as_str).collect();
            prop_assert!(references_in(&dereferenced.schema).is_subset(&reported));
        }
    }

    #[test]
    fn the_shared_form_serializes_to_the_tree(
        document in document_full_of_references(),
        ignore in any::<bool>()
    ) {
        let options = Options {
            circular: if ignore { CircularPolicy::Ignore } else { CircularPolicy::Expand },
            max_depth: 64,
            max_bytes: 1_000_000,
            ..Options::default()
        };

        match (dereference_schema_shared(&document, &options), dereference_schema_with(&document, &options)) {
            (Ok(shared), Ok(tree)) => {
                prop_assert_eq!(serde_json::to_value(&shared)?, tree.schema.clone());
                prop_assert_eq!(shared.to_value(), tree.schema);
                prop_assert_eq!(shared.circular(), tree.circular);
            }
            (Err(shared), Err(tree)) => prop_assert_eq!(shared, tree),
            (Ok(_), Err(_)) | (Err(_), Ok(_)) => prop_assert!(false, "the two forms disagree"),
        }
    }

    #[test]
    fn the_shared_form_of_a_reference_graph_serializes_to_the_tree(graph in reference_graph()) {
        let document = document_of(&graph);

        let shared = dereference_schema_shared(&document, &Options::default())?;

        prop_assert_eq!(serde_json::to_value(&shared)?, dereference_schema(&document)?.schema);
    }
}

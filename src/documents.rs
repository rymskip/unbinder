//! Documents that refer to one another, and the resources inside them.

use alloc::borrow::ToOwned;
use alloc::collections::btree_map::Entry as MapEntry;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::format;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use fluent_uri::{Iri, IriRef};
use serde_json::{Map, Value};

use crate::dereference::{Form, Options, SharedSchema, expand};
use crate::error::Error;
use crate::walk::REFERENCE_KEY;

/// A document, or a part of one that an id makes a resource of its own.
/// References written in it are relative to its URL, and a pointer with no
/// URL in front of it starts at its root.
#[derive(Clone, Debug)]
pub(crate) struct Document<'doc> {
    pub(crate) root: &'doc Value,
    /// Absent for a document dereferenced on its own that gives itself none.
    pub(crate) url: Option<Rc<str>>,
}

/// A reference split into the document it names and the pointer into it.
/// The document is empty for a reference into the document it is written
/// in.
pub(crate) fn split(reference: &str) -> (&str, &str) {
    reference.split_once('#').unwrap_or((reference, ""))
}

/// The URL `location` names, made absolute against `base` and normalized so
/// that two spellings of one URL compare equal.
pub(crate) fn absolute(base: Option<&str>, location: &str) -> Result<String, Error> {
    let invalid = || Error::InvalidUrl {
        url: location.to_owned(),
    };
    let resolved = if let Some(base) = base {
        let Ok(base) = Iri::parse(base) else {
            return Err(invalid());
        };
        let Ok(reference) = IriRef::parse(location) else {
            return Err(invalid());
        };
        let Ok(resolved) = reference.resolve_against(&base) else {
            return Err(invalid());
        };
        resolved.normalize()
    } else {
        let Ok(parsed) = Iri::parse(location) else {
            return Err(invalid());
        };
        parsed.normalize()
    };
    Ok(resolved.strip_fragment().as_str().to_owned())
}

/// Whether the ids in a document are followed. They are when its root has
/// one, or says it is written to a draft of JSON Schema that defines them.
fn follows_ids(root: &Value) -> bool {
    let Some(members) = root.as_object() else {
        return false;
    };
    id_of(members, uses_legacy_ids(members, false)).is_some() || dialect(members).is_some()
}

/// Whether ids are written `id` as in draft 4, or `$id` as since. A schema
/// that names no draft writes them as the schema around it does.
fn uses_legacy_ids(members: &Map<String, Value>, inherited: bool) -> bool {
    dialect(members).unwrap_or(inherited)
}

/// Whether the schema declares draft 4, if it declares a draft at all.
fn dialect(members: &Map<String, Value>) -> Option<bool> {
    if let Some(schema) = members.get("$schema").and_then(Value::as_str) {
        if schema.contains("draft-04/") {
            return Some(true);
        }
        let later = [
            "draft-06/",
            "draft-07/",
            "draft/2019-09/",
            "draft/2020-12/",
            "oas/3.1/",
        ];
        if later.iter().any(|draft| schema.contains(draft)) {
            return Some(false);
        }
    }
    let version = members.get("openapi").and_then(Value::as_str)?;
    (version == "3.1" || version.starts_with("3.1.")).then_some(false)
}

fn id_of(members: &Map<String, Value>, legacy: bool) -> Option<&str> {
    let id = members
        .get(if legacy { "id" } else { "$id" })
        .and_then(Value::as_str)?;
    (!id.is_empty()).then_some(id)
}

/// What a document says about its own layout.
pub(crate) struct Survey<'doc> {
    /// Each value an id makes a resource of its own, with the resource.
    pub(crate) resources: Vec<Document<'doc>>,
    /// Each value an id names by a fragment, under the full URL.
    pub(crate) names: Vec<(String, &'doc Value)>,
    /// Each reference, with the resource it is written in.
    pub(crate) references: Vec<(&'doc str, Document<'doc>)>,
}

/// Walks `document` once, noting where an id starts a resource or names a
/// value, and which resource each reference is written in.
pub(crate) fn survey<'doc>(document: &Document<'doc>) -> Survey<'doc> {
    let follows = follows_ids(document.root);
    let mut survey = Survey {
        resources: Vec::new(),
        names: Vec::new(),
        references: Vec::new(),
    };
    let mut pending = vec![(document.root, document.clone(), false)];
    while let Some((value, mut within, mut legacy)) = pending.pop() {
        match value {
            Value::Object(members) => {
                if follows {
                    legacy = uses_legacy_ids(members, legacy);
                    if let Some(id) = id_of(members, legacy) {
                        let (location, fragment) = split(id);
                        // An id that is not a URL names nothing and is left
                        // as the keyword it is.
                        if !location.is_empty()
                            && let Ok(url) = absolute(within.url.as_deref(), location)
                        {
                            within = Document {
                                root: value,
                                url: Some(url.into()),
                            };
                            survey.resources.push(within.clone());
                        }
                        if !fragment.is_empty()
                            && let Some(base) = &within.url
                        {
                            survey.names.push((format!("{base}#{fragment}"), value));
                        }
                    }
                }
                if let Some(reference) = members.get(REFERENCE_KEY).and_then(Value::as_str) {
                    survey.references.push((reference, within.clone()));
                }
                pending.extend(
                    members
                        .values()
                        .map(|member| (member, within.clone(), legacy)),
                );
            }
            Value::Array(items) => {
                pending.extend(items.iter().map(|item| (item, within.clone(), legacy)));
            }
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
        }
    }
    survey
}

#[derive(Debug)]
struct Entry {
    document: Value,
    /// The URLs this document refers to.
    refers_to: BTreeSet<String>,
    /// The URLs its ids give to parts of it.
    provides: BTreeSet<String>,
}

/// A set of documents, each known by its URL, whose references may point
/// into one another.
///
/// Nothing here reads a file or opens a connection. The caller loads each
/// document and adds it, and [`Documents::missing`] says which ones the set
/// still refers to.
///
/// ```
/// use serde_json::json;
/// use unbinder::{Documents, Options};
///
/// let mut documents = Documents::new();
/// documents.insert(
///     "https://example.com/schemas/order.json",
///     json!({ "properties": { "customer": { "$ref": "customer.json#/definitions/name" } } }),
/// )?;
/// assert_eq!(
///     documents.missing(),
///     ["https://example.com/schemas/customer.json"]
/// );
///
/// documents.insert(
///     "https://example.com/schemas/customer.json",
///     json!({ "definitions": { "name": { "type": "string" } } }),
/// )?;
/// assert!(documents.missing().is_empty());
///
/// let order = documents.dereference("https://example.com/schemas/order.json", &Options::default())?;
/// assert_eq!(
///     order.to_value(),
///     json!({ "properties": { "customer": { "type": "string" } } })
/// );
/// # Ok::<(), unbinder::Error>(())
/// ```
#[derive(Debug, Default)]
pub struct Documents {
    entries: BTreeMap<String, Entry>,
}

impl Documents {
    /// An empty set.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }

    /// Adds `document` under `url`, replacing any document already there.
    ///
    /// # Errors
    ///
    /// Fails when `url` is not an absolute URL, and when a reference in
    /// `document` names a document by something that is not a URL.
    pub fn insert(&mut self, url: &str, document: Value) -> Result<(), Error> {
        self.add(url, document).map(|_| ())
    }

    /// Adds `document` under `url` and returns the URLs it refers to.
    pub(crate) fn add(&mut self, url: &str, document: Value) -> Result<&BTreeSet<String>, Error> {
        let url = absolute(None, url)?;
        let (refers_to, provides) = {
            let surveyed = survey(&Document {
                root: &document,
                url: Some(url.as_str().into()),
            });
            let provides: BTreeSet<String> = surveyed
                .resources
                .iter()
                .filter_map(|resource| resource.url.as_deref().map(ToOwned::to_owned))
                .collect();
            let refers_to = surveyed
                .references
                .iter()
                .map(|(reference, within)| (split(reference).0, within))
                .filter(|(location, _)| !location.is_empty())
                .map(|(location, within)| absolute(within.url.as_deref(), location))
                .filter(|referred| match referred {
                    Ok(referred) => *referred != url && !provides.contains(referred),
                    Err(_) => true,
                })
                .collect::<Result<_, _>>()?;
            (refers_to, provides)
        };
        let entry = Entry {
            document,
            refers_to,
            provides,
        };
        let entry = match self.entries.entry(url) {
            MapEntry::Occupied(mut held) => {
                held.insert(entry);
                held.into_mut()
            }
            MapEntry::Vacant(free) => free.insert(entry),
        };
        Ok(&entry.refers_to)
    }

    /// `url` as the set knows it: absolute, and spelled one way.
    #[cfg(feature = "wasm")]
    pub(crate) fn normalized(url: &str) -> Result<String, Error> {
        absolute(None, url)
    }

    /// The URLs the documents in the set refer to that are neither in it nor
    /// given by an id to a part of one that is, sorted. Dereferencing can
    /// succeed once this is empty.
    #[must_use]
    pub fn missing(&self) -> Vec<String> {
        self.entries
            .values()
            .flat_map(|entry| &entry.refers_to)
            .filter(|url| self.get(url).is_none() && self.holding(url).is_none())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .cloned()
            .collect()
    }

    /// Dereferences the document at `url`, inlining what it refers to in
    /// the other documents of the set as well as in itself.
    ///
    /// A reference that takes part in a cycle and is left in place stays as
    /// it is written, relative to the document it came from.
    ///
    /// # Errors
    ///
    /// Fails when `url` or a document it leads to is not in the set, and for
    /// the reasons [`dereference_schema_shared`] does.
    ///
    /// [`dereference_schema_shared`]: crate::dereference_schema_shared
    pub fn dereference(&self, url: &str, options: &Options) -> Result<SharedSchema<'_>, Error> {
        let url = absolute(None, url)?;
        let Some(root) = self.get(&url) else {
            return Err(Error::MissingDocument { url });
        };
        let expanded = expand(&root, Some(self), options, Form::Written)?;
        if expanded.bytes > options.max_bytes {
            return Err(Error::SizeLimitExceeded {
                limit: options.max_bytes,
            });
        }
        Ok(expanded.schema)
    }

    /// The document known by the absolute URL `url`.
    pub(crate) fn get(&self, url: &str) -> Option<Document<'_>> {
        let (known, entry) = self.entries.get_key_value(url)?;
        Some(Document {
            root: &entry.document,
            url: Some(known.as_str().into()),
        })
    }

    /// The document that has a part an id gives the absolute URL `url`.
    pub(crate) fn holding(&self, url: &str) -> Option<Document<'_>> {
        let (known, entry) = self
            .entries
            .iter()
            .find(|(_, entry)| entry.provides.contains(url))?;
        Some(Document {
            root: &entry.document,
            url: Some(known.as_str().into()),
        })
    }
}

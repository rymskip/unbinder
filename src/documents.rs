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

use crate::bundle::bundle;
use crate::dereference::{Form, Options, SharedSchema, expand};
use crate::error::{Error, PointerError};
use crate::hooks::{Hooks, NoHooks};
use crate::pointer;
#[cfg(feature = "wasm")]
use crate::references::Policy;
use crate::references::References;
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
        self.dereference_with_hooks(url, options, &mut NoHooks)
    }

    /// [`Documents::dereference`], telling `hooks` what it does as it goes
    /// and asking them what to leave as written.
    ///
    /// # Errors
    ///
    /// Fails for the reasons [`Documents::dereference`] does, unless
    /// [`Hooks::failed`] lets the run carry on, and when a hook stops it.
    pub fn dereference_with_hooks<H: Hooks + ?Sized>(
        &self,
        url: &str,
        options: &Options,
        hooks: &mut H,
    ) -> Result<SharedSchema<'_>, Error> {
        let root = self.root(url)?;
        let expanded = expand(&root, Some(self), options, Form::Written, hooks)?;
        if expanded.bytes > options.max_bytes {
            return Err(Error::SizeLimitExceeded {
                limit: options.max_bytes,
            });
        }
        Ok(expanded.schema)
    }

    /// Bundles the document at `url`: each value of another document that a
    /// reference leads to is written into it once, at the first place that
    /// refers to it, and every other reference to that value points there.
    /// The result refers to no other document, and its references are not
    /// inlined. References within it are pointed straight at what they lead
    /// to, unless [`Options::internal`] says otherwise.
    ///
    /// The places are chosen as json-schema-ref-parser chooses them.
    ///
    /// ```
    /// use serde_json::json;
    /// use unbinder::{Documents, Options};
    ///
    /// let mut documents = Documents::new();
    /// documents.insert(
    ///     "https://example.com/order.json",
    ///     json!({
    ///         "billing": { "$ref": "address.json" },
    ///         "shipping": { "$ref": "address.json" }
    ///     }),
    /// )?;
    /// documents.insert("https://example.com/address.json", json!({ "type": "object" }))?;
    ///
    /// assert_eq!(
    ///     documents.bundle("https://example.com/order.json", &Options::default())?,
    ///     json!({
    ///         "billing": { "type": "object" },
    ///         "shipping": { "$ref": "#/billing" }
    ///     })
    /// );
    /// # Ok::<(), unbinder::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Fails when `url` or a document it leads to is not in the set, when a
    /// reference does not resolve, and when the bundle nests deeper than
    /// [`Options::max_depth`] or takes more than [`Options::max_bytes`].
    pub fn bundle(&self, url: &str, options: &Options) -> Result<Value, Error> {
        self.bundle_with_hooks(url, options, &mut NoHooks)
    }

    /// [`Documents::bundle`], telling `hooks` what it does as it goes and
    /// asking them what to leave as written.
    ///
    /// # Errors
    ///
    /// Fails for the reasons [`Documents::bundle`] does, unless
    /// [`Hooks::failed`] lets the run carry on, and when a hook stops it.
    pub fn bundle_with_hooks<H: Hooks + ?Sized>(
        &self,
        url: &str,
        options: &Options,
        hooks: &mut H,
    ) -> Result<Value, Error> {
        bundle(&self.root(url)?, Some(self), options, hooks)
    }

    /// The URLs of the documents in the set, sorted.
    #[must_use]
    pub fn urls(&self) -> Vec<&str> {
        self.entries.keys().map(String::as_str).collect()
    }

    /// The document at `url`, if it is in the set.
    #[must_use]
    pub fn document(&self, url: &str) -> Option<&Value> {
        match absolute(None, url) {
            Ok(url) => self.entries.get(&url).map(|entry| &entry.document),
            // Nothing is in the set under what is not a URL.
            Err(_) => None,
        }
    }

    /// The value `reference` points at: an absolute URL of a document in
    /// the set, with a JSON Pointer or a name an id gives in its fragment.
    /// A pointer passes through any reference on its way, and a bare
    /// reference where it ends is followed to what it leads to.
    ///
    /// ```
    /// use serde_json::json;
    /// use unbinder::{Documents, Options};
    ///
    /// let mut documents = Documents::new();
    /// documents.insert(
    ///     "https://example.com/order.json",
    ///     json!({ "customer": { "$ref": "customer.json" } }),
    /// )?;
    /// documents.insert(
    ///     "https://example.com/customer.json",
    ///     json!({ "properties": { "name": { "type": "string" } } }),
    /// )?;
    ///
    /// assert_eq!(
    ///     documents.lookup(
    ///         "https://example.com/order.json#/customer/properties/name",
    ///         &Options::default()
    ///     )?,
    ///     json!({ "type": "string" })
    /// );
    /// # Ok::<(), unbinder::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Fails when the document is not in the set and when the pointer does
    /// not resolve.
    pub fn lookup(&self, reference: &str, options: &Options) -> Result<Value, Error> {
        let (location, fragment) = split(reference);
        let root = self.root(location)?;
        let local = format!("#{fragment}");
        let mut references = References::analyze(&root, Some(self), options)?;
        let target = references.target(&root, &local)?;
        let (target, _, _) = references.settle(target)?;
        Ok(target.value().clone())
    }

    /// Puts `value` where `reference` points: an absolute URL of a document
    /// in the set, with a JSON Pointer in its fragment. Members missing on
    /// the way are added as empty objects, and `-` as the last token of a
    /// pointer into an array adds an item at its end.
    ///
    /// # Errors
    ///
    /// Fails when the document is not in the set, when the pointer cannot
    /// be read or passes through a value that is neither an object nor an
    /// array, and when the document then refers to something that is not a
    /// URL. The document is unchanged when it fails.
    pub fn set(&mut self, reference: &str, value: Value) -> Result<(), Error> {
        let (location, fragment) = split(reference);
        let url = absolute(None, location)?;
        let unresolved = |cause| Error::UnresolvedReference {
            reference: reference.to_owned(),
            cause,
        };
        let tokens = pointer::tokens(fragment).map_err(unresolved)?;
        let Some(entry) = self.entries.get(&url) else {
            return Err(Error::MissingDocument { url });
        };
        let mut document = entry.document.clone();
        place(&mut document, &tokens, value).map_err(unresolved)?;
        self.add(&url, document).map(|_| ())
    }

    /// The absolute URLs of the documents the document at `url` refers to,
    /// under the policies in `options`.
    #[cfg(feature = "wasm")]
    pub(crate) fn referred(&self, url: &str, options: &Options) -> Result<Vec<String>, Error> {
        let Some(document) = self.get(url) else {
            return Ok(Vec::new());
        };
        let policy = Policy::of(options);
        let mut referred = Vec::new();
        for (reference, within) in survey(&document).references {
            let (location, _) = split(reference);
            if location.is_empty() || !policy.follows(reference) {
                continue;
            }
            let base = options.external_base.as_deref().or(within.url.as_deref());
            referred.push(absolute(base, location)?);
        }
        Ok(referred)
    }

    /// The document at `url`, which has to be in the set.
    pub(crate) fn root(&self, url: &str) -> Result<Document<'_>, Error> {
        let url = absolute(None, url)?;
        self.get(&url).ok_or(Error::MissingDocument { url })
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

/// Puts `value` at the end of `tokens` in `document`, adding an empty object
/// for each member missing on the way.
fn place(document: &mut Value, tokens: &[String], value: Value) -> Result<(), PointerError> {
    let Some((last, leading)) = tokens.split_last() else {
        *document = value;
        return Ok(());
    };
    let mut current = document;
    for token in leading {
        current = match current {
            Value::Object(members) => members
                .entry(token.clone())
                .or_insert_with(|| Value::Object(Map::new())),
            Value::Array(items) => {
                let index = array_index(token, items.len())?;
                if index == items.len() {
                    items.push(Value::Object(Map::new()));
                }
                let length = items.len();
                items
                    .get_mut(index)
                    .ok_or(PointerError::IndexOutOfBounds { index, length })?
            }
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {
                return Err(PointerError::NotAContainer {
                    token: token.clone(),
                });
            }
        };
    }
    match current {
        Value::Object(members) => {
            members.insert(last.clone(), value);
            Ok(())
        }
        Value::Array(items) => {
            let index = array_index(last, items.len())?;
            match items.get_mut(index) {
                Some(item) => *item = value,
                None => items.push(value),
            }
            Ok(())
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {
            Err(PointerError::NotAContainer {
                token: last.clone(),
            })
        }
    }
}

/// The position `token` names in an array of `length` items: one that is
/// there, or `-` or `length` for one added at the end.
fn array_index(token: &str, length: usize) -> Result<usize, PointerError> {
    if token == "-" {
        return Ok(length);
    }
    let invalid = || PointerError::InvalidIndex {
        token: token.to_owned(),
    };
    if token != "0" && token.starts_with(['0', '+']) {
        return Err(invalid());
    }
    match token.parse::<usize>() {
        Ok(index) if index <= length => Ok(index),
        Ok(index) => Err(PointerError::IndexOutOfBounds { index, length }),
        Err(_) => Err(invalid()),
    }
}

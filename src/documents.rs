//! Documents that refer to one another.

use alloc::borrow::ToOwned;
use alloc::collections::btree_map::Entry as MapEntry;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::vec::Vec;

use fluent_uri::{Iri, IriRef};
use serde_json::Value;

use crate::dereference::{Form, Options, SharedSchema, expand};
use crate::error::Error;
use crate::walk::{Values, reference_of};

/// A document and the URL it is known by.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Document<'doc> {
    pub(crate) root: &'doc Value,
    /// Absent for a document dereferenced on its own.
    pub(crate) url: Option<&'doc str>,
}

/// A reference split into the document it names and the pointer into it.
/// The document is empty for a reference into the document it is written
/// in.
pub(crate) fn split(reference: &str) -> (&str, &str) {
    reference.split_once('#').unwrap_or((reference, ""))
}

/// The URL `location` names, made absolute against `base` and normalized so
/// that two spellings of one URL compare equal.
fn absolute(base: Option<&str>, location: &str) -> Result<String, Error> {
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

#[derive(Debug)]
struct Entry {
    document: Value,
    /// The URLs of the documents this one refers to.
    refers_to: BTreeSet<String>,
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
        let refers_to = Values::of(&document)
            .filter_map(reference_of)
            .map(|reference| split(reference).0)
            .filter(|location| !location.is_empty())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .map(|location| absolute(Some(&url), location))
            .collect::<Result<_, _>>()?;
        let entry = match self.entries.entry(url) {
            MapEntry::Occupied(mut held) => {
                held.insert(Entry {
                    document,
                    refers_to,
                });
                held.into_mut()
            }
            MapEntry::Vacant(free) => free.insert(Entry {
                document,
                refers_to,
            }),
        };
        Ok(&entry.refers_to)
    }

    /// `url` as the set knows it: absolute, and spelled one way.
    #[cfg(feature = "wasm")]
    pub(crate) fn normalized(url: &str) -> Result<String, Error> {
        absolute(None, url)
    }

    /// The URLs the documents in the set refer to that are not in it,
    /// sorted. Dereferencing can succeed once this is empty.
    #[must_use]
    pub fn missing(&self) -> Vec<String> {
        self.entries
            .values()
            .flat_map(|entry| &entry.refers_to)
            .filter(|url| !self.entries.contains_key(*url))
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
        let root = self.find(None, url)?;
        let (shared, bytes) = expand(root, Some(self), options, Form::Written)?;
        if bytes > options.max_bytes {
            return Err(Error::SizeLimitExceeded {
                limit: options.max_bytes,
            });
        }
        Ok(shared)
    }

    /// The document that `location` names when written in the document at
    /// `base`.
    pub(crate) fn find(&self, base: Option<&str>, location: &str) -> Result<Document<'_>, Error> {
        let url = absolute(base, location)?;
        match self.entries.get_key_value(&url) {
            Some((known, entry)) => Ok(Document {
                root: &entry.document,
                url: Some(known),
            }),
            None => Err(Error::MissingDocument { url }),
        }
    }
}

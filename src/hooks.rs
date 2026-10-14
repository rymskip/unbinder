//! What a run tells its caller, and asks of it, as it goes.

use alloc::collections::BTreeMap;
use alloc::rc::Rc;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::cell::OnceCell;
use core::fmt;
use core::ptr;

use serde_json::Value;

use crate::documents::Document;
use crate::error::Error;

/// A step from a value to one of its children.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "wasm", derive(serde::Serialize), serde(untagged))]
pub enum Step<'doc> {
    /// A member of an object, by name.
    Name(&'doc str),
    /// An item of an array, by position.
    Index(usize),
}

/// `path` as a JSON Pointer in a URI fragment, `#` included, written as
/// json-schema-ref-parser writes one: `%`, `~` and `/` in a name are escaped.
pub(crate) fn fragment(path: &[Step<'_>]) -> String {
    let mut written = String::from("#");
    for step in path {
        written.push('/');
        match step {
            Step::Name(name) => {
                for character in name.chars() {
                    match character {
                        '%' => written.push_str("%25"),
                        '~' => written.push_str("~0"),
                        '/' => written.push_str("~1"),
                        other => written.push(other),
                    }
                }
            }
            Step::Index(position) => written.push_str(&position.to_string()),
        }
    }
    written
}

/// Where a value is written: the URL of its document, if the document has
/// one, and the path to it from the document's root.
///
/// It displays as a URL with the path as a JSON Pointer in its fragment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source<'run> {
    /// The URL of the document.
    pub url: Option<&'run str>,
    /// The path from the root of the document.
    pub path: Vec<Step<'run>>,
}

impl fmt::Display for Source<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.url.unwrap_or_default())?;
        formatter.write_str(&fragment(&self.path))
    }
}

/// A value a hook is told about: where it is in the output, and the value
/// as its document has it.
#[derive(Debug)]
pub struct Place<'run, 'doc> {
    pub(crate) path: &'run [Step<'doc>],
    pub(crate) value: &'doc Value,
    pub(crate) sources: &'run Sources<'doc>,
}

impl<'run, 'doc> Place<'run, 'doc> {
    /// The path to the value from the root of the output.
    #[must_use]
    pub const fn path(&self) -> &'run [Step<'doc>] {
        self.path
    }

    /// The value as its document has it. For a reference, this is the
    /// reference as written.
    #[must_use]
    pub const fn value(&self) -> &'doc Value {
        self.value
    }

    /// Where the value is written.
    #[must_use]
    pub fn source(&self) -> Source<'run> {
        self.sources.of(self.value)
    }
}

/// Callbacks a dereference or a bundle makes as it goes. Every method has a
/// default that changes nothing.
///
/// ```
/// use serde_json::json;
/// use unbinder::{Hooks, Options, Place, Step, dereference_schema_with_hooks};
///
/// /// Leaves everything under `examples` as written.
/// struct SkipExamples;
///
/// impl Hooks for SkipExamples {
///     fn excluded(&mut self, place: &Place<'_, '_>) -> Result<bool, unbinder::Error> {
///         Ok(place.path().last() == Some(&Step::Name("examples")))
///     }
/// }
///
/// let schema = json!({
///     "name": { "$ref": "#/definitions/name" },
///     "examples": [{ "$ref": "#/definitions/name" }],
///     "definitions": { "name": { "type": "string" } }
/// });
///
/// let shared = dereference_schema_with_hooks(&schema, &Options::default(), &mut SkipExamples)?;
/// assert_eq!(shared.to_value()["examples"], json!([{ "$ref": "#/definitions/name" }]));
/// assert_eq!(shared.to_value()["name"], json!({ "type": "string" }));
/// # Ok::<(), unbinder::Error>(())
/// ```
pub trait Hooks {
    /// Whether to leave the value at `place`, and everything inside it, as
    /// written. Asked of every value the run reaches, and again of a target
    /// where a reference first reaches it.
    ///
    /// # Errors
    ///
    /// An error stops the run with that error.
    fn excluded(&mut self, place: &Place<'_, '_>) -> Result<bool, Error> {
        let _ = place;
        Ok(false)
    }

    /// A reference that takes part in a cycle, left in place or linked back
    /// to its target.
    fn circular(&mut self, place: &Place<'_, '_>) {
        let _ = place;
    }

    /// A reference that was replaced by its target. The place is where the
    /// reference stood, and its value is the reference as written.
    fn dereferenced(&mut self, place: &Place<'_, '_>) {
        let _ = place;
    }

    /// A member of an object that is an object with a `$ref` member, once a
    /// bundle has looked through it.
    fn bundled(&mut self, place: &Place<'_, '_>) {
        let _ = place;
    }

    /// The reference at `place` cannot be resolved. The run stops with the
    /// error unless this returns `Ok`, and then the reference becomes `null`.
    ///
    /// # Errors
    ///
    /// Returns the error to stop the run with.
    fn failed(&mut self, place: &Place<'_, '_>, error: Error) -> Result<(), Error> {
        let _ = place;
        Err(error)
    }

    /// Called before each value is expanded, so that a run can be stopped
    /// part way, say once it has taken too long.
    ///
    /// # Errors
    ///
    /// An error stops the run with that error.
    fn proceed(&mut self) -> Result<(), Error> {
        Ok(())
    }

    /// Whether the run should keep track of where it is, which every place
    /// a hook is told about needs. A run with hooks that observe nothing
    /// can skip it.
    fn observes(&self) -> bool {
        true
    }
}

/// Hooks that observe nothing and stop at the first error.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct NoHooks;

impl Hooks for NoHooks {
    fn observes(&self) -> bool {
        false
    }
}

/// Where each value of a document came from.
type Parents<'doc> = BTreeMap<*const Value, (*const Value, Step<'doc>)>;

/// Where the values of a run's documents are written, worked out the first
/// time it is asked for.
#[derive(Debug)]
pub(crate) struct Sources<'doc> {
    documents: Vec<Document<'doc>>,
    index: OnceCell<Index<'doc>>,
}

#[derive(Debug)]
struct Index<'doc> {
    parents: Parents<'doc>,
    roots: BTreeMap<*const Value, Option<Rc<str>>>,
}

impl<'doc> Sources<'doc> {
    pub(crate) const fn new(documents: Vec<Document<'doc>>) -> Self {
        Self {
            documents,
            index: OnceCell::new(),
        }
    }

    fn index(&self) -> &Index<'doc> {
        self.index.get_or_init(|| {
            let mut index = Index {
                parents: BTreeMap::new(),
                roots: BTreeMap::new(),
            };
            for document in &self.documents {
                index
                    .roots
                    .insert(ptr::from_ref(document.root), document.url.clone());
                let mut pending = vec![document.root];
                while let Some(value) = pending.pop() {
                    let parent = ptr::from_ref(value);
                    match value {
                        Value::Object(members) => {
                            for (name, member) in members {
                                index
                                    .parents
                                    .insert(ptr::from_ref(member), (parent, Step::Name(name)));
                                pending.push(member);
                            }
                        }
                        Value::Array(items) => {
                            for (position, item) in items.iter().enumerate() {
                                index
                                    .parents
                                    .insert(ptr::from_ref(item), (parent, Step::Index(position)));
                                pending.push(item);
                            }
                        }
                        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
                    }
                }
            }
            index
        })
    }

    /// Where `value` is written. A value in none of the documents is the
    /// root of a document with no URL.
    pub(crate) fn of(&self, value: &Value) -> Source<'_> {
        let index = self.index();
        let mut path = Vec::new();
        let mut current = ptr::from_ref(value);
        while let Some((parent, step)) = index.parents.get(&current) {
            path.push(*step);
            current = *parent;
        }
        path.reverse();
        Source {
            url: index.roots.get(&current).and_then(Option::as_deref),
            path,
        }
    }
}

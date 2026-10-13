//! Inlining of references.

use alloc::borrow::ToOwned;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;
use core::convert::Infallible;
use core::ptr;

use serde::{Serialize, Serializer};
use serde_json::{Map, Value};

use crate::error::Error;
use crate::references::{CycleId, References, Target};
use crate::shared::{Merged, Shared};
use crate::walk::{REFERENCE_KEY, reference_of};

/// The memory a value takes inside its parent.
const VALUE_BYTES: usize = size_of::<Value>();
/// The memory an object member takes, not counting its value or the text of
/// its name.
const NAME_BYTES: usize = size_of::<String>();

/// What to do with a reference that takes part in a cycle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(
    feature = "wasm",
    derive(serde::Deserialize, tsify::Tsify),
    serde(rename_all = "lowercase")
)]
pub enum CircularPolicy {
    /// Inline the cycle once, then leave the reference that would repeat it
    /// in place.
    #[default]
    Expand,
    /// Leave in place every reference that leads to a cycle, without
    /// inlining any of it.
    Ignore,
    /// Fail with [`Error::CircularReference`].
    Reject,
}

/// How the keywords next to a `$ref` are combined with its target.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(
    feature = "wasm",
    derive(serde::Deserialize, tsify::Tsify),
    serde(rename_all = "lowercase")
)]
pub enum SiblingMerge {
    /// Where a sibling and the target both hold an object under one keyword,
    /// merge the two objects, and so on inward. Anything else in a sibling
    /// replaces what the target holds.
    #[default]
    Deep,
    /// A sibling replaces whatever the target holds under the same keyword.
    Shallow,
}

/// How to dereference a document.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(
    feature = "wasm",
    derive(serde::Deserialize, tsify::Tsify),
    serde(default, rename_all = "camelCase", deny_unknown_fields)
)]
pub struct Options {
    /// What to do with a reference that takes part in a cycle.
    pub circular: CircularPolicy,
    /// How the keywords next to a `$ref` are combined with its target.
    pub siblings: SiblingMerge,
    /// How deep the output may nest, and how many references a pointer may
    /// pass through in one step. A reference and the value inlined for it
    /// each count as a level.
    pub max_depth: usize,
    /// Roughly how much memory the output may take as a tree, in bytes.
    /// Writing a target out once per reference to it can make a small
    /// document describe a very large output.
    pub max_bytes: usize,
}

impl Options {
    /// The default for [`Options::max_depth`]. Low enough for an unoptimized
    /// build to stay inside the one mebibyte stack of a WebAssembly module.
    pub const DEFAULT_MAX_DEPTH: usize = 128;
    /// The default for [`Options::max_bytes`], 256 mebibytes.
    pub const DEFAULT_MAX_BYTES: usize = 268_435_456;
}

impl Default for Options {
    fn default() -> Self {
        Self {
            circular: CircularPolicy::default(),
            siblings: SiblingMerge::default(),
            max_depth: Self::DEFAULT_MAX_DEPTH,
            max_bytes: Self::DEFAULT_MAX_BYTES,
        }
    }
}

/// A dereferenced document.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
pub struct Dereferenced {
    /// The document with its references inlined.
    #[cfg_attr(feature = "wasm", tsify(type = "unknown"))]
    pub schema: Value,
    /// The references left in place because they lead to a cycle, sorted and
    /// without duplicates.
    pub circular: Vec<String>,
}

/// A dereferenced document that holds each inlined target once.
///
/// It borrows from the document it was made from and takes about as much
/// memory, however many references the document has. Serializing it writes
/// the full output without building it first.
///
/// ```
/// use serde_json::json;
/// use unbinder::{Options, dereference_schema_shared};
///
/// let schema = json!({
///     "first": { "$ref": "#/name" },
///     "second": { "$ref": "#/name" },
///     "name": { "type": "string" }
/// });
///
/// let shared = dereference_schema_shared(&schema, &Options::default())?;
///
/// assert_eq!(
///     serde_json::to_string(&shared)?,
///     r#"{"first":{"type":"string"},"name":{"type":"string"},"second":{"type":"string"}}"#
/// );
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Clone, Debug)]
pub struct SharedSchema<'doc> {
    shared: Shared<'doc>,
    circular: Vec<String>,
}

impl<'doc> SharedSchema<'doc> {
    /// The references left in place because they lead to a cycle, sorted and
    /// without duplicates.
    #[must_use]
    pub fn circular(&self) -> &[String] {
        &self.circular
    }

    /// Builds the output as a tree, copying each target once per reference
    /// to it.
    #[must_use]
    pub fn to_value(&self) -> Value {
        self.shared.to_value()
    }

    #[cfg(feature = "wasm")]
    pub(crate) const fn shared(&self) -> &Shared<'doc> {
        &self.shared
    }
}

impl Serialize for SharedSchema<'_> {
    fn serialize<Format: Serializer>(&self, format: Format) -> Result<Format::Ok, Format::Error> {
        self.shared.serialize(format)
    }
}

/// Replaces every `$ref` in `schema` with a copy of the value it points at,
/// using the default [`Options`].
///
/// A reference is any object with a string `$ref` member. Its other members
/// are merged over the inlined target, so a sibling `description` overrides
/// the target's. References are resolved against `schema` as given, which is
/// left untouched. A pointer that reaches another reference on its way
/// passes through it.
///
/// A reference that takes part in a cycle is inlined wherever it is reached
/// from outside the cycle and left in place inside it, so recursive schemas
/// are expanded one level and stay finite.
///
/// # Errors
///
/// Fails when a reference is external or does not resolve, and when the
/// output exceeds a default limit.
///
/// # Examples
///
/// ```
/// use serde_json::json;
/// use unbinder::dereference_schema;
///
/// let schema = json!({
///     "properties": { "name": { "$ref": "#/definitions/name" } },
///     "definitions": { "name": { "type": "string" } }
/// });
///
/// let dereferenced = dereference_schema(&schema)?;
///
/// assert_eq!(
///     dereferenced.schema,
///     json!({
///         "properties": { "name": { "type": "string" } },
///         "definitions": { "name": { "type": "string" } }
///     })
/// );
/// assert!(dereferenced.circular.is_empty());
/// # Ok::<(), unbinder::Error>(())
/// ```
pub fn dereference_schema(schema: &Value) -> Result<Dereferenced, Error> {
    dereference_schema_with(schema, &Options::default())
}

/// [`dereference_schema`] with `options` in place of the defaults.
///
/// # Errors
///
/// Fails when a reference is external, does not resolve, or is circular
/// under [`CircularPolicy::Reject`], and when the output exceeds a limit set
/// in `options`.
///
/// # Examples
///
/// ```
/// use serde_json::json;
/// use unbinder::{CircularPolicy, Options, dereference_schema_with};
///
/// let schema = json!({
///     "node": { "properties": { "next": { "$ref": "#/node" } } },
///     "list": { "$ref": "#/node" }
/// });
/// let options = Options {
///     circular: CircularPolicy::Ignore,
///     ..Options::default()
/// };
///
/// let dereferenced = dereference_schema_with(&schema, &options)?;
///
/// assert_eq!(dereferenced.schema, schema);
/// assert_eq!(dereferenced.circular, ["#/node"]);
/// # Ok::<(), unbinder::Error>(())
/// ```
pub fn dereference_schema_with(schema: &Value, options: &Options) -> Result<Dereferenced, Error> {
    let shared = dereference_schema_shared(schema, options)?;
    Ok(Dereferenced {
        schema: shared.to_value(),
        circular: shared.circular,
    })
}

/// [`dereference_schema_with`] without the copying: the result holds each
/// inlined target once and writes the full output when it is serialized.
///
/// # Errors
///
/// Fails for the reasons [`dereference_schema_with`] does. The limits apply
/// to the output as it will be written.
pub fn dereference_schema_shared<'doc>(
    schema: &'doc Value,
    options: &Options,
) -> Result<SharedSchema<'doc>, Error> {
    let (shared, bytes) = expand(schema, options)?;
    if bytes > options.max_bytes {
        return Err(Error::SizeLimitExceeded {
            limit: options.max_bytes,
        });
    }
    Ok(shared)
}

/// Expands `schema` into its shared form, and measures the memory it would
/// take written out as a tree.
pub(crate) fn expand<'doc>(
    schema: &'doc Value,
    options: &Options,
) -> Result<(SharedSchema<'doc>, usize), Error> {
    let mut expansion = Expansion {
        references: References::analyze(schema, options.max_depth)?,
        options,
        bytes_left: options.max_bytes,
        inlined: BTreeMap::new(),
        circular: BTreeSet::new(),
    };
    let root = expansion.value(schema, None, options.max_depth)?;
    let schema = SharedSchema {
        shared: root.shared,
        circular: expansion
            .circular
            .into_iter()
            .map(ToOwned::to_owned)
            .collect(),
    };
    Ok((schema, root.bytes))
}

/// A value with what it comes to once written out as a tree.
#[derive(Clone)]
struct Measured<'doc> {
    shared: Shared<'doc>,
    /// How many levels deep it nests.
    height: usize,
    /// The memory it takes, its own place in its parent included.
    bytes: usize,
}

impl<'doc> Measured<'doc> {
    fn scalar(value: &'doc Value, text: usize) -> Self {
        Self {
            shared: Shared::Original(value),
            height: 1,
            bytes: VALUE_BYTES.saturating_add(text),
        }
    }

    /// Whether this is `value` exactly as the document has it.
    fn is(&self, value: &Value) -> bool {
        match &self.shared {
            Shared::Original(original) => ptr::eq(*original, value),
            Shared::Array(_) | Shared::Object(_) | Shared::Inlined(_) | Shared::Merged(_) => false,
        }
    }
}

/// The measurements of a container as its children are added to it.
struct Totals {
    height: usize,
    bytes: usize,
}

impl Totals {
    const fn new() -> Self {
        Self {
            height: 1,
            bytes: VALUE_BYTES,
        }
    }

    fn add(&mut self, child: &Measured<'_>, name: usize) {
        self.height = self.height.max(child.height.saturating_add(1));
        self.bytes = self.bytes.saturating_add(name).saturating_add(child.bytes);
    }
}

struct Expansion<'doc, 'options> {
    references: References<'doc>,
    options: &'options Options,
    /// What the shared form may still allocate.
    bytes_left: usize,
    /// The expansion of each target inlined so far, by where the target sits
    /// in the document.
    inlined: BTreeMap<*const Value, Measured<'doc>>,
    circular: BTreeSet<&'doc str>,
}

impl<'doc> Expansion<'doc, '_> {
    /// Takes `bytes` out of the budget. Called before the memory is
    /// allocated, so the limit holds for what is requested and not only for
    /// what has been filled.
    fn charge(&mut self, slots: usize, slot: usize) -> Result<(), Error> {
        self.bytes_left = self
            .bytes_left
            .checked_sub(slots.saturating_mul(slot))
            .ok_or(Error::SizeLimitExceeded {
                limit: self.options.max_bytes,
            })?;
        Ok(())
    }

    fn too_deep(&self) -> Error {
        Error::DepthLimitExceeded {
            limit: self.options.max_depth,
        }
    }

    /// Expands `node`. `enclosing` is the cycle of the innermost target being
    /// inlined around it.
    fn value(
        &mut self,
        node: &'doc Value,
        enclosing: Option<CycleId>,
        depth_left: usize,
    ) -> Result<Measured<'doc>, Error> {
        let target = match node {
            Value::Object(_) | Value::Array(_) => self.references.at(node),
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => None,
        };
        match target {
            // A target reached where it stands expands to what a reference
            // to it inlines, so the two share one expansion.
            Some(target) if target.cycle() == enclosing => self.inline(&target, depth_left),
            Some(_) | None => self.expand(node, enclosing, depth_left),
        }
    }

    /// The expansion of `target`, made once and shared by every reference to
    /// it. A target always expands the same way: inside its own cycle.
    fn inline(
        &mut self,
        target: &Target<'doc>,
        depth_left: usize,
    ) -> Result<Measured<'doc>, Error> {
        let address = ptr::from_ref(target.value());
        if let Some(done) = self.inlined.get(&address) {
            return if done.height <= depth_left {
                Ok(done.clone())
            } else {
                Err(self.too_deep())
            };
        }
        let mut expanded = self.expand(target.value(), target.cycle(), depth_left)?;
        expanded.shared = match expanded.shared {
            // A scalar is copied, and a reference to a reference shares the
            // expansion it already points at.
            Shared::Original(
                value @ (Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_)),
            ) => Shared::Original(value),
            shared @ Shared::Inlined(_) => shared,
            shared @ (Shared::Original(Value::Array(_) | Value::Object(_))
            | Shared::Array(_)
            | Shared::Object(_)
            | Shared::Merged(_)) => Shared::Inlined(Rc::new(shared)),
        };
        self.inlined.insert(address, expanded.clone());
        Ok(expanded)
    }

    /// Expands `node` without sharing it.
    ///
    /// The recursion uses plain loops, not iterator adaptors, to keep each
    /// level of nesting to a few small stack frames.
    fn expand(
        &mut self,
        node: &'doc Value,
        enclosing: Option<CycleId>,
        depth_left: usize,
    ) -> Result<Measured<'doc>, Error> {
        let Some(depth_left) = depth_left.checked_sub(1) else {
            return Err(self.too_deep());
        };
        match node {
            Value::Object(members) => match reference_of(node) {
                Some(reference) => self.reference(node, members, reference, enclosing, depth_left),
                None => self.object(node, members, enclosing, depth_left),
            },
            Value::Array(items) => {
                let mut totals = Totals::new();
                let mut changed: Option<Vec<Shared<'doc>>> = None;
                for (position, item) in items.iter().enumerate() {
                    let child = self.value(item, enclosing, depth_left)?;
                    totals.add(&child, 0);
                    if let Some(expanded) = &mut changed {
                        expanded.push(child.shared);
                    } else if !child.is(item) {
                        self.charge(items.len(), size_of::<Shared<'_>>())?;
                        let mut expanded = Vec::with_capacity(items.len());
                        expanded.extend(items.iter().take(position).map(Shared::Original));
                        expanded.push(child.shared);
                        changed = Some(expanded);
                    }
                }
                Ok(Measured {
                    shared: match changed {
                        Some(expanded) => Shared::Array(expanded.into()),
                        None => Shared::Original(node),
                    },
                    height: totals.height,
                    bytes: totals.bytes,
                })
            }
            Value::String(text) => Ok(Measured::scalar(node, text.len())),
            Value::Null | Value::Bool(_) | Value::Number(_) => Ok(Measured::scalar(node, 0)),
        }
    }

    /// Expands the members of `node`, which stays as it is in the document
    /// when none of them changes.
    fn object(
        &mut self,
        node: &'doc Value,
        members: &'doc Map<String, Value>,
        enclosing: Option<CycleId>,
        depth_left: usize,
    ) -> Result<Measured<'doc>, Error> {
        let mut totals = Totals::new();
        let mut changed: Option<Vec<(&'doc str, Shared<'doc>)>> = None;
        for (position, (name, member)) in members.iter().enumerate() {
            let child = self.value(member, enclosing, depth_left)?;
            totals.add(&child, NAME_BYTES.saturating_add(name.len()));
            if let Some(expanded) = &mut changed {
                expanded.push((name, child.shared));
            } else if !child.is(member) {
                self.charge(members.len(), size_of::<(&str, Shared<'_>)>())?;
                let mut expanded = Vec::with_capacity(members.len());
                expanded.extend(
                    members
                        .iter()
                        .take(position)
                        .map(|(name, member)| (name.as_str(), Shared::Original(member))),
                );
                expanded.push((name, child.shared));
                changed = Some(expanded);
            }
        }
        Ok(Measured {
            shared: match changed {
                Some(expanded) => Shared::Object(expanded.into()),
                None => Shared::Original(node),
            },
            height: totals.height,
            bytes: totals.bytes,
        })
    }

    fn reference(
        &mut self,
        node: &'doc Value,
        members: &'doc Map<String, Value>,
        reference: &'doc str,
        enclosing: Option<CycleId>,
        depth_left: usize,
    ) -> Result<Measured<'doc>, Error> {
        let target = self.references.target(reference)?;
        let stays = match self.options.circular {
            CircularPolicy::Ignore => target.leads_to_cycle(),
            // Two targets share a cycle only when each leads to the other, so
            // inlining this one would lead back to the target being inlined.
            CircularPolicy::Expand | CircularPolicy::Reject => {
                target.cycle().is_some() && target.cycle() == enclosing
            }
        };
        if stays {
            return match self.options.circular {
                CircularPolicy::Expand | CircularPolicy::Ignore => {
                    self.circular.insert(reference);
                    self.object(node, members, enclosing, depth_left)
                }
                CircularPolicy::Reject => Err(Error::CircularReference {
                    reference: reference.to_owned(),
                }),
            };
        }
        let mut inlined = self.inline(&target, depth_left)?;
        inlined.height = inlined.height.saturating_add(1);
        // Siblings have nothing to merge into unless the target is an object.
        if members.len() == 1 || !inlined.shared.is_object() {
            return Ok(inlined);
        }
        self.charge(members.len(), size_of::<(&str, Shared<'_>)>())?;
        let mut siblings = Vec::with_capacity(members.len());
        for (name, member) in members {
            if name == REFERENCE_KEY {
                continue;
            }
            let sibling = self.value(member, enclosing, depth_left)?;
            inlined.height = inlined.height.max(sibling.height.saturating_add(1));
            inlined.bytes = inlined
                .bytes
                .saturating_add(NAME_BYTES)
                .saturating_add(name.len())
                .saturating_add(sibling.bytes);
            siblings.push((name.as_str(), sibling.shared));
        }
        inlined.shared = self.merge(inlined.shared, siblings)?;
        Ok(inlined)
    }

    /// Lays `siblings` over `base`. Under [`SiblingMerge::Deep`] two objects
    /// under one name are merged in turn, and any other sibling replaces
    /// what `base` holds.
    fn merge(
        &mut self,
        base: Shared<'doc>,
        siblings: Vec<(&'doc str, Shared<'doc>)>,
    ) -> Result<Shared<'doc>, Error> {
        let mut overrides = Vec::with_capacity(siblings.len());
        for (name, sibling) in siblings {
            let held = match self.options.siblings {
                SiblingMerge::Deep if sibling.is_object() => {
                    // Looking a name up can mean reading every member of the
                    // base, at every reference that merges into it.
                    self.charge(base.scan(), size_of::<&str>())?;
                    base.member(name)
                }
                SiblingMerge::Deep | SiblingMerge::Shallow => None,
            };
            match held {
                Some(held) if held.is_object() => {
                    self.charge(sibling.width(), size_of::<(&str, Shared<'_>)>())?;
                    let mut inner = Vec::with_capacity(sibling.width());
                    let Ok(()) = sibling.members::<Infallible>(&mut |name, member| {
                        inner.push((name, member.clone()));
                        Ok(())
                    });
                    overrides.push((name, self.merge(held, inner)?));
                }
                Some(_) | None => overrides.push((name, sibling)),
            }
        }
        Ok(Shared::Merged(Rc::new(Merged::new(base, overrides))))
    }
}

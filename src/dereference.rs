//! Inlining of references.

use alloc::borrow::ToOwned;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::rc::Rc;
#[cfg(feature = "wasm")]
use alloc::rc::Weak;
use alloc::string::String;
use alloc::vec::Vec;
#[cfg(feature = "wasm")]
use core::cell::OnceCell;
use core::convert::Infallible;
use core::ptr;

use serde::{Serialize, Serializer};
use serde_json::{Map, Value};

use crate::documents::{Document, Documents};
use crate::error::Error;
use crate::references::{CycleId, References, Target};
#[cfg(feature = "wasm")]
use crate::shared::Back;
use crate::shared::{Anchor, Merged, Shared};
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
    /// Link a cycle back into itself where the output can hold one, as a
    /// result made of JavaScript objects can. A tree or text cannot, and
    /// gets what [`CircularPolicy::Expand`] gives.
    #[default]
    Link,
    /// Inline the cycle once, then leave the reference that would repeat it
    /// in place.
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

/// What a `$ref` with keywords next to it becomes when it points at the
/// schema `true`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(
    feature = "wasm",
    derive(serde::Deserialize, tsify::Tsify),
    serde(rename_all = "lowercase")
)]
pub enum TrueTarget {
    /// The keywords on their own, since `true` adds nothing to them.
    #[default]
    Keep,
    /// `true`, with the keywords dropped, as json-schema-ref-parser has it.
    Replace,
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
    /// What a `$ref` with keywords next to it becomes when it points at the
    /// schema `true`.
    pub true_target: TrueTarget,
    /// How deep the output may nest, and how many references a pointer may
    /// pass through in one step. A reference and the value inlined for it
    /// each count as a level.
    pub max_depth: usize,
    /// Roughly how much memory the output may take as a tree, in bytes.
    /// Writing a target out once per reference to it can make a small
    /// document describe a very large output.
    pub max_bytes: usize,
    /// How many documents the references may lead through. A document can
    /// name any number of others, each of which has to be loaded.
    pub max_documents: usize,
}

impl Options {
    /// The default for [`Options::max_depth`]. Low enough for an unoptimized
    /// build to stay inside the one mebibyte stack of a WebAssembly module.
    pub const DEFAULT_MAX_DEPTH: usize = 128;
    /// The default for [`Options::max_bytes`], 256 mebibytes.
    pub const DEFAULT_MAX_BYTES: usize = 268_435_456;
    /// The default for [`Options::max_documents`].
    pub const DEFAULT_MAX_DOCUMENTS: usize = 1024;
}

impl Default for Options {
    fn default() -> Self {
        Self {
            circular: CircularPolicy::default(),
            siblings: SiblingMerge::default(),
            true_target: TrueTarget::default(),
            max_depth: Self::DEFAULT_MAX_DEPTH,
            max_bytes: Self::DEFAULT_MAX_BYTES,
            max_documents: Self::DEFAULT_MAX_DOCUMENTS,
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
///     "name": { "type": "string" },
///     "second": { "$ref": "#/name" }
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
    let root = Document {
        root: schema,
        url: None,
    };
    let expanded = expand(&root, None, options, Form::Written)?;
    if expanded.bytes > options.max_bytes {
        return Err(Error::SizeLimitExceeded {
            limit: options.max_bytes,
        });
    }
    Ok(expanded.schema)
}

/// Expands the document `root` into its shared form, following references
/// into `documents` if there are any, and measures the memory it would take
/// written out as a tree.
pub(crate) fn expand<'doc>(
    root: &Document<'doc>,
    documents: Option<&'doc Documents>,
    options: &Options,
    form: Form,
) -> Result<Expanded<'doc>, Error> {
    let linking = match form {
        #[cfg(feature = "wasm")]
        Form::Linked => options.circular == CircularPolicy::Link,
        Form::Written => false,
    };
    let mut expansion = Expansion {
        references: References::analyze(root, documents, options)?,
        options,
        linking,
        bytes_left: options.max_bytes,
        inlined: BTreeMap::new(),
        #[cfg(feature = "wasm")]
        expanding: BTreeSet::new(),
        #[cfg(feature = "wasm")]
        links: BTreeMap::new(),
        #[cfg(feature = "wasm")]
        queued: Vec::new(),
        circular: BTreeSet::new(),
    };
    let expanded = expansion.value(root.root, root, None, options.max_depth)?;
    // A link stands in for a target until the target is expanded here, on its
    // own, however long the chain of references that led to it.
    #[cfg(feature = "wasm")]
    while let Some(target) = expansion.queued.pop() {
        expansion.inline(&target, options.max_depth)?;
    }
    Ok(Expanded {
        schema: SharedSchema {
            shared: expanded.shared,
            circular: expansion
                .circular
                .into_iter()
                .map(ToOwned::to_owned)
                .collect(),
        },
        bytes: expanded.bytes,
        #[cfg(feature = "wasm")]
        anchors: expansion
            .inlined
            .into_values()
            .filter_map(|measured| match measured.shared {
                Shared::Inlined(anchor) => Some(anchor),
                Shared::Original(_)
                | Shared::Array(_)
                | Shared::Object(_)
                | Shared::Merged(_)
                | Shared::Back(_) => None,
            })
            .collect(),
    })
}

/// A document in its shared form.
pub(crate) struct Expanded<'doc> {
    pub(crate) schema: SharedSchema<'doc>,
    /// The memory the document would take written out as a tree.
    pub(crate) bytes: usize,
    /// The expansions that the links in `schema` point at. A link does not
    /// keep its target alive, so these do, for as long as they are held.
    #[cfg(feature = "wasm")]
    pub(crate) anchors: Vec<Rc<Anchor<'doc>>>,
}

/// What the expansion is for.
#[derive(Clone, Copy)]
pub(crate) enum Form {
    /// A tree or text, which holds a copy of a target at every reference.
    Written,
    /// Objects that refer to one another, which can hold a cycle.
    #[cfg(feature = "wasm")]
    Linked,
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
            #[cfg(feature = "wasm")]
            Shared::Back(_) => false,
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
    /// Whether a reference that closes a cycle becomes a link back to the
    /// target, in place of staying as written.
    linking: bool,
    /// What the shared form may still allocate.
    bytes_left: usize,
    /// The expansion of each target inlined so far, by where the target sits
    /// in the document.
    inlined: BTreeMap<*const Value, Measured<'doc>>,
    /// The targets being expanded right now.
    #[cfg(feature = "wasm")]
    expanding: BTreeSet<*const Value>,
    /// For each target a link points at, the place its expansion goes once
    /// it is made.
    #[cfg(feature = "wasm")]
    links: BTreeMap<*const Value, Rc<OnceCell<Weak<Anchor<'doc>>>>>,
    /// The targets a link points at that are still to be expanded.
    #[cfg(feature = "wasm")]
    queued: Vec<Rc<Target<'doc>>>,
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

    /// Expands `node`, which is in the document `within`. `enclosing` is the
    /// cycle of the innermost target being inlined around it.
    fn value(
        &mut self,
        node: &'doc Value,
        within: &Document<'doc>,
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
            Some(target) if self.linking || target.cycle() == enclosing => {
                self.inline(&target, depth_left)
            }
            Some(_) | None => self.expand(node, within, enclosing, depth_left),
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
        #[cfg(feature = "wasm")]
        self.expanding.insert(address);
        let expanded = self.expand(
            target.value(),
            target.document(),
            target.cycle(),
            depth_left,
        );
        #[cfg(feature = "wasm")]
        self.expanding.remove(&address);
        let mut expanded = expanded?;
        expanded.shared = match expanded.shared {
            // A scalar is copied, and a reference to a reference shares the
            // expansion it already points at.
            Shared::Original(
                value @ (Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_)),
            ) => Shared::Original(value),
            shared @ Shared::Inlined(_) => shared,
            #[cfg(feature = "wasm")]
            shared @ Shared::Back(_) => shared,
            shared @ (Shared::Original(Value::Array(_) | Value::Object(_))
            | Shared::Array(_)
            | Shared::Object(_)
            | Shared::Merged(_)) => Shared::Inlined(Rc::new(Anchor { value: shared })),
        };
        #[cfg(feature = "wasm")]
        if let Shared::Inlined(anchor) = &expanded.shared {
            self.links
                .entry(address)
                .or_default()
                .get_or_init(|| Rc::downgrade(anchor));
        }
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
        within: &Document<'doc>,
        enclosing: Option<CycleId>,
        depth_left: usize,
    ) -> Result<Measured<'doc>, Error> {
        let Some(depth_left) = depth_left.checked_sub(1) else {
            return Err(self.too_deep());
        };
        // An id makes the value a resource of its own, which the references
        // inside it are relative to.
        let scope = self.references.scope(node).cloned();
        let within = scope.as_ref().unwrap_or(within);
        match node {
            Value::Object(members) => match reference_of(node) {
                Some(reference) => {
                    self.reference(node, members, reference, within, enclosing, depth_left)
                }
                None => self.object(node, members, within, enclosing, depth_left),
            },
            Value::Array(items) => {
                let mut totals = Totals::new();
                let mut changed: Option<Vec<Shared<'doc>>> = None;
                for (position, item) in items.iter().enumerate() {
                    let child = self.value(item, within, enclosing, depth_left)?;
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
        within: &Document<'doc>,
        enclosing: Option<CycleId>,
        depth_left: usize,
    ) -> Result<Measured<'doc>, Error> {
        let mut totals = Totals::new();
        let mut changed: Option<Vec<(&'doc str, Shared<'doc>)>> = None;
        for (position, (name, member)) in members.iter().enumerate() {
            let child = self.value(member, within, enclosing, depth_left)?;
            // A string under this name is a reference, and this one would be
            // one that was never written and never resolved.
            if name == REFERENCE_KEY
                && let Some(reference) = reference_of(member)
                && matches!(child.shared, Shared::Original(Value::String(_)))
            {
                return Err(Error::ReferenceToReference {
                    reference: reference.to_owned(),
                });
            }
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
        within: &Document<'doc>,
        enclosing: Option<CycleId>,
        depth_left: usize,
    ) -> Result<Measured<'doc>, Error> {
        let target = self.references.target(within, reference)?;
        #[cfg(feature = "wasm")]
        let target = if self.linking {
            if target.cycle().is_some() && target.cycle() == enclosing {
                self.circular.insert(reference);
            }
            let Some(target) = self.settle(target)? else {
                // The references lead only to one another, so this one stays.
                return self.object(node, members, within, enclosing, depth_left);
            };
            if members.len() == 1 {
                return Ok(self.link(node, &target));
            }
            // Keywords next to the reference merge into its target, which has
            // to be expanded first. One that is being expanded right now
            // cannot be merged into, so such a reference stays.
            if self.expanding.contains(&ptr::from_ref(target.value())) {
                return self.object(node, members, within, enclosing, depth_left);
            }
            target
        } else {
            target
        };
        let stays = !self.linking
            && match self.options.circular {
                CircularPolicy::Ignore => target.leads_to_cycle(),
                // Two targets share a cycle only when each leads to the
                // other, so inlining this one would lead back to the target
                // being inlined.
                CircularPolicy::Link | CircularPolicy::Expand | CircularPolicy::Reject => {
                    target.cycle().is_some() && target.cycle() == enclosing
                }
            };
        if stays {
            return match self.options.circular {
                CircularPolicy::Link | CircularPolicy::Expand | CircularPolicy::Ignore => {
                    self.circular.insert(reference);
                    self.object(node, members, within, enclosing, depth_left)
                }
                CircularPolicy::Reject => Err(Error::CircularReference {
                    reference: reference.to_owned(),
                }),
            };
        }
        let mut inlined = self.inline(&target, depth_left)?;
        inlined.height = inlined.height.saturating_add(1);
        if members.len() == 1 {
            return Ok(inlined);
        }
        // The keywords next to a reference apply along with its target. They
        // merge into a target that is an object and are all there is next to
        // one that accepts everything. Nothing passes a target that accepts
        // nothing, and any other value is not a schema they could apply to.
        let accepts_everything = matches!(self.options.true_target, TrueTarget::Keep)
            && matches!(inlined.shared, Shared::Original(Value::Bool(true)));
        if !accepts_everything && !inlined.shared.is_object() {
            return Ok(inlined);
        }
        self.charge(members.len(), size_of::<(&str, Shared<'_>)>())?;
        let mut siblings = Vec::with_capacity(members.len());
        for (name, member) in members {
            if name == REFERENCE_KEY {
                continue;
            }
            let sibling = self.value(member, within, enclosing, depth_left)?;
            inlined.height = inlined.height.max(sibling.height.saturating_add(1));
            inlined.bytes = inlined
                .bytes
                .saturating_add(NAME_BYTES)
                .saturating_add(name.len())
                .saturating_add(sibling.bytes);
            siblings.push((name.as_str(), sibling.shared));
        }
        inlined.shared = if accepts_everything {
            Shared::Object(siblings.into())
        } else {
            self.merge(inlined.shared, siblings, depth_left)?
        };
        Ok(inlined)
    }

    /// The target a reference to `target` reaches in the end. A reference to
    /// a bare reference is a reference to what that one points at. There is
    /// no such target when the references lead only to one another.
    #[cfg(feature = "wasm")]
    fn settle(&mut self, mut target: Rc<Target<'doc>>) -> Result<Option<Rc<Target<'doc>>>, Error> {
        let mut passed = BTreeSet::new();
        while let Some(next) = reference_of(target.value())
            && target
                .value()
                .as_object()
                .is_some_and(|bare| bare.len() == 1)
        {
            if !passed.insert(ptr::from_ref(target.value())) {
                return Ok(None);
            }
            if passed.len() > self.options.max_depth {
                return Err(self.too_deep());
            }
            let document = target.document().clone();
            target = self.references.target(&document, next)?;
        }
        Ok(Some(target))
    }

    /// Makes the reference `node` a link to `target`. The target is expanded
    /// later and on its own, so a chain of references is never followed
    /// deeper than one of its links.
    #[cfg(feature = "wasm")]
    fn link(&mut self, node: &'doc Value, target: &Rc<Target<'doc>>) -> Measured<'doc> {
        let address = ptr::from_ref(target.value());
        match target.value() {
            Value::Object(_) | Value::Array(_) => {
                if !self.links.contains_key(&address) && !self.inlined.contains_key(&address) {
                    self.queued.push(Rc::clone(target));
                }
                Measured {
                    shared: Shared::Back(Back {
                        link: Rc::clone(self.links.entry(address).or_default()),
                        written: node,
                        target: target.value(),
                    }),
                    height: 1,
                    bytes: VALUE_BYTES,
                }
            }
            Value::String(text) => Measured::scalar(target.value(), text.len()),
            Value::Null | Value::Bool(_) | Value::Number(_) => Measured::scalar(target.value(), 0),
        }
    }

    /// What a link is merged as: the expansion of its target. A target that
    /// is being expanded right now has none to give yet, so its link is
    /// merged as the reference it was written as, which still asks for
    /// everything the target does.
    #[cfg(feature = "wasm")]
    fn behind(&mut self, shared: Shared<'doc>, depth_left: usize) -> Result<Shared<'doc>, Error> {
        let Shared::Back(back) = &shared else {
            return Ok(shared);
        };
        if self.expanding.contains(&ptr::from_ref(back.target)) {
            return Ok(Shared::Original(back.written));
        }
        match self.references.at(back.target) {
            Some(target) => Ok(self.inline(&target, depth_left)?.shared),
            None => Ok(Shared::Original(back.written)),
        }
    }

    /// Lays `siblings` over `base`. Under [`SiblingMerge::Deep`] two objects
    /// under one name are merged in turn, and any other sibling replaces
    /// what `base` holds.
    fn merge(
        &mut self,
        base: Shared<'doc>,
        siblings: Vec<(&'doc str, Shared<'doc>)>,
        depth_left: usize,
    ) -> Result<Shared<'doc>, Error> {
        let mut overrides = Vec::with_capacity(siblings.len());
        for (name, sibling) in siblings {
            let held = match self.options.siblings {
                SiblingMerge::Deep if sibling.may_be_object() => {
                    // Looking a name up can mean reading every member of the
                    // base, at every reference that merges into it.
                    self.charge(base.scan(), size_of::<&str>())?;
                    base.member(name)
                }
                SiblingMerge::Deep | SiblingMerge::Shallow => None,
            };
            // Where two values meet under one name, a link gives way to what
            // it stands for, or two objects would not be seen to be objects
            // and one would replace the other.
            #[cfg(feature = "wasm")]
            let held = match held {
                Some(held) => Some(self.behind(held, depth_left)?),
                None => None,
            };
            let Some(held) = held.filter(Shared::is_object) else {
                overrides.push((name, sibling));
                continue;
            };
            #[cfg(feature = "wasm")]
            let sibling = {
                let opened = self.behind(sibling.clone(), depth_left)?;
                if !opened.is_object() {
                    overrides.push((name, sibling));
                    continue;
                }
                opened
            };
            // Each object merged into is a level, as each one expanded is.
            // Two links that lead back to themselves would merge forever.
            let Some(deeper) = depth_left.checked_sub(1) else {
                return Err(self.too_deep());
            };
            self.charge(sibling.width(), size_of::<(&str, Shared<'_>)>())?;
            let mut inner = Vec::with_capacity(sibling.width());
            let Ok(()) = sibling.members::<Infallible>(&mut |name, member| {
                inner.push((name, member.clone()));
                Ok(())
            });
            overrides.push((name, self.merge(held, inner, deeper)?));
        }
        Ok(Shared::Merged(Rc::new(Merged::new(base, overrides))))
    }
}

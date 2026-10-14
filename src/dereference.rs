//! Inlining of references.

use alloc::borrow::ToOwned;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::rc::Rc;
#[cfg(feature = "wasm")]
use alloc::rc::Weak;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
#[cfg(feature = "wasm")]
use core::cell::OnceCell;
use core::convert::Infallible;
use core::ptr;

use serde::{Serialize, Serializer};
use serde_json::{Map, Value};

use crate::documents::{Document, Documents};
use crate::error::Error;
use crate::hooks::{Hooks, NoHooks, Place, Sources, Step};
use crate::references::{CycleId, References, Target};
#[cfg(feature = "wasm")]
use crate::shared::Back;
use crate::shared::{Anchor, Merged, Shared};
use crate::walk::{REFERENCE_KEY, reference_of};

/// What a reference that cannot be resolved becomes when the run carries on.
static NULL: Value = Value::Null;

/// How many levels deep `value` nests, and roughly the memory it takes as a
/// tree, its own place in its parent included.
pub(crate) fn footprint(value: &Value) -> (usize, usize) {
    let mut height = 0;
    let mut bytes: usize = 0;
    let mut pending = vec![(value, 1_usize)];
    while let Some((current, level)) = pending.pop() {
        height = height.max(level);
        bytes = bytes.saturating_add(VALUE_BYTES);
        let deeper = level.saturating_add(1);
        match current {
            Value::Object(members) => {
                for (name, member) in members {
                    bytes = bytes.saturating_add(NAME_BYTES).saturating_add(name.len());
                    pending.push((member, deeper));
                }
            }
            Value::Array(items) => pending.extend(items.iter().map(|item| (item, deeper))),
            Value::String(text) => bytes = bytes.saturating_add(text.len()),
            Value::Null | Value::Bool(_) | Value::Number(_) => {}
        }
    }
    (height, bytes)
}

/// The memory a value takes inside its parent.
pub(crate) const VALUE_BYTES: usize = size_of::<Value>();
/// The memory an object member takes, not counting its value or the text of
/// its name.
pub(crate) const NAME_BYTES: usize = size_of::<String>();

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

/// What to do with a reference to another document.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(
    feature = "wasm",
    derive(serde::Deserialize, tsify::Tsify),
    serde(rename_all = "lowercase")
)]
pub enum ExternalPolicy {
    /// Follow it into the document it names. A document dereferenced on its
    /// own fails with [`Error::ExternalReference`].
    #[default]
    Follow,
    /// Leave it as written.
    Ignore,
}

/// Which references within a document are followed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(
    feature = "wasm",
    derive(serde::Deserialize, tsify::Tsify),
    serde(rename_all = "lowercase")
)]
pub enum LocalPolicy {
    /// Every one. A fragment that is not a JSON Pointer names the value an
    /// id gives that name, and an empty reference is the document itself.
    #[default]
    Any,
    /// Only a JSON Pointer. `#name` and an empty `$ref` are left as written,
    /// as json-schema-ref-parser leaves them, and so is a name in a
    /// reference to another document.
    Pointers,
}

/// How bundling writes a reference that stays within the bundled document.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(
    feature = "wasm",
    derive(serde::Deserialize, tsify::Tsify),
    serde(rename_all = "lowercase")
)]
pub enum InternalPolicy {
    /// Point it straight at the value it leads to in the end, past any
    /// reference on the way.
    #[default]
    Direct,
    /// Leave it as written.
    Written,
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
    /// What to do with a reference to another document.
    pub external: ExternalPolicy,
    /// Which references within a document are followed.
    pub local: LocalPolicy,
    /// The URL a reference to another document is resolved against, in place
    /// of the URL of the document it is written in.
    #[cfg_attr(feature = "wasm", tsify(optional))]
    pub external_base: Option<String>,
    /// How bundling writes a reference that stays within the bundle.
    pub internal: InternalPolicy,
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
            external: ExternalPolicy::default(),
            local: LocalPolicy::default(),
            external_base: None,
            internal: InternalPolicy::default(),
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
    dereference_schema_with_hooks(schema, options, &mut NoHooks)
}

/// [`dereference_schema_shared`], telling `hooks` what it does as it goes
/// and asking them what to leave as written.
///
/// # Errors
///
/// Fails for the reasons [`dereference_schema_with`] does, unless
/// [`Hooks::failed`] lets the run carry on, and when a hook stops it.
pub fn dereference_schema_with_hooks<'doc, H: Hooks + ?Sized>(
    schema: &'doc Value,
    options: &Options,
    hooks: &mut H,
) -> Result<SharedSchema<'doc>, Error> {
    let root = Document {
        root: schema,
        url: None,
    };
    let expanded = expand(&root, None, options, Form::Written, hooks)?;
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
pub(crate) fn expand<'doc, H: Hooks + ?Sized>(
    root: &Document<'doc>,
    documents: Option<&'doc Documents>,
    options: &Options,
    form: Form,
    hooks: &mut H,
) -> Result<Expanded<'doc>, Error> {
    let linking = match form {
        #[cfg(feature = "wasm")]
        Form::Linked => options.circular == CircularPolicy::Link,
        Form::Written => false,
    };
    let references = References::analyze(root, documents, options)?;
    let sources = Sources::new(references.admitted().to_vec());
    let mut expansion = Expansion {
        references,
        sources,
        options,
        observing: hooks.observes(),
        hooks,
        path: Vec::new(),
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
    while let Some((target, path)) = expansion.queued.pop() {
        expansion.path = path;
        expansion.inline(&target, options.max_depth, Check::Reached)?;
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

    /// `value` as the document has it, with nothing inside it expanded.
    fn original(value: &'doc Value) -> Self {
        let (height, bytes) = footprint(value);
        Self {
            shared: Shared::Original(value),
            height,
            bytes,
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

/// Whether the hooks have been asked about a target at the place it is
/// reached.
#[derive(Clone, Copy)]
enum Check {
    Done,
    Reached,
}

struct Expansion<'doc, 'run, H: Hooks + ?Sized> {
    references: References<'doc>,
    sources: Sources<'doc>,
    options: &'run Options,
    hooks: &'run mut H,
    /// Whether the hooks are told where things happen, which needs `path`.
    observing: bool,
    /// Where in the output the value being expanded goes.
    path: Vec<Step<'doc>>,
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
    /// The targets a link points at that are still to be expanded, with the
    /// place of the first link to each.
    #[cfg(feature = "wasm")]
    queued: Vec<(Rc<Target<'doc>>, Vec<Step<'doc>>)>,
    circular: BTreeSet<&'doc str>,
}

impl<'doc, H: Hooks + ?Sized> Expansion<'doc, '_, H> {
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

    fn enter(&mut self, step: Step<'doc>) {
        if self.observing {
            self.path.push(step);
        }
    }

    fn leave(&mut self) {
        if self.observing {
            self.path.pop();
        }
    }

    /// Whether the hooks want `value`, at the place being expanded, left as
    /// written.
    fn excluded(&mut self, value: &'doc Value) -> Result<bool, Error> {
        if !self.observing {
            return Ok(false);
        }
        let place = Place {
            path: &self.path,
            value,
            sources: &self.sources,
        };
        self.hooks.excluded(&place)
    }

    /// Tells the hooks that the reference `node` was replaced.
    fn tell_dereferenced(&mut self, node: &'doc Value) {
        if self.observing {
            let place = Place {
                path: &self.path,
                value: node,
                sources: &self.sources,
            };
            self.hooks.dereferenced(&place);
        }
    }

    /// Tells the hooks that the reference `node` takes part in a cycle.
    fn tell_circular(&mut self, node: &'doc Value) {
        if self.observing {
            let place = Place {
                path: &self.path,
                value: node,
                sources: &self.sources,
            };
            self.hooks.circular(&place);
        }
    }

    /// What the reference `node`, which cannot be resolved, becomes if the
    /// hooks let the run carry on.
    fn failed(&mut self, node: &'doc Value, error: Error) -> Result<Measured<'doc>, Error> {
        if !error.is_recoverable() {
            return Err(error);
        }
        let place = Place {
            path: &self.path,
            value: node,
            sources: &self.sources,
        };
        self.hooks.failed(&place, error)?;
        Ok(Measured::scalar(&NULL, 0))
    }

    /// `node` as written, which has to fit in the depth left.
    fn kept(&self, node: &'doc Value, depth_left: usize) -> Result<Measured<'doc>, Error> {
        let kept = Measured::original(node);
        if kept.height > depth_left {
            return Err(self.too_deep());
        }
        Ok(kept)
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
        if self.excluded(node)? {
            return self.kept(node, depth_left);
        }
        let target = match node {
            Value::Object(_) | Value::Array(_) => self.references.at(node),
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => None,
        };
        match target {
            // A target reached where it stands expands to what a reference
            // to it inlines, so the two share one expansion.
            Some(target) if self.linking || target.cycle() == enclosing => {
                self.inline(&target, depth_left, Check::Done)
            }
            Some(_) | None => self.expand(node, within, enclosing, depth_left),
        }
    }

    /// The expansion of `target`, made once and shared by every reference to
    /// it. A target always expands the same way: inside its own cycle.
    /// A target left as written where the hooks first see it stays so for
    /// every reference to it.
    fn inline(
        &mut self,
        target: &Target<'doc>,
        depth_left: usize,
        check: Check,
    ) -> Result<Measured<'doc>, Error> {
        let address = ptr::from_ref(target.value());
        if let Some(done) = self.inlined.get(&address) {
            return if done.height <= depth_left {
                Ok(done.clone())
            } else {
                Err(self.too_deep())
            };
        }
        let excluded = match check {
            Check::Reached => self.excluded(target.value())?,
            Check::Done => false,
        };
        let expanded = if excluded {
            self.kept(target.value(), depth_left)
        } else {
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
            expanded
        };
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
        self.hooks.proceed()?;
        let Some(depth_left) = depth_left.checked_sub(1) else {
            return Err(self.too_deep());
        };
        // An id makes the value a resource of its own, which the references
        // inside it are relative to.
        let scope = self.references.scope(node).cloned();
        let within = scope.as_ref().unwrap_or(within);
        let followed = reference_of(node).filter(|reference| self.references.follows(reference));
        match node {
            Value::Object(members) => match followed {
                Some(reference) => {
                    self.reference(node, members, reference, within, enclosing, depth_left)
                }
                None => self.object(node, members, within, enclosing, depth_left),
            },
            Value::Array(items) => {
                let mut totals = Totals::new();
                let mut changed: Option<Vec<Shared<'doc>>> = None;
                for (position, item) in items.iter().enumerate() {
                    self.enter(Step::Index(position));
                    let child = self.value(item, within, enclosing, depth_left);
                    self.leave();
                    let child = child?;
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
            self.enter(Step::Name(name));
            let child = self.value(member, within, enclosing, depth_left);
            self.leave();
            let child = child?;
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
        let target = match self.references.target(within, reference) {
            Ok(target) => target,
            Err(error) => return self.failed(node, error),
        };
        #[cfg(feature = "wasm")]
        let target = if self.linking {
            if target.cycle().is_some() && target.cycle() == enclosing {
                self.circular.insert(reference);
                self.tell_circular(node);
            }
            let target = match self.settle(target) {
                Ok(Some(target)) => target,
                // The references lead only to one another, so this one stays.
                Ok(None) => return self.object(node, members, within, enclosing, depth_left),
                Err(error) => return self.failed(node, error),
            };
            if members.len() == 1 {
                self.tell_dereferenced(node);
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
            self.tell_circular(node);
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
        let mut inlined = self.inline(&target, depth_left, Check::Reached)?;
        self.tell_dereferenced(node);
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
            self.enter(Step::Name(name));
            let sibling = self.value(member, within, enclosing, depth_left);
            self.leave();
            let sibling = sibling?;
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
            && self.references.follows(next)
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
                    let path = if self.observing {
                        self.path.clone()
                    } else {
                        Vec::new()
                    };
                    self.queued.push((Rc::clone(target), path));
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
            Some(target) => Ok(self.inline(&target, depth_left, Check::Done)?.shared),
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

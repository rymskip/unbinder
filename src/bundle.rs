//! Bundling: pulling what references to other documents point at into the
//! root document, so that every reference left points within it.
//!
//! This follows json-schema-ref-parser. Each value another document holds is
//! written once, at the first place that refers to it in the order the
//! references are ranked, and every other reference to it, or into it, points
//! there. References within the root document are pointed straight at the
//! value they lead to.

use alloc::borrow::ToOwned;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::format;
use alloc::rc::Rc;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::cmp::{Ordering, Reverse};
use core::ptr;

use percent_encoding::{AsciiSet, CONTROLS, percent_decode_str, utf8_percent_encode};
use serde_json::{Map, Value};

use crate::dereference::{
    InternalPolicy, NAME_BYTES, Options, SiblingMerge, VALUE_BYTES, footprint,
};
use crate::documents::{Document, Documents};
use crate::error::Error;
use crate::hooks::{Hooks, NoHooks, Place, Sources, Step, fragment};
use crate::references::References;
use crate::walk::{REFERENCE_KEY, reference_of};

/// What a URL may not hold in its fragment unescaped.
const FRAGMENT: &AsciiSet = &CONTROLS.add(b' ').add(b'"').add(b'<').add(b'>').add(b'`');

/// Bundles `schema` on its own: every reference in it is pointed straight at
/// the value it leads to, past any reference on the way, unless
/// [`Options::internal`] says otherwise. [`Documents::bundle`] also pulls in
/// what references to other documents point at.
///
/// ```
/// use serde_json::json;
/// use unbinder::{Options, bundle_schema};
///
/// let schema = json!({
///     "name": { "$ref": "#/alias" },
///     "alias": { "$ref": "#/definitions/name" },
///     "definitions": { "name": { "type": "string" } }
/// });
///
/// assert_eq!(
///     bundle_schema(&schema, &Options::default())?["name"],
///     json!({ "$ref": "#/definitions/name" })
/// );
/// # Ok::<(), unbinder::Error>(())
/// ```
///
/// # Errors
///
/// Fails when a reference does not resolve or points outside `schema`, and
/// when the bundle nests deeper than [`Options::max_depth`] or takes more
/// than [`Options::max_bytes`].
pub fn bundle_schema(schema: &Value, options: &Options) -> Result<Value, Error> {
    bundle_schema_with_hooks(schema, options, &mut NoHooks)
}

/// [`bundle_schema`], telling `hooks` what it does as it goes and asking
/// them what to leave as written.
///
/// # Errors
///
/// Fails for the reasons [`bundle_schema`] does, unless [`Hooks::failed`]
/// lets the run carry on, and when a hook stops it.
pub fn bundle_schema_with_hooks<H: Hooks + ?Sized>(
    schema: &Value,
    options: &Options,
    hooks: &mut H,
) -> Result<Value, Error> {
    let root = Document {
        root: schema,
        url: None,
    };
    bundle(&root, None, options, hooks)
}

/// Bundles the document `root` among `documents`.
pub(crate) fn bundle<'doc, H: Hooks + ?Sized>(
    root: &Document<'doc>,
    documents: Option<&'doc Documents>,
    options: &Options,
    hooks: &mut H,
) -> Result<Value, Error> {
    let references = References::analyze(root, documents, options)?;
    let sources = Sources::new(references.admitted().to_vec());
    let mut inventory = Inventory {
        references,
        sources,
        options,
        hooks,
        root: root.url.clone(),
        work: Vec::new(),
        seen: BTreeSet::new(),
        entries: Vec::new(),
        positions: BTreeMap::new(),
    };
    inventory.work.push(Work::Crawl {
        node: root.root,
        within: root.clone(),
        path: Vec::new(),
        indirections: 0,
        checked: Checked::No,
    });
    inventory.crawl()?;
    let mut entries: Vec<Entry<'doc>> = inventory.entries.into_iter().flatten().collect();
    entries.sort_by(rank);
    let mut builder = Builder {
        actions: remap(&entries, options)?,
        merge_keys: matches!(options.siblings, SiblingMerge::Deep),
        path: Vec::new(),
        marks: Vec::new(),
        bytes_left: options.max_bytes,
        max_bytes: options.max_bytes,
        too_deep: Error::DepthLimitExceeded {
            limit: options.max_depth,
        },
    };
    let mut bundled = builder.build(root.root, options.max_depth)?;
    if matches!(options.internal, InternalPolicy::Direct) {
        builder.marks.sort_by_key(|(order, _)| *order);
        for (_, path) in &builder.marks {
            through_references(&mut bundled, path);
        }
    }
    Ok(bundled)
}

/// A reference found in the documents, and what it leads to.
struct Entry<'doc> {
    /// The reference as written.
    node: &'doc Value,
    /// Where it is reached from the root of the bundle.
    path: Vec<Step<'doc>>,
    /// `path` written as a JSON Pointer.
    pointer: String,
    /// The URL of the document the value it leads to is in.
    file: Option<Rc<str>>,
    /// Where in that document the value is.
    hash: String,
    /// The value it leads to, past any reference on the way.
    value: &'doc Value,
    /// Whether it leads back to itself.
    circular: bool,
    /// Whether it has keywords next to `$ref`.
    extended: bool,
    /// Whether it leads out of the root document.
    external: bool,
    /// How many references were passed on the way to it.
    indirections: usize,
}

/// Whether the hooks have been asked about a value at its place.
#[derive(Clone, Copy)]
enum Checked {
    No,
    Yes,
}

enum Work<'doc> {
    /// Look through `node`.
    Crawl {
        node: &'doc Value,
        within: Document<'doc>,
        path: Vec<Step<'doc>>,
        indirections: usize,
        checked: Checked,
    },
    /// Look through the children of `node`, from the one at `next`.
    Children {
        node: &'doc Value,
        within: Document<'doc>,
        path: Vec<Step<'doc>>,
        steps: Vec<Step<'doc>>,
        next: usize,
        indirections: usize,
    },
    /// Tell the hooks about `node`, a member with a `$ref` member, once it
    /// has been looked through.
    Bundled {
        node: &'doc Value,
        path: Vec<Step<'doc>>,
    },
}

struct Inventory<'doc, 'run, H: Hooks + ?Sized> {
    references: References<'doc>,
    sources: Sources<'doc>,
    options: &'run Options,
    hooks: &'run mut H,
    /// The URL of the root document.
    root: Option<Rc<str>>,
    work: Vec<Work<'doc>>,
    /// The values looked through so far.
    seen: BTreeSet<*const Value>,
    /// The references found, in the order found. One found again closer to
    /// the root leaves a gap where it was.
    entries: Vec<Option<Entry<'doc>>>,
    /// Where each reference is in `entries`.
    positions: BTreeMap<*const Value, usize>,
}

impl<'doc, H: Hooks + ?Sized> Inventory<'doc, '_, H> {
    fn excluded(&mut self, path: &[Step<'doc>], value: &'doc Value) -> Result<bool, Error> {
        let place = Place {
            path,
            value,
            sources: &self.sources,
        };
        self.hooks.excluded(&place)
    }

    /// Whether `node` is a reference that is followed.
    fn followed(&self, node: &Value) -> bool {
        reference_of(node).is_some_and(|reference| self.references.follows(reference))
    }

    /// Looks through the documents from the root, depth first, as
    /// json-schema-ref-parser does: `definitions` and `$defs` first, then
    /// the shortest names, so that the first place to refer to a value is
    /// as close to the root as it can be.
    fn crawl(&mut self) -> Result<(), Error> {
        while let Some(work) = self.work.pop() {
            self.hooks.proceed()?;
            match work {
                Work::Crawl {
                    node,
                    within,
                    path,
                    indirections,
                    checked,
                } => {
                    if matches!(
                        node,
                        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_)
                    ) {
                        continue;
                    }
                    if path.len() > self.options.max_depth {
                        return Err(Error::DepthLimitExceeded {
                            limit: self.options.max_depth,
                        });
                    }
                    if matches!(checked, Checked::No) && self.excluded(&path, node)? {
                        continue;
                    }
                    if !self.seen.insert(ptr::from_ref(node)) {
                        continue;
                    }
                    let within = self.references.scope(node).cloned().unwrap_or(within);
                    if self.followed(node) {
                        self.inventory(node, path, &within, indirections)?;
                        continue;
                    }
                    self.work.push(Work::Children {
                        node,
                        within,
                        path,
                        steps: ordered(node),
                        next: 0,
                        indirections,
                    });
                }
                Work::Children {
                    node,
                    within,
                    path,
                    steps,
                    next,
                    indirections,
                } => self.next_child(node, within, path, steps, next, indirections)?,
                Work::Bundled { node, path } => {
                    let place = Place {
                        path: &path,
                        value: node,
                        sources: &self.sources,
                    };
                    self.hooks.bundled(&place);
                }
            }
        }
        Ok(())
    }

    /// Looks through the child of `node` at `next` among `steps`, and leaves
    /// the rest of them for later.
    fn next_child(
        &mut self,
        node: &'doc Value,
        within: Document<'doc>,
        path: Vec<Step<'doc>>,
        steps: Vec<Step<'doc>>,
        next: usize,
        indirections: usize,
    ) -> Result<(), Error> {
        let Some(step) = steps.get(next).copied() else {
            return Ok(());
        };
        let Some(child) = child(node, step) else {
            return Ok(());
        };
        let mut child_path = path.clone();
        child_path.push(step);
        self.work.push(Work::Children {
            node,
            within: within.clone(),
            path,
            steps,
            next: next.saturating_add(1),
            indirections,
        });
        if self.excluded(&child_path, child)? {
            return Ok(());
        }
        if child
            .as_object()
            .is_some_and(|members| members.contains_key(REFERENCE_KEY))
        {
            self.work.push(Work::Bundled {
                node: child,
                path: child_path.clone(),
            });
        }
        if self.followed(child) {
            let within = self.references.scope(child).cloned().unwrap_or(within);
            self.inventory(child, child_path, &within, indirections)?;
        } else {
            self.work.push(Work::Crawl {
                node: child,
                within,
                path: child_path,
                indirections,
                checked: Checked::Yes,
            });
        }
        Ok(())
    }

    /// Records the reference `node`, reached at `path`, and looks through
    /// what it leads to.
    fn inventory(
        &mut self,
        node: &'doc Value,
        path: Vec<Step<'doc>>,
        within: &Document<'doc>,
        indirections: usize,
    ) -> Result<(), Error> {
        let Some(reference) = reference_of(node) else {
            return Ok(());
        };
        let settled = match self.references.target(within, reference) {
            Ok(target) => self.references.settle(target),
            Err(error) => Err(error),
        };
        let (target, hops, circular) = match settled {
            Ok(settled) => settled,
            Err(error) if error.is_recoverable() => {
                let place = Place {
                    path: &path,
                    value: node,
                    sources: &self.sources,
                };
                return self.hooks.failed(&place, error);
            }
            Err(error) => return Err(error),
        };
        let source = self.sources.of(target.value());
        let file: Option<Rc<str>> = source.url.map(Into::into);
        let external = file != self.root;
        let indirections = indirections.saturating_add(hops);
        let address = ptr::from_ref(node);
        let existed = match self.positions.get(&address).copied() {
            Some(position) => {
                let Some(slot) = self.entries.get_mut(position) else {
                    return Ok(());
                };
                match slot {
                    Some(existing)
                        if path.len() < existing.path.len()
                            || indirections < existing.indirections =>
                    {
                        *slot = None;
                    }
                    Some(_) | None => return Ok(()),
                }
                true
            }
            None => false,
        };
        self.positions.insert(address, self.entries.len());
        self.entries.push(Some(Entry {
            node,
            pointer: fragment(&path),
            path: path.clone(),
            file,
            hash: hash(&source.path),
            value: target.value(),
            circular,
            extended: node.as_object().is_some_and(|members| members.len() > 1),
            external,
            indirections,
        }));
        if !existed || external {
            self.work.push(Work::Crawl {
                node: target.value(),
                within: target.document().clone(),
                path,
                indirections: indirections.saturating_add(1),
                checked: Checked::No,
            });
        }
        Ok(())
    }
}

/// The children of `node` in the order a bundle looks through them.
fn ordered(node: &Value) -> Vec<Step<'_>> {
    match node {
        Value::Object(members) => {
            let mut names: Vec<&str> = members.keys().map(String::as_str).collect();
            names.sort_by_key(|name| {
                (
                    *name != "definitions" && *name != "$defs",
                    name.encode_utf16().count(),
                )
            });
            names.into_iter().map(Step::Name).collect()
        }
        Value::Array(items) => (0..items.len()).map(Step::Index).collect(),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => Vec::new(),
    }
}

fn child<'doc>(node: &'doc Value, step: Step<'_>) -> Option<&'doc Value> {
    match step {
        Step::Name(name) => node.as_object()?.get(name),
        Step::Index(position) => node.as_array()?.get(position),
    }
}

/// The order in which references claim the value they lead to: grouped by
/// that value, then a reference that leads back to itself, a bare one, a
/// direct one, the one closest to the root, the one closest to a
/// `definitions` member, and the shortest.
fn rank(first: &Entry<'_>, second: &Entry<'_>) -> Ordering {
    first
        .file
        .cmp(&second.file)
        .then_with(|| first.hash.cmp(&second.hash))
        .then_with(|| second.circular.cmp(&first.circular))
        .then_with(|| first.extended.cmp(&second.extended))
        .then_with(|| first.indirections.cmp(&second.indirections))
        .then_with(|| first.path.len().cmp(&second.path.len()))
        .then_with(|| {
            Reverse(definitions(&first.pointer)).cmp(&Reverse(definitions(&second.pointer)))
        })
        .then_with(|| {
            first
                .pointer
                .encode_utf16()
                .count()
                .cmp(&second.pointer.encode_utf16().count())
        })
}

/// Where the last `definitions` or `$defs` member in `pointer` starts.
fn definitions(pointer: &str) -> Option<usize> {
    pointer.rfind("/definitions").max(pointer.rfind("/$defs"))
}

/// What becomes of a reference in the bundle.
#[derive(Clone)]
enum Action<'doc> {
    /// It points at `reference`.
    Rewrite { order: usize, reference: String },
    /// The value it leads to takes its place. One that leads back to itself
    /// points at where it now is.
    Inline {
        order: usize,
        value: &'doc Value,
        circular: Option<String>,
    },
}

/// Decides what becomes of each reference, given in rank order.
fn remap<'doc>(
    entries: &[Entry<'doc>],
    options: &Options,
) -> Result<BTreeMap<*const Value, Action<'doc>>, Error> {
    let mut actions = BTreeMap::new();
    // The value the last reference to be inlined led to, and where it went.
    let mut group: Option<(&Option<Rc<str>>, &str, &str)> = None;
    for (order, entry) in entries.iter().enumerate() {
        let action = if entry.external {
            match group {
                Some((file, hash, pointer)) if *file == entry.file && hash == entry.hash => {
                    Some(Action::Rewrite {
                        order,
                        reference: pointer.to_owned(),
                    })
                }
                Some((file, hash, pointer))
                    if *file == entry.file && entry.hash.starts_with(&format!("{hash}/")) =>
                {
                    let below = entry.hash.get(hash.len()..).unwrap_or_default();
                    Some(Action::Rewrite {
                        order,
                        reference: join(pointer, &tokens(&format!("#{below}"))?),
                    })
                }
                Some(_) | None => {
                    group = Some((&entry.file, &entry.hash, &entry.pointer));
                    Some(Action::Inline {
                        order,
                        value: entry.value,
                        circular: entry.circular.then(|| entry.pointer.clone()),
                    })
                }
            }
        } else {
            match options.internal {
                InternalPolicy::Direct => Some(Action::Rewrite {
                    order,
                    reference: entry.hash.clone(),
                }),
                InternalPolicy::Written => None,
            }
        };
        if let Some(action) = action {
            actions.insert(ptr::from_ref(entry.node), action);
        }
    }
    Ok(actions)
}

/// Writes the bundle out.
struct Builder<'doc> {
    actions: BTreeMap<*const Value, Action<'doc>>,
    merge_keys: bool,
    /// Where in the bundle the value being written goes.
    path: Vec<Step<'doc>>,
    /// Where each reference that was rewritten ended up, by rank.
    marks: Vec<(usize, Vec<Step<'doc>>)>,
    /// What the bundle may still take. A value can be written at more than
    /// one place, so a small document can describe a very large bundle.
    bytes_left: usize,
    max_bytes: usize,
    too_deep: Error,
}

impl<'doc> Builder<'doc> {
    /// Takes `bytes` out of what the bundle may take, before they are
    /// allocated.
    fn charge(&mut self, bytes: usize) -> Result<(), Error> {
        self.bytes_left = self
            .bytes_left
            .checked_sub(bytes)
            .ok_or(Error::SizeLimitExceeded {
                limit: self.max_bytes,
            })?;
        Ok(())
    }

    fn build(&mut self, node: &'doc Value, depth_left: usize) -> Result<Value, Error> {
        let Some(depth_left) = depth_left.checked_sub(1) else {
            return Err(self.too_deep.clone());
        };
        match self.actions.get(&ptr::from_ref(node)).cloned() {
            Some(Action::Rewrite { order, reference }) => {
                self.charge(footprint(node).1.saturating_add(reference.len()))?;
                let mut written = node.clone();
                if let Some(members) = written.as_object_mut() {
                    members.insert(REFERENCE_KEY.to_owned(), Value::String(reference));
                }
                self.marks.push((order, self.path.clone()));
                return Ok(written);
            }
            Some(Action::Inline {
                order,
                value,
                circular,
            }) => {
                // The keywords next to the reference are copied as written.
                self.charge(footprint(node).1)?;
                // A reference that leads back to itself is not written out
                // again inside itself.
                let built = match circular {
                    Some(_) => {
                        self.charge(footprint(value).1)?;
                        value.clone()
                    }
                    None => self.build(value, depth_left)?,
                };
                let mut inlined = merged(node, built, self.merge_keys);
                if let (Some(pointer), Some(members)) = (circular, inlined.as_object_mut()) {
                    members.insert(REFERENCE_KEY.to_owned(), Value::String(pointer));
                }
                if reference_of(&inlined).is_some() {
                    self.marks.push((order, self.path.clone()));
                }
                return Ok(inlined);
            }
            None => {}
        }
        match node {
            Value::Object(members) => {
                self.charge(VALUE_BYTES)?;
                let mut written = Map::new();
                for (name, member) in members {
                    self.charge(NAME_BYTES.saturating_add(name.len()))?;
                    self.path.push(Step::Name(name));
                    let built = self.build(member, depth_left);
                    self.path.pop();
                    written.insert(name.clone(), built?);
                }
                Ok(Value::Object(written))
            }
            Value::Array(items) => {
                self.charge(VALUE_BYTES)?;
                let mut written = Vec::with_capacity(items.len());
                for (position, item) in items.iter().enumerate() {
                    self.path.push(Step::Index(position));
                    let built = self.build(item, depth_left);
                    self.path.pop();
                    written.push(built?);
                }
                Ok(Value::Array(written))
            }
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {
                self.charge(footprint(node).1)?;
                Ok(node.clone())
            }
        }
    }
}

/// What a reference with keywords next to it becomes in place of its
/// target, as json-schema-ref-parser makes it: the keywords, then each
/// member of the target they do not have, with an object under a name both
/// have merged into the target's. A bare reference becomes the target.
fn merged(node: &Value, target: Value, merge_keys: bool) -> Value {
    let Some(written) = node.as_object().filter(|members| members.len() > 1) else {
        return target;
    };
    let target_members: Vec<(String, Value)> = match target {
        Value::Object(members) => members.into_iter().collect(),
        Value::Array(items) => items
            .into_iter()
            .enumerate()
            .map(|(position, item)| (position.to_string(), item))
            .collect(),
        scalar @ (Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_)) => {
            return scalar;
        }
    };
    let mut members: Map<String, Value> = written
        .iter()
        .filter(|(name, _)| *name != REFERENCE_KEY)
        .map(|(name, member)| (name.clone(), member.clone()))
        .collect();
    for (name, member) in target_members {
        match members.get_mut(&name) {
            None => {
                members.insert(name, member);
            }
            Some(sibling) if merge_keys && is_container(sibling) && is_container(&member) => {
                *sibling = deep_merge(member, sibling);
            }
            Some(_) => {}
        }
    }
    Value::Object(members)
}

/// Whether JavaScript would call `value` an object.
const fn is_container(value: &Value) -> bool {
    matches!(value, Value::Object(_) | Value::Array(_))
}

/// `source` laid over `target`: objects merge member by member, and
/// anything else in `source` replaces what `target` has.
fn deep_merge(target: Value, source: &Value) -> Value {
    let (Value::Object(mut output), Value::Object(laid)) = (target, source) else {
        return source.clone();
    };
    for (name, member) in laid {
        let merged = match member {
            Value::Object(_) => match output.remove(name) {
                Some(held) => deep_merge(held, member),
                None => member.clone(),
            },
            Value::Null
            | Value::Bool(_)
            | Value::Number(_)
            | Value::String(_)
            | Value::Array(_) => member.clone(),
        };
        output.insert(name.clone(), merged);
    }
    Value::Object(output)
}

/// A step written as a JSON Pointer token in a URI fragment, the way
/// json-schema-ref-parser joins one onto a path.
fn escape(step: Step<'_>) -> String {
    match step {
        Step::Name(name) => name
            .replace('%', "%25")
            .replace('~', "~0")
            .replace('/', "~1"),
        Step::Index(position) => position.to_string(),
    }
}

/// `path` as the fragment of a URL, which escapes what a URL may not hold.
fn hash(path: &[Step<'_>]) -> String {
    let mut written = String::from("#");
    for step in path {
        written.push('/');
        written.extend(utf8_percent_encode(&escape(*step), FRAGMENT));
    }
    written
}

/// `base` with `tokens` joined on.
fn join(base: &str, tokens: &[String]) -> String {
    let mut written = String::from(base);
    if !written.contains('#') {
        written.push('#');
    }
    for token in tokens {
        written.push('/');
        written.push_str(&escape(Step::Name(token)));
    }
    written
}

/// The fragment of `reference` with its percent-encoding removed, where it
/// decodes.
fn decoded(reference: &str) -> String {
    let fragment = reference
        .split_once('#')
        .map_or("", |(_, fragment)| fragment);
    match percent_decode_str(fragment).decode_utf8() {
        Ok(decoded) => decoded.into_owned(),
        Err(_) => fragment.to_owned(),
    }
}

/// The tokens of the JSON Pointer in the fragment of `reference`.
fn tokens(reference: &str) -> Result<Vec<String>, Error> {
    let pointer = decoded(reference);
    if pointer.is_empty() {
        return Ok(Vec::new());
    }
    let Some(path) = pointer.strip_prefix('/') else {
        return Err(Error::UnresolvedReference {
            reference: reference.to_owned(),
            cause: crate::error::PointerError::MissingLeadingSlash,
        });
    };
    Ok(path
        .split('/')
        .map(|token| token.replace("~1", "/").replace("~0", "~"))
        .collect())
}

/// Whether `reference` is a JSON Pointer into the bundle itself.
fn is_internal(reference: &str) -> bool {
    reference.starts_with('#') && decoded(reference).starts_with('/')
}

/// The member of `value` a token names, as a JavaScript property lookup
/// finds it.
fn member<'value>(value: &'value Value, token: &str) -> Option<&'value Value> {
    match value {
        Value::Object(members) => members.get(token),
        Value::Array(items) => match token.parse::<usize>() {
            Ok(position) if token == "0" || !token.starts_with(['0', '+']) => items.get(position),
            Ok(_) | Err(_) => None,
        },
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => None,
    }
}

/// The value at the JSON Pointer `reference` in `schema`.
fn walk<'value>(schema: &'value Value, reference: &str) -> Option<&'value Value> {
    if !is_internal(reference) {
        return None;
    }
    let Ok(steps) = tokens(reference) else {
        return None;
    };
    steps
        .iter()
        .try_fold(schema, |current, token| member(current, token))
}

/// `reference` rewritten so that it does not pass through another
/// reference, which a JSON Pointer cannot do.
fn direct(schema: &Value, reference: &str) -> String {
    let Ok(steps) = tokens(reference) else {
        return reference.to_owned();
    };
    let mut current = Some(schema);
    let mut resolved: Vec<String> = Vec::new();
    for token in steps {
        let Some(at) = current.filter(|at| is_container(at)) else {
            return reference.to_owned();
        };
        let mut at = at;
        // A keyword next to the reference is reached directly.
        if member(at, &token).is_none()
            && let Some(through) = reference_of(at).filter(|through| is_internal(through))
        {
            let Ok(restarted) = tokens(through) else {
                return reference.to_owned();
            };
            resolved = restarted;
            match walk(schema, through).filter(|target| is_container(target)) {
                Some(target) => at = target,
                None => return reference.to_owned(),
            }
        }
        current = member(at, &token);
        resolved.push(token);
    }
    join("#", &resolved)
}

/// Points the reference at `path` in `bundled` past any reference its
/// pointer passes through.
fn through_references(bundled: &mut Value, path: &[Step<'_>]) {
    let Some(reference) = at(bundled, path)
        .and_then(reference_of)
        .filter(|reference| is_internal(reference))
    else {
        return;
    };
    let fixed = direct(bundled, reference);
    if let Some(members) = at_mut(bundled, path).and_then(Value::as_object_mut) {
        members.insert(REFERENCE_KEY.to_owned(), Value::String(fixed));
    }
}

/// The value at `path` in a bundle, where an array merged into an object
/// is found under the names of its positions.
fn at<'value>(value: &'value Value, path: &[Step<'_>]) -> Option<&'value Value> {
    path.iter()
        .try_fold(value, |current, step| match (current, step) {
            (Value::Object(members), Step::Name(name)) => members.get(*name),
            (Value::Object(members), Step::Index(position)) => members.get(&position.to_string()),
            (Value::Array(items), Step::Index(position)) => items.get(*position),
            _ => None,
        })
}

fn at_mut<'value>(value: &'value mut Value, path: &[Step<'_>]) -> Option<&'value mut Value> {
    path.iter()
        .try_fold(value, |current, step| match (current, step) {
            (Value::Object(members), Step::Name(name)) => members.get_mut(*name),
            (Value::Object(members), Step::Index(position)) => {
                members.get_mut(&position.to_string())
            }
            (Value::Array(items), Step::Index(position)) => items.get_mut(*position),
            _ => None,
        })
}

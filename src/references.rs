//! The graph formed by the references of a document and of the documents it
//! refers to, and the cycles in it.
//!
//! The nodes are the values that references point at. An edge runs from a
//! target to every target referenced inside it, and to every target nested
//! directly inside it, since inlining the outer one copies the inner one's
//! references too. The graph is built in one pass over each document, so the
//! analysis stays proportional to the documents however their targets nest.

use alloc::borrow::ToOwned;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::format;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec;
use alloc::vec::{IntoIter, Vec};
use core::cell::Cell;
use core::mem;
use core::ptr;

use serde_json::Value;

use crate::dereference::Options;
use crate::documents::{Document, Documents, absolute, split, survey};
use crate::error::{Error, PointerError};
use crate::pointer::{self, Slashed};
use crate::walk::reference_of;

/// Identifies a group of targets that can all reach one another.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CycleId(usize);

#[derive(Clone, Copy, Debug)]
enum Visit {
    Unvisited,
    /// On the search path, at this position in it.
    OnPath {
        position: usize,
    },
    Finished,
}

/// The value a reference points at.
#[derive(Debug)]
pub(crate) struct Target<'doc> {
    value: &'doc Value,
    document: Document<'doc>,
    cycle: Cell<Option<CycleId>>,
    leads_to_cycle: Cell<bool>,
    visit: Cell<Visit>,
}

impl<'doc> Target<'doc> {
    pub(crate) fn value(&self) -> &'doc Value {
        self.value
    }

    /// The resource the value is in, which its own references are relative
    /// to.
    pub(crate) fn document(&self) -> &Document<'doc> {
        &self.document
    }

    /// The cycle this target takes part in, if any.
    pub(crate) fn cycle(&self) -> Option<CycleId> {
        self.cycle.get()
    }

    /// Whether inlining this target would reach a cycle, its own or one
    /// further in.
    pub(crate) fn leads_to_cycle(&self) -> bool {
        self.leads_to_cycle.get()
    }
}

/// A reference as it is told apart from others: by the document it is
/// written in and by its text.
type Written<'doc> = (*const Value, &'doc str);

fn written<'doc>(within: &Document<'doc>, reference: &'doc str) -> Written<'doc> {
    (ptr::from_ref(within.root), reference)
}

/// A pointer partway through being followed.
struct Walk<'doc> {
    key: Written<'doc>,
    current: &'doc Value,
    /// The resource `current` is in.
    within: Document<'doc>,
    tokens: Vec<String>,
    /// How many of the tokens have been applied.
    applied: usize,
    /// The references passed through since the last token was applied.
    passed: BTreeSet<*const Value>,
}

impl Walk<'_> {
    fn unresolved(&self, cause: PointerError) -> Error {
        Error::UnresolvedReference {
            reference: self.key.1.to_owned(),
            cause,
        }
    }
}

/// The targets each target leads to, keyed by where the target sits in its
/// document.
type Successors<'doc> = BTreeMap<*const Value, Vec<Rc<Target<'doc>>>>;

/// A step of the walk that builds [`Successors`].
enum Step<'doc> {
    Enter(&'doc Value),
    /// Every value inside the innermost open target has been entered.
    LeaveTarget,
    /// Every value inside the innermost open resource has been entered.
    LeaveResource,
}

/// A target whose successors are still being followed.
struct Frame<'doc> {
    target: Rc<Target<'doc>>,
    position: usize,
    /// The lowest path position reachable from this target.
    lowest: usize,
    successors: IntoIter<Rc<Target<'doc>>>,
    leads_to_itself: bool,
    leads_to_cycle: bool,
}

/// Every reference a document leads to, resolved and grouped into cycles.
#[derive(Debug)]
pub(crate) struct References<'doc> {
    /// The other documents references may point into, if any were given.
    documents: Option<&'doc Documents>,
    by_reference: BTreeMap<Written<'doc>, Rc<Target<'doc>>>,
    /// Two spellings of one pointer share a target, so targets are told
    /// apart by the address of the value they point at.
    by_address: BTreeMap<*const Value, Rc<Target<'doc>>>,
    /// The resources ids set up inside the documents, by URL.
    resources: BTreeMap<String, Document<'doc>>,
    /// The values ids name by a fragment, by full URL.
    names: BTreeMap<String, &'doc Value>,
    /// The resource each value that starts one is the root of.
    scopes: BTreeMap<*const Value, Document<'doc>>,
    /// The names that hold a `/`, for each object a pointer looked for one
    /// in.
    slashed: BTreeMap<*const Value, Slashed<'doc>>,
    /// The roots of the documents looked through so far.
    admitted: Vec<&'doc Value>,
    /// References found in an admitted document and not yet resolved.
    unresolved: Vec<(&'doc str, Document<'doc>)>,
    cycles: usize,
    /// How many references a pointer may pass through between two tokens.
    max_hops: usize,
    max_documents: usize,
}

impl<'doc> References<'doc> {
    /// Resolves every reference `root` leads to, in it and in the documents
    /// it refers to, and finds the cycles among them.
    pub(crate) fn analyze(
        root: &Document<'doc>,
        documents: Option<&'doc Documents>,
        options: &Options,
    ) -> Result<Self, Error> {
        let mut references = Self {
            documents,
            by_reference: BTreeMap::new(),
            by_address: BTreeMap::new(),
            resources: BTreeMap::new(),
            names: BTreeMap::new(),
            scopes: BTreeMap::new(),
            slashed: BTreeMap::new(),
            admitted: Vec::new(),
            unresolved: Vec::new(),
            cycles: 0,
            max_hops: options.max_depth,
            max_documents: options.max_documents,
        };
        references.admit(root)?;
        while let Some((reference, within)) = references.unresolved.pop() {
            references.target(&within, reference)?;
        }
        let mut successors = Successors::new();
        for admitted in references.admitted.clone() {
            references.successors(admitted, &mut successors);
        }
        let starts: Vec<_> = references.by_address.values().map(Rc::clone).collect();
        for start in starts {
            references.search(start, &mut successors);
        }
        Ok(references)
    }

    /// Looks through `document`, once: notes the resources and names its ids
    /// set up, and queues its references to be resolved.
    fn admit(&mut self, document: &Document<'doc>) -> Result<(), Error> {
        if self
            .admitted
            .iter()
            .any(|admitted| ptr::eq(*admitted, document.root))
        {
            return Ok(());
        }
        if self.admitted.len() >= self.max_documents {
            return Err(Error::DocumentLimitExceeded {
                limit: self.max_documents,
            });
        }
        self.admitted.push(document.root);
        let surveyed = survey(document);
        for resource in surveyed.resources {
            self.scopes
                .insert(ptr::from_ref(resource.root), resource.clone());
            if let Some(url) = &resource.url {
                self.resources.insert(url.as_ref().to_owned(), resource);
            }
        }
        self.names.extend(surveyed.names);
        self.unresolved.extend(surveyed.references);
        Ok(())
    }

    /// The target that `value` is, if any reference points at it.
    pub(crate) fn at(&self, value: &Value) -> Option<Rc<Target<'doc>>> {
        self.by_address.get(&ptr::from_ref(value)).map(Rc::clone)
    }

    /// The resource `value` is the root of, if an id makes it one.
    pub(crate) fn scope(&self, value: &Value) -> Option<&Document<'doc>> {
        self.scopes.get(&ptr::from_ref(value))
    }

    /// The member of `current` that the tokens ahead name, and how many of
    /// them name it. A name that holds a `/` may be spelled by several.
    fn member(&mut self, current: &'doc Value, ahead: &[String]) -> Option<(&'doc Value, usize)> {
        let members = current.as_object()?;
        if let Some(member) = members.get(ahead.first()?) {
            return Some((member, 1));
        }
        if ahead.len() < 2 {
            return None;
        }
        let (name, count) = self
            .slashed
            .entry(ptr::from_ref(current))
            .or_insert_with(|| Slashed::of(members))
            .longest(ahead)?;
        Some((members.get(name)?, count))
    }

    /// The target of `reference` as written in the resource `within`,
    /// resolved on first use.
    ///
    /// A pointer that reaches a reference with tokens still to apply passes
    /// through it and carries on from that reference's target. Pointers
    /// waiting on one another are kept on an explicit stack, so a long chain
    /// of them cannot exhaust the call stack.
    pub(crate) fn target(
        &mut self,
        within: &Document<'doc>,
        reference: &'doc str,
    ) -> Result<Rc<Target<'doc>>, Error> {
        if let Some(known) = self.by_reference.get(&written(within, reference)) {
            return Ok(Rc::clone(known));
        }
        let mut active = BTreeSet::new();
        let mut waiting = Vec::new();
        let mut walk = self.start(within, reference, &mut active)?;
        loop {
            let ahead = walk.tokens.get(walk.applied..).unwrap_or_default();
            let Some(token) = ahead.first() else {
                let target = self.register(&walk);
                active.remove(&walk.key);
                let Some(resumed) = waiting.pop() else {
                    return Ok(target);
                };
                walk = resumed;
                walk.current = target.value;
                walk.within = target.document.clone();
                continue;
            };
            let found = self.member(walk.current, ahead);
            // A reference's own members come first, as they do once its
            // target is merged into it. The pointer passes through it for
            // any other.
            let through = match found {
                Some(_) => None,
                None => reference_of(walk.current),
            };
            let Some(through) = through else {
                let (next, count) = match found {
                    Some(found) => found,
                    None => (
                        pointer::descend(walk.current, token)
                            .map_err(|cause| walk.unresolved(cause))?,
                        1,
                    ),
                };
                walk.current = next;
                if let Some(scope) = self.scope(walk.current) {
                    walk.within = scope.clone();
                }
                walk.applied = walk.applied.saturating_add(count);
                walk.passed.clear();
                continue;
            };
            if walk.passed.len() >= self.max_hops {
                return Err(Error::DepthLimitExceeded {
                    limit: self.max_hops,
                });
            }
            let key = written(&walk.within, through);
            if !walk.passed.insert(ptr::from_ref(walk.current)) || active.contains(&key) {
                return Err(walk.unresolved(PointerError::ReferenceLoop));
            }
            if let Some(known) = self.by_reference.get(&key) {
                walk.current = known.value;
                walk.within = known.document.clone();
            } else {
                let from = walk.within.clone();
                let next = self.start(&from, through, &mut active)?;
                waiting.push(mem::replace(&mut walk, next));
            }
        }
    }

    /// The resource a reference written in `within` points into, when it
    /// names one by a URL.
    fn resource(
        &mut self,
        within: &Document<'doc>,
        reference: &str,
        location: &str,
    ) -> Result<Document<'doc>, Error> {
        let external = || Error::ExternalReference {
            reference: reference.to_owned(),
        };
        let url = match absolute(within.url.as_deref(), location) {
            Ok(url) => url,
            // With no other documents to go by, a relative URL is simply a
            // reference out of this one.
            Err(invalid) => {
                return Err(match self.documents {
                    Some(_) => invalid,
                    None => external(),
                });
            }
        };
        if let Some(resource) = self.resources.get(&url) {
            return Ok(resource.clone());
        }
        let Some(documents) = self.documents else {
            return Err(external());
        };
        if let Some(document) = documents.get(&url) {
            self.admit(&document)?;
            // An id on its root may give the document another URL.
            return Ok(self.scope(document.root).cloned().unwrap_or(document));
        }
        if let Some(holder) = documents.holding(&url) {
            self.admit(&holder)?;
            if let Some(resource) = self.resources.get(&url) {
                return Ok(resource.clone());
            }
        }
        Err(Error::MissingDocument { url })
    }

    fn start(
        &mut self,
        within: &Document<'doc>,
        reference: &'doc str,
        active: &mut BTreeSet<Written<'doc>>,
    ) -> Result<Walk<'doc>, Error> {
        let (location, fragment) = split(reference);
        let resource = if location.is_empty() {
            within.clone()
        } else {
            self.resource(within, reference, location)?
        };
        // A fragment that is not a pointer may be a name an id gave a value.
        let named = match &resource.url {
            Some(url) if !fragment.is_empty() && !fragment.starts_with('/') => {
                self.names.get(&format!("{url}#{fragment}")).copied()
            }
            Some(_) | None => None,
        };
        let (current, tokens) = match named {
            Some(value) => (value, Vec::new()),
            None => (
                resource.root,
                pointer::tokens(fragment).map_err(|cause| Error::UnresolvedReference {
                    reference: reference.to_owned(),
                    cause,
                })?,
            ),
        };
        let key = written(within, reference);
        active.insert(key);
        Ok(Walk {
            key,
            current,
            within: self.scope(current).cloned().unwrap_or(resource),
            tokens,
            applied: 0,
            passed: BTreeSet::new(),
        })
    }

    /// Records where a finished walk ended up.
    fn register(&mut self, walk: &Walk<'doc>) -> Rc<Target<'doc>> {
        let target = Rc::clone(
            self.by_address
                .entry(ptr::from_ref(walk.current))
                .or_insert_with(|| {
                    Rc::new(Target {
                        value: walk.current,
                        document: walk.within.clone(),
                        cycle: Cell::new(None),
                        leads_to_cycle: Cell::new(false),
                        visit: Cell::new(Visit::Unvisited),
                    })
                }),
        );
        self.by_reference.insert(walk.key, Rc::clone(&target));
        target
    }

    /// Walks a document once, giving each reference and each nested target
    /// to the innermost target that encloses it.
    fn successors(&self, root: &'doc Value, successors: &mut Successors<'doc>) {
        let mut open = Vec::new();
        // The roots of the resources the walk is inside, innermost last.
        let mut within = vec![ptr::from_ref(root)];
        let mut steps = vec![Step::Enter(root)];
        while let Some(step) = steps.pop() {
            let value = match step {
                Step::Enter(value) => value,
                Step::LeaveTarget => {
                    open.pop();
                    continue;
                }
                Step::LeaveResource => {
                    within.pop();
                    continue;
                }
            };
            let address = ptr::from_ref(value);
            if self.scopes.contains_key(&address) && !ptr::eq(value, root) {
                within.push(address);
                steps.push(Step::LeaveResource);
            }
            if let Some(target) = self.by_address.get(&address) {
                if let Some(enclosing) = open.last() {
                    successors
                        .entry(*enclosing)
                        .or_default()
                        .push(Rc::clone(target));
                }
                open.push(address);
                steps.push(Step::LeaveTarget);
            }
            let referenced = reference_of(value)
                .zip(within.last())
                .and_then(|(reference, resource)| self.by_reference.get(&(*resource, reference)));
            if let (Some(enclosing), Some(referenced)) = (open.last(), referenced) {
                successors
                    .entry(*enclosing)
                    .or_default()
                    .push(Rc::clone(referenced));
            }
            match value {
                Value::Object(members) => steps.extend(members.values().map(Step::Enter)),
                Value::Array(items) => steps.extend(items.iter().map(Step::Enter)),
                Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
            }
        }
    }

    /// Tarjan's strongly connected components algorithm, with an explicit
    /// stack so a long chain of references cannot exhaust the call stack.
    fn search(&mut self, start: Rc<Target<'doc>>, successors: &mut Successors<'doc>) {
        let mut path = Vec::new();
        let mut frames = Vec::new();
        if matches!(start.visit.get(), Visit::Unvisited) {
            frames.push(Self::enter(start, &mut path, successors));
        }
        while let Some(mut frame) = frames.pop() {
            let Some(successor) = frame.successors.next() else {
                let leads_to_cycle = self.leave(&frame, &mut path);
                if let Some(parent) = frames.last_mut() {
                    parent.lowest = parent.lowest.min(frame.lowest);
                    parent.leads_to_cycle |= leads_to_cycle;
                }
                continue;
            };
            frame.leads_to_itself |= Rc::ptr_eq(&successor, &frame.target);
            frame.leads_to_cycle |= successor.leads_to_cycle.get();
            match successor.visit.get() {
                Visit::Unvisited => {
                    frames.push(frame);
                    frames.push(Self::enter(successor, &mut path, successors));
                }
                Visit::OnPath { position } => {
                    frame.lowest = frame.lowest.min(position);
                    frames.push(frame);
                }
                Visit::Finished => frames.push(frame),
            }
        }
    }

    fn enter(
        target: Rc<Target<'doc>>,
        path: &mut Vec<Rc<Target<'doc>>>,
        successors: &mut Successors<'doc>,
    ) -> Frame<'doc> {
        let position = path.len();
        target.visit.set(Visit::OnPath { position });
        path.push(Rc::clone(&target));
        // A target is entered once, so its successors can be taken.
        let successors = successors
            .remove(&ptr::from_ref(target.value))
            .unwrap_or_default()
            .into_iter();
        Frame {
            target,
            position,
            lowest: position,
            successors,
            leads_to_itself: false,
            leads_to_cycle: false,
        }
    }

    /// Closes the component that `frame` is the root of, if it is one.
    /// Returns whether the frame's target leads to a cycle.
    fn leave(&mut self, frame: &Frame<'doc>, path: &mut Vec<Rc<Target<'doc>>>) -> bool {
        if frame.lowest != frame.position {
            // The target shares a component with one further up the path.
            return true;
        }
        let mut members = Vec::new();
        while let Some(member) = path.pop() {
            let is_root = Rc::ptr_eq(&member, &frame.target);
            members.push(member);
            if is_root {
                break;
            }
        }
        let cycle = (members.len() > 1 || frame.leads_to_itself).then(|| self.next_cycle());
        let leads_to_cycle = cycle.is_some() || frame.leads_to_cycle;
        for member in members {
            member.cycle.set(cycle);
            member.leads_to_cycle.set(leads_to_cycle);
            member.visit.set(Visit::Finished);
        }
        leads_to_cycle
    }

    fn next_cycle(&mut self) -> CycleId {
        let cycle = CycleId(self.cycles);
        // Every cycle holds at least one target, so the count cannot saturate.
        self.cycles = self.cycles.saturating_add(1);
        cycle
    }
}

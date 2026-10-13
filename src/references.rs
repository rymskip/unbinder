//! The graph formed by a document's references, and the cycles in it.
//!
//! The nodes are the values that references point at. An edge runs from a
//! target to every target referenced inside it, and to every target nested
//! directly inside it, since inlining the outer one copies the inner one's
//! references too. The graph is built in one pass over the document, so the
//! analysis stays proportional to the document however its targets nest.

use alloc::borrow::ToOwned;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec;
use alloc::vec::{IntoIter, Vec};
use core::cell::Cell;
use core::iter::Peekable;
use core::mem;
use core::ptr;

use serde_json::Value;

use crate::error::{Error, PointerError};
use crate::pointer;
use crate::walk::{Values, reference_of};

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
    cycle: Cell<Option<CycleId>>,
    leads_to_cycle: Cell<bool>,
    visit: Cell<Visit>,
}

impl<'doc> Target<'doc> {
    pub(crate) fn value(&self) -> &'doc Value {
        self.value
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

/// A pointer partway through being followed.
struct Walk<'doc> {
    reference: &'doc str,
    current: &'doc Value,
    tokens: Peekable<IntoIter<String>>,
    /// The references passed through since the last token was applied.
    passed: BTreeSet<*const Value>,
}

impl Walk<'_> {
    fn unresolved(&self, cause: PointerError) -> Error {
        Error::UnresolvedReference {
            reference: self.reference.to_owned(),
            cause,
        }
    }
}

/// The reference a pointer has to pass through before `token` can be applied
/// to `current`. A reference's own members come first, as they do once its
/// target is merged into it.
fn passes_through<'doc>(current: &'doc Value, token: &str) -> Option<&'doc str> {
    let reference = reference_of(current)?;
    (!current.as_object()?.contains_key(token)).then_some(reference)
}

/// The targets each target leads to, keyed by where the target sits in the
/// document.
type Successors<'doc> = BTreeMap<*const Value, Vec<Rc<Target<'doc>>>>;

/// A step of the walk that builds [`Successors`].
enum Step<'doc> {
    Enter(&'doc Value),
    /// Every value inside the innermost open target has been entered.
    Leave,
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

/// Every reference in a document, resolved and grouped into cycles.
#[derive(Debug)]
pub(crate) struct References<'doc> {
    root: &'doc Value,
    by_reference: BTreeMap<&'doc str, Rc<Target<'doc>>>,
    /// Two spellings of one pointer share a target, so targets are told
    /// apart by the address of the value they point at.
    by_address: BTreeMap<*const Value, Rc<Target<'doc>>>,
    cycles: usize,
    /// How many references a pointer may pass through between two tokens.
    max_hops: usize,
}

impl<'doc> References<'doc> {
    /// Resolves every reference in `root` and finds the cycles among them.
    pub(crate) fn analyze(root: &'doc Value, max_hops: usize) -> Result<Self, Error> {
        let mut references = Self {
            root,
            by_reference: BTreeMap::new(),
            by_address: BTreeMap::new(),
            cycles: 0,
            max_hops,
        };
        for reference in Values::of(root).filter_map(reference_of) {
            references.target(reference)?;
        }
        let mut successors = references.successors();
        let starts: Vec<_> = references.by_address.values().map(Rc::clone).collect();
        for start in starts {
            references.search(start, &mut successors);
        }
        Ok(references)
    }

    /// The target that `value` is, if any reference points at it.
    pub(crate) fn at(&self, value: &Value) -> Option<Rc<Target<'doc>>> {
        self.by_address.get(&ptr::from_ref(value)).map(Rc::clone)
    }

    /// The target of `reference`, resolved on first use.
    ///
    /// A pointer that reaches a reference with tokens still to apply passes
    /// through it and carries on from that reference's target. Pointers
    /// waiting on one another are kept on an explicit stack, so a long chain
    /// of them cannot exhaust the call stack.
    pub(crate) fn target(&mut self, reference: &'doc str) -> Result<Rc<Target<'doc>>, Error> {
        if let Some(known) = self.by_reference.get(reference) {
            return Ok(Rc::clone(known));
        }
        let mut active = BTreeSet::new();
        let mut waiting = Vec::new();
        let mut walk = self.start(reference, &mut active)?;
        loop {
            let Some(token) = walk.tokens.peek() else {
                let target = self.register(&walk);
                active.remove(walk.reference);
                let Some(resumed) = waiting.pop() else {
                    return Ok(target);
                };
                walk = resumed;
                walk.current = target.value;
                continue;
            };
            let Some(through) = passes_through(walk.current, token) else {
                walk.current = pointer::descend(walk.current, token)
                    .map_err(|cause| walk.unresolved(cause))?;
                walk.tokens.next();
                walk.passed.clear();
                continue;
            };
            if walk.passed.len() >= self.max_hops {
                return Err(Error::DepthLimitExceeded {
                    limit: self.max_hops,
                });
            }
            if !walk.passed.insert(ptr::from_ref(walk.current)) || active.contains(through) {
                return Err(walk.unresolved(PointerError::ReferenceLoop));
            }
            if let Some(known) = self.by_reference.get(through) {
                walk.current = known.value;
            } else {
                let next = self.start(through, &mut active)?;
                waiting.push(mem::replace(&mut walk, next));
            }
        }
    }

    fn start(
        &self,
        reference: &'doc str,
        active: &mut BTreeSet<&'doc str>,
    ) -> Result<Walk<'doc>, Error> {
        let Some(fragment) = reference.strip_prefix('#') else {
            return Err(Error::ExternalReference {
                reference: reference.to_owned(),
            });
        };
        let tokens = pointer::tokens(fragment).map_err(|cause| Error::UnresolvedReference {
            reference: reference.to_owned(),
            cause,
        })?;
        active.insert(reference);
        Ok(Walk {
            reference,
            current: self.root,
            tokens: tokens.into_iter().peekable(),
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
                        cycle: Cell::new(None),
                        leads_to_cycle: Cell::new(false),
                        visit: Cell::new(Visit::Unvisited),
                    })
                }),
        );
        self.by_reference.insert(walk.reference, Rc::clone(&target));
        target
    }

    /// Walks the document once, giving each reference and each nested target
    /// to the innermost target that encloses it.
    fn successors(&self) -> Successors<'doc> {
        let mut successors = Successors::new();
        let mut open = Vec::new();
        let mut steps = vec![Step::Enter(self.root)];
        while let Some(step) = steps.pop() {
            let value = match step {
                Step::Enter(value) => value,
                Step::Leave => {
                    open.pop();
                    continue;
                }
            };
            let address = ptr::from_ref(value);
            if let Some(target) = self.by_address.get(&address) {
                if let Some(enclosing) = open.last() {
                    successors
                        .entry(*enclosing)
                        .or_default()
                        .push(Rc::clone(target));
                }
                open.push(address);
                steps.push(Step::Leave);
            }
            let referenced =
                reference_of(value).and_then(|reference| self.by_reference.get(reference));
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
        successors
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

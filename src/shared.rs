//! A dereferenced document that holds each expanded target once.
//!
//! Inlining a target at every reference to it makes the output as large as
//! the references are many. Here every reference points at one expansion of
//! its target, and the parts of the document with no reference in them are
//! borrowed as they stand, so the structure stays about the size of the
//! document. It expands into the full tree only when it is written out.

use alloc::borrow::ToOwned;
use alloc::collections::BTreeMap;
use alloc::rc::{Rc, Weak};
use alloc::vec::Vec;
use core::cell::OnceCell;
use core::convert::Infallible;

use serde::ser::SerializeMap;
use serde::{Serialize, Serializer};
use serde_json::{Map, Value};

/// A value of the dereferenced document.
#[derive(Clone, Debug)]
pub(crate) enum Shared<'doc> {
    /// A part of the document, as it stands.
    Original(&'doc Value),
    Array(Rc<[Self]>),
    Object(Rc<[(&'doc str, Self)]>),
    /// The expansion of a target. Every reference to the target holds the
    /// same one.
    Inlined(Rc<Anchor<'doc>>),
    /// An object with the keywords next to a reference merged over it.
    Merged(Rc<Merged<'doc>>),
    /// A reference to a target that was still being expanded when the
    /// reference was reached, which closes a cycle.
    Back(Back<'doc>),
}

/// The one expansion of a target.
#[derive(Debug)]
pub(crate) struct Anchor<'doc> {
    pub(crate) value: Shared<'doc>,
}

/// A link back to an expansion that contains it.
#[derive(Clone, Debug)]
pub(crate) struct Back<'doc> {
    /// The expansion, once it is finished. Held weakly, because the
    /// expansion holds this link.
    pub(crate) link: Rc<OnceCell<Weak<Anchor<'doc>>>>,
    /// The reference as written, for an output that cannot hold a cycle.
    pub(crate) written: &'doc Value,
}

impl<'doc> Back<'doc> {
    /// The expansion this links back to, while it is alive.
    pub(crate) fn target(&self) -> Option<Rc<Anchor<'doc>>> {
        self.link.get().and_then(Weak::upgrade)
    }
}

#[derive(Debug)]
pub(crate) struct Merged<'doc> {
    /// The object merged into.
    pub(crate) base: Shared<'doc>,
    /// The members that replace the base's or add to them, in the order
    /// they were written.
    pub(crate) overrides: Vec<(&'doc str, Shared<'doc>)>,
    /// Where each override is, by name.
    positions: BTreeMap<&'doc str, usize>,
}

impl<'doc> Merged<'doc> {
    pub(crate) fn new(base: Shared<'doc>, overrides: Vec<(&'doc str, Shared<'doc>)>) -> Self {
        let positions = overrides
            .iter()
            .enumerate()
            .map(|(position, (name, _))| (*name, position))
            .collect();
        Self {
            base,
            overrides,
            positions,
        }
    }

    fn overridden(&self, name: &str) -> Option<&Shared<'doc>> {
        let position = self.positions.get(name)?;
        self.overrides.get(*position).map(|(_, member)| member)
    }
}

impl<'doc> Shared<'doc> {
    /// Whether this is an object once it is written out.
    pub(crate) fn is_object(&self) -> bool {
        match self {
            Self::Original(value) => value.is_object(),
            Self::Object(_) | Self::Merged(_) => true,
            Self::Array(_) | Self::Back(_) => false,
            Self::Inlined(anchor) => anchor.value.is_object(),
        }
    }

    /// How many members this has to look through to find one: all of them
    /// where they are kept in a list, and none where they are indexed.
    pub(crate) fn scan(&self) -> usize {
        match self {
            Self::Original(_) | Self::Array(_) | Self::Back(_) => 0,
            Self::Object(members) => members.len(),
            Self::Inlined(anchor) => anchor.value.scan(),
            Self::Merged(merged) => merged.base.scan(),
        }
    }

    /// How many members this has, if it is an object.
    pub(crate) fn width(&self) -> usize {
        match self {
            Self::Original(value) => value.as_object().map_or(0, Map::len),
            Self::Array(_) | Self::Back(_) => 0,
            Self::Object(members) => members.len(),
            Self::Inlined(anchor) => anchor.value.width(),
            Self::Merged(merged) => merged.base.width().saturating_add(merged.overrides.len()),
        }
    }

    /// The member `name`, if this is an object that has it.
    pub(crate) fn member(&self, name: &str) -> Option<Self> {
        match self {
            Self::Original(value) => value.as_object()?.get(name).map(Self::Original),
            Self::Object(members) => members
                .iter()
                .find(|(held, _)| *held == name)
                .map(|(_, member)| member.clone()),
            Self::Array(_) | Self::Back(_) => None,
            Self::Inlined(anchor) => anchor.value.member(name),
            Self::Merged(merged) => merged
                .overridden(name)
                .cloned()
                .or_else(|| merged.base.member(name)),
        }
    }

    /// Calls `visit` with each member of this object, and with nothing if it
    /// is not one. The keywords written next to a reference come first, then
    /// what its target adds.
    pub(crate) fn members<Failure>(
        &self,
        visit: &mut dyn FnMut(&'doc str, &Self) -> Result<(), Failure>,
    ) -> Result<(), Failure> {
        match self {
            Self::Original(value) => match value.as_object() {
                Some(members) => members
                    .iter()
                    .try_for_each(|(name, member)| visit(name, &Self::Original(member))),
                None => Ok(()),
            },
            Self::Object(members) => members
                .iter()
                .try_for_each(|(name, member)| visit(name, member)),
            Self::Array(_) | Self::Back(_) => Ok(()),
            Self::Inlined(anchor) => anchor.value.members(visit),
            Self::Merged(merged) => {
                merged
                    .overrides
                    .iter()
                    .try_for_each(|(name, member)| visit(name, member))?;
                merged
                    .base
                    .members(&mut |name, member| match merged.overridden(name) {
                        Some(_) => Ok(()),
                        None => visit(name, member),
                    })
            }
        }
    }

    /// Writes the value out as a tree, copying every target once per
    /// reference to it.
    pub(crate) fn to_value(&self) -> Value {
        match self {
            Self::Original(value) => (*value).clone(),
            Self::Array(items) => Value::Array(items.iter().map(Self::to_value).collect()),
            Self::Object(members) => Value::Object(
                members
                    .iter()
                    .map(|(name, member)| ((*name).to_owned(), member.to_value()))
                    .collect(),
            ),
            Self::Inlined(anchor) => anchor.value.to_value(),
            Self::Merged(_) => {
                let mut members = Map::new();
                let Ok(()) = self.members::<Infallible>(&mut |name, member| {
                    members.insert(name.to_owned(), member.to_value());
                    Ok(())
                });
                Value::Object(members)
            }
            Self::Back(back) => back.written.clone(),
        }
    }
}

impl Serialize for Shared<'_> {
    fn serialize<Format: Serializer>(&self, format: Format) -> Result<Format::Ok, Format::Error> {
        match self {
            Self::Original(value) => value.serialize(format),
            Self::Array(items) => format.collect_seq(items.iter()),
            Self::Object(members) => {
                format.collect_map(members.iter().map(|(name, member)| (name, member)))
            }
            Self::Inlined(anchor) => anchor.value.serialize(format),
            Self::Merged(_) => {
                let mut map = format.serialize_map(None)?;
                self.members(&mut |name, member| map.serialize_entry(name, member))?;
                map.end()
            }
            Self::Back(back) => back.written.serialize(format),
        }
    }
}

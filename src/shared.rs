//! A dereferenced document that holds each expanded target once.
//!
//! Inlining a target at every reference to it makes the output as large as
//! the references are many. Here every reference points at one expansion of
//! its target, and the parts of the document with no reference in them are
//! borrowed as they stand, so the structure stays about the size of the
//! document. It expands into the full tree only when it is written out.

use alloc::borrow::ToOwned;
use alloc::rc::Rc;
use alloc::vec::Vec;
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
    Inlined(Rc<Self>),
    /// An object with the keywords next to a reference merged over it.
    Merged(Rc<Merged<'doc>>),
}

#[derive(Debug)]
pub(crate) struct Merged<'doc> {
    /// The object merged into.
    pub(crate) base: Shared<'doc>,
    /// The members that replace the base's or add to them, sorted by name.
    pub(crate) overrides: Vec<(&'doc str, Shared<'doc>)>,
}

impl<'doc> Merged<'doc> {
    pub(crate) fn new(base: Shared<'doc>, mut overrides: Vec<(&'doc str, Shared<'doc>)>) -> Self {
        overrides.sort_unstable_by_key(|(name, _)| *name);
        Self { base, overrides }
    }

    fn overridden(&self, name: &str) -> Option<&Shared<'doc>> {
        match self.overrides.binary_search_by_key(&name, |(held, _)| held) {
            Ok(position) => self.overrides.get(position).map(|(_, member)| member),
            Err(_) => None,
        }
    }
}

impl<'doc> Shared<'doc> {
    /// Whether this is an object once it is written out.
    pub(crate) fn is_object(&self) -> bool {
        match self {
            Self::Original(value) => value.is_object(),
            Self::Object(_) | Self::Merged(_) => true,
            Self::Array(_) => false,
            Self::Inlined(inner) => inner.is_object(),
        }
    }

    /// How many members this has to look through to find one: all of them
    /// where they are kept in a list, and none where they are indexed.
    pub(crate) fn scan(&self) -> usize {
        match self {
            Self::Original(_) | Self::Array(_) => 0,
            Self::Object(members) => members.len(),
            Self::Inlined(inner) => inner.scan(),
            Self::Merged(merged) => merged.base.scan(),
        }
    }

    /// How many members this has, if it is an object.
    pub(crate) fn width(&self) -> usize {
        match self {
            Self::Original(value) => value.as_object().map_or(0, Map::len),
            Self::Array(_) => 0,
            Self::Object(members) => members.len(),
            Self::Inlined(inner) => inner.width(),
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
            Self::Array(_) => None,
            Self::Inlined(inner) => inner.member(name),
            Self::Merged(merged) => merged
                .overridden(name)
                .cloned()
                .or_else(|| merged.base.member(name)),
        }
    }

    /// Calls `visit` with each member of this object, and with nothing if it
    /// is not one.
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
            Self::Array(_) => Ok(()),
            Self::Inlined(inner) => inner.members(visit),
            Self::Merged(merged) => {
                // The overrides are sorted, so slotting each in ahead of the
                // first base member that sorts after it keeps the members in
                // order whenever the base's are.
                let mut pending = merged.overrides.iter().peekable();
                merged.base.members(&mut |name, member| {
                    while let Some((held, replacement)) = pending.next_if(|(held, _)| *held < name)
                    {
                        visit(held, replacement)?;
                    }
                    match pending.next_if(|(held, _)| *held == name) {
                        Some((held, replacement)) => visit(held, replacement),
                        None if merged.overridden(name).is_some() => Ok(()),
                        None => visit(name, member),
                    }
                })?;
                pending.try_for_each(|(name, member)| visit(name, member))
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
            Self::Inlined(inner) => inner.to_value(),
            Self::Merged(_) => {
                let mut members = Map::new();
                let Ok(()) = self.members::<Infallible>(&mut |name, member| {
                    members.insert(name.to_owned(), member.to_value());
                    Ok(())
                });
                Value::Object(members)
            }
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
            Self::Inlined(inner) => inner.serialize(format),
            Self::Merged(_) => {
                let mut map = format.serialize_map(None)?;
                self.members(&mut |name, member| map.serialize_entry(name, member))?;
                map.end()
            }
        }
    }
}

//! JSON Pointer syntax and single steps ([RFC 6901]).
//!
//! [RFC 6901]: https://datatracker.ietf.org/doc/html/rfc6901

use alloc::borrow::ToOwned;
use alloc::string::String;
use alloc::vec::Vec;

use percent_encoding::percent_decode_str;
use serde_json::{Map, Value};

use crate::error::PointerError;

/// The unescaped tokens of a pointer in its URI fragment form, without the
/// leading `#`.
pub(crate) fn tokens(fragment: &str) -> Result<Vec<String>, PointerError> {
    let Ok(pointer) = percent_decode_str(fragment).decode_utf8() else {
        return Err(PointerError::InvalidPercentEncoding);
    };
    if pointer.is_empty() {
        return Ok(Vec::new());
    }
    let Some(path) = pointer.strip_prefix('/') else {
        return Err(PointerError::MissingLeadingSlash);
    };
    path.split('/').map(unescape).collect()
}

/// The child of `current` that an unescaped `token` names.
pub(crate) fn descend<'doc>(
    current: &'doc Value,
    token: &str,
) -> Result<&'doc Value, PointerError> {
    match current {
        Value::Object(members) => members.get(token).ok_or_else(|| PointerError::MissingKey {
            key: token.to_owned(),
        }),
        Value::Array(items) => {
            let index = parse_index(token)?;
            items.get(index).ok_or(PointerError::IndexOutOfBounds {
                index,
                length: items.len(),
            })
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {
            Err(PointerError::NotAContainer {
                token: token.to_owned(),
            })
        }
    }
}

/// The member names of one object that hold a `/`, in order.
///
/// A name written into a pointer with its `/` not escaped reads as several
/// tokens. Such a pointer is followed all the same: where a token names no
/// member, the longest run of tokens that spells one of these names is taken
/// as that name.
#[derive(Debug)]
pub(crate) struct Slashed<'doc> {
    names: Vec<&'doc str>,
}

impl<'doc> Slashed<'doc> {
    pub(crate) fn of(members: &'doc Map<String, Value>) -> Self {
        let mut names: Vec<_> = members
            .keys()
            .map(String::as_str)
            .filter(|name| name.contains('/'))
            .collect();
        names.sort_unstable();
        Self { names }
    }

    /// The longest name that the first of `tokens` spell when a `/` joins
    /// them, and how many of them it takes, which is two at the least.
    pub(crate) fn longest(&self, tokens: &[String]) -> Option<(&'doc str, usize)> {
        let mut candidates = self.names.as_slice();
        let mut spelled: usize = 0;
        let mut longest = None;
        for (position, token) in tokens.iter().enumerate() {
            if position > 0 {
                candidates = continuing(candidates, spelled, "/");
                spelled = spelled.saturating_add(1);
            }
            candidates = continuing(candidates, spelled, token);
            spelled = spelled.saturating_add(token.len());
            // A name sorts ahead of every longer name that starts with it.
            match candidates.first() {
                Some(name) if position > 0 && name.len() == spelled => {
                    longest = Some((*name, position.saturating_add(1)));
                }
                Some(_) => {}
                None => break,
            }
        }
        longest
    }
}

/// The `candidates` that go on with `piece` after their first `spelled`
/// bytes, which they all share. Only `piece` is compared, so following a
/// name costs its length once, however many tokens spell it.
fn continuing<'names, 'doc>(
    candidates: &'names [&'doc str],
    spelled: usize,
    piece: &str,
) -> &'names [&'doc str] {
    let below = candidates.partition_point(|name| rest(name, spelled) < piece.as_bytes());
    let from = candidates.get(below..).unwrap_or_default();
    let run = from.partition_point(|name| rest(name, spelled).starts_with(piece.as_bytes()));
    from.get(..run).unwrap_or_default()
}

/// What `name` holds after its first `spelled` bytes.
fn rest(name: &str, spelled: usize) -> &[u8] {
    name.as_bytes().get(spelled..).unwrap_or_default()
}

fn unescape(token: &str) -> Result<String, PointerError> {
    let mut unescaped = String::with_capacity(token.len());
    let mut characters = token.chars();
    while let Some(character) = characters.next() {
        if character != '~' {
            unescaped.push(character);
            continue;
        }
        match characters.next() {
            Some('0') => unescaped.push('~'),
            Some('1') => unescaped.push('/'),
            Some(_) | None => {
                return Err(PointerError::InvalidEscape {
                    token: token.to_owned(),
                });
            }
        }
    }
    Ok(unescaped)
}

/// Parses an array index, which is `0` or digits with no leading zero.
fn parse_index(token: &str) -> Result<usize, PointerError> {
    match token.parse() {
        Ok(index) if token == "0" || !token.starts_with(['0', '+']) => Ok(index),
        Ok(_) | Err(_) => Err(PointerError::InvalidIndex {
            token: token.to_owned(),
        }),
    }
}

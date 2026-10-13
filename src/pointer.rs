//! JSON Pointer syntax and single steps ([RFC 6901]).
//!
//! [RFC 6901]: https://datatracker.ietf.org/doc/html/rfc6901

use alloc::borrow::ToOwned;
use alloc::string::String;
use alloc::vec::Vec;

use percent_encoding::percent_decode_str;
use serde_json::Value;

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

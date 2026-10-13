//! Traversal of a JSON document.

use alloc::vec;
use alloc::vec::Vec;

use serde_json::Value;

/// The member that marks an object as a reference.
pub(crate) const REFERENCE_KEY: &str = "$ref";

/// The pointer `value` carries, if it is a reference.
pub(crate) fn reference_of(value: &Value) -> Option<&str> {
    value.as_object()?.get(REFERENCE_KEY)?.as_str()
}

/// Every value in a document, the root included.
///
/// Iterative, so the depth of the document cannot exhaust the stack.
pub(crate) struct Values<'doc> {
    pending: Vec<&'doc Value>,
}

impl<'doc> Values<'doc> {
    pub(crate) fn of(root: &'doc Value) -> Self {
        Self {
            pending: vec![root],
        }
    }
}

impl<'doc> Iterator for Values<'doc> {
    type Item = &'doc Value;

    fn next(&mut self) -> Option<Self::Item> {
        let value = self.pending.pop()?;
        match value {
            Value::Object(members) => self.pending.extend(members.values()),
            Value::Array(items) => self.pending.extend(items),
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
        }
        Some(value)
    }
}

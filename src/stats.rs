//! Size statistics.

use serde_json::Value;

use crate::walk::{Values, reference_of};

/// How many containers and references a document holds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "wasm", derive(serde::Serialize, tsify::Tsify))]
pub struct Stats {
    /// Objects, counting references.
    pub objects: usize,
    /// Arrays.
    pub arrays: usize,
    /// Objects with a string `$ref` member.
    pub references: usize,
}

impl Stats {
    /// Counts the containers and references in `schema`.
    ///
    /// ```
    /// use serde_json::json;
    /// use unbinder::Stats;
    ///
    /// let schema = json!({
    ///     "properties": {
    ///         "tags": { "type": "array", "items": { "$ref": "#/definitions/tag" } }
    ///     },
    ///     "required": ["tags"],
    ///     "definitions": { "tag": { "type": "string" } }
    /// });
    ///
    /// let stats = Stats::of(&schema);
    /// assert_eq!(stats.objects, 6);
    /// assert_eq!(stats.arrays, 1);
    /// assert_eq!(stats.references, 1);
    /// ```
    #[must_use]
    pub fn of(schema: &Value) -> Self {
        Values::of(schema).fold(Self::default(), |stats, value| match value {
            Value::Object(_) => Self {
                objects: stats.objects.saturating_add(1),
                references: stats
                    .references
                    .saturating_add(usize::from(reference_of(value).is_some())),
                ..stats
            },
            Value::Array(_) => Self {
                arrays: stats.arrays.saturating_add(1),
                ..stats
            },
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => stats,
        })
    }
}

//! Error types.

use alloc::string::String;
use core::fmt;

/// Why a document could not be dereferenced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// A reference points outside the document, and the document was
    /// dereferenced on its own. [`Documents`] follows such references.
    ///
    /// [`Documents`]: crate::Documents
    ExternalReference {
        /// The reference as written in the document.
        reference: String,
    },
    /// A document is named by something that is not a URL, or by a relative
    /// URL where there is no absolute one to resolve it against.
    InvalidUrl {
        /// The URL as written.
        url: String,
    },
    /// A reference points into a document that is not in the set.
    MissingDocument {
        /// The absolute URL of the document.
        url: String,
    },
    /// A reference points at a location that does not exist in the document.
    UnresolvedReference {
        /// The reference as written in the document.
        reference: String,
        /// Where resolution stopped.
        cause: PointerError,
    },
    /// A reference takes part in a cycle and [`CircularPolicy::Reject`] is in
    /// effect.
    ///
    /// [`CircularPolicy::Reject`]: crate::CircularPolicy::Reject
    CircularReference {
        /// The reference that closes the cycle.
        reference: String,
    },
    /// The output nests deeper than [`Options::max_depth`], or a pointer
    /// passes through more references than that.
    ///
    /// [`Options::max_depth`]: crate::Options::max_depth
    DepthLimitExceeded {
        /// The limit that was in effect.
        limit: usize,
    },
    /// The references lead through more documents than
    /// [`Options::max_documents`].
    ///
    /// [`Options::max_documents`]: crate::Options::max_documents
    DocumentLimitExceeded {
        /// The limit that was in effect.
        limit: usize,
    },
    /// The output takes more memory than [`Options::max_bytes`].
    ///
    /// [`Options::max_bytes`]: crate::Options::max_bytes
    SizeLimitExceeded {
        /// The limit that was in effect.
        limit: usize,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ExternalReference { reference } => {
                write!(
                    formatter,
                    "external reference `{reference}` is not supported"
                )
            }
            Self::InvalidUrl { url } => write!(formatter, "`{url}` is not a usable URL"),
            Self::MissingDocument { url } => {
                write!(formatter, "document `{url}` has not been loaded")
            }
            Self::UnresolvedReference { reference, cause } => {
                write!(formatter, "cannot resolve reference `{reference}`: {cause}")
            }
            Self::CircularReference { reference } => {
                write!(formatter, "reference `{reference}` is circular")
            }
            Self::DepthLimitExceeded { limit } => {
                write!(formatter, "depth limit of {limit} levels exceeded")
            }
            Self::DocumentLimitExceeded { limit } => {
                write!(
                    formatter,
                    "references lead through more than {limit} documents"
                )
            }
            Self::SizeLimitExceeded { limit } => {
                write!(formatter, "output takes more than {limit} bytes")
            }
        }
    }
}

impl core::error::Error for Error {}

/// Why a JSON Pointer ([RFC 6901]) could not be followed.
///
/// [RFC 6901]: https://datatracker.ietf.org/doc/html/rfc6901
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PointerError {
    /// The fragment does not decode to UTF-8 once its percent-encoding is
    /// removed.
    InvalidPercentEncoding,
    /// A non-empty pointer does not start with `/`.
    MissingLeadingSlash,
    /// A token holds a `~` that is not part of a `~0` or `~1` escape.
    InvalidEscape {
        /// The token as written in the pointer.
        token: String,
    },
    /// An object has no member with this name.
    MissingKey {
        /// The unescaped member name.
        key: String,
    },
    /// A token addresses an array but is not an array index.
    InvalidIndex {
        /// The unescaped token.
        token: String,
    },
    /// An array is shorter than the index requires.
    IndexOutOfBounds {
        /// The requested index.
        index: usize,
        /// The length of the array.
        length: usize,
    },
    /// The pointer continues past a value that has no children.
    NotAContainer {
        /// The unescaped token that could not be applied.
        token: String,
    },
    /// The pointer passes through a reference whose own pointer leads back
    /// to it.
    ReferenceLoop,
}

impl fmt::Display for PointerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPercentEncoding => {
                formatter.write_str("percent-encoding does not decode to UTF-8")
            }
            Self::MissingLeadingSlash => formatter.write_str("pointer does not start with `/`"),
            Self::InvalidEscape { token } => {
                write!(formatter, "token `{token}` holds an invalid `~` escape")
            }
            Self::MissingKey { key } => write!(formatter, "no member named `{key}`"),
            Self::InvalidIndex { token } => write!(formatter, "`{token}` is not an array index"),
            Self::IndexOutOfBounds { index, length } => {
                write!(
                    formatter,
                    "index {index} is out of bounds for length {length}"
                )
            }
            Self::NotAContainer { token } => {
                write!(
                    formatter,
                    "cannot apply `{token}` to a value with no children"
                )
            }
            Self::ReferenceLoop => {
                formatter.write_str("pointer passes through a reference that leads back to it")
            }
        }
    }
}

impl core::error::Error for PointerError {}

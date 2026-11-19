// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `DOMException` names as a Rust error type.
//!
//! Web IDL §2.8.1 "Error names" lists the names; the legacy numeric codes are the `code` column of that table.

use std::fmt;

/// An error a DOM algorithm throws. Each variant is a `DOMException` name from Web IDL's error names table;
/// the bindings layer turns it into a `DOMException` with [`DomError::name`] and [`DomError::code`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DomError {
    /// "IndexSizeError": the index is not in the allowed range.
    IndexSize,
    /// "HierarchyRequestError": the operation would yield an incorrect node tree.
    HierarchyRequest,
    /// "WrongDocumentError": the object is in the wrong document.
    WrongDocument,
    /// "InvalidCharacterError": the string contains invalid characters.
    InvalidCharacter,
    /// "NoModificationAllowedError": the object can not be modified.
    NoModificationAllowed,
    /// "NotFoundError": the object can not be found here.
    NotFound,
    /// "NotSupportedError": the operation is not supported.
    NotSupported,
    /// "InUseAttributeError": the attribute is in use by another element.
    InUseAttribute,
    /// "InvalidStateError": the object is in an invalid state.
    InvalidState,
    /// "SyntaxError": the string did not match the expected pattern.
    Syntax,
    /// "InvalidModificationError": the object can not be modified in this way.
    InvalidModification,
    /// "NamespaceError": the operation is not allowed by Namespaces in XML.
    Namespace,
    /// "InvalidAccessError": the object does not support the operation or argument.
    InvalidAccess,
    /// "TypeMismatchError" (legacy).
    TypeMismatch,
    /// "SecurityError": the operation is insecure.
    Security,
    /// "NetworkError": a network error occurred.
    Network,
    /// "AbortError": the operation was aborted.
    Abort,
    /// "QuotaExceededError" (legacy code 22).
    QuotaExceeded,
    /// "TimeoutError": the operation timed out.
    Timeout,
    /// "InvalidNodeTypeError": the supplied node is incorrect or has an incorrect ancestor for this operation.
    InvalidNodeType,
    /// "DataCloneError": the object can not be cloned.
    DataClone,
}

impl DomError {
    /// The `DOMException` `name` attribute value.
    pub fn name(self) -> &'static str {
        match self {
            DomError::IndexSize => "IndexSizeError",
            DomError::HierarchyRequest => "HierarchyRequestError",
            DomError::WrongDocument => "WrongDocumentError",
            DomError::InvalidCharacter => "InvalidCharacterError",
            DomError::NoModificationAllowed => "NoModificationAllowedError",
            DomError::NotFound => "NotFoundError",
            DomError::NotSupported => "NotSupportedError",
            DomError::InUseAttribute => "InUseAttributeError",
            DomError::InvalidState => "InvalidStateError",
            DomError::Syntax => "SyntaxError",
            DomError::InvalidModification => "InvalidModificationError",
            DomError::Namespace => "NamespaceError",
            DomError::InvalidAccess => "InvalidAccessError",
            DomError::TypeMismatch => "TypeMismatchError",
            DomError::Security => "SecurityError",
            DomError::Network => "NetworkError",
            DomError::Abort => "AbortError",
            DomError::QuotaExceeded => "QuotaExceededError",
            DomError::Timeout => "TimeoutError",
            DomError::InvalidNodeType => "InvalidNodeTypeError",
            DomError::DataClone => "DataCloneError",
        }
    }

    /// The legacy `DOMException` `code` (0 for names that have none).
    pub fn code(self) -> u16 {
        match self {
            DomError::IndexSize => 1,
            DomError::HierarchyRequest => 3,
            DomError::WrongDocument => 4,
            DomError::InvalidCharacter => 5,
            DomError::NoModificationAllowed => 7,
            DomError::NotFound => 8,
            DomError::NotSupported => 9,
            DomError::InUseAttribute => 10,
            DomError::InvalidState => 11,
            DomError::Syntax => 12,
            DomError::InvalidModification => 13,
            DomError::Namespace => 14,
            DomError::InvalidAccess => 15,
            DomError::TypeMismatch => 17,
            DomError::Security => 18,
            DomError::Network => 19,
            DomError::Abort => 20,
            DomError::QuotaExceeded => 22,
            DomError::Timeout => 23,
            DomError::InvalidNodeType => 24,
            DomError::DataClone => 25,
        }
    }
}

impl fmt::Display for DomError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl std::error::Error for DomError {}

/// Result of a DOM algorithm that can throw.
pub type DomResult<T> = Result<T, DomError>;

//! Indirect-document components: kinds, ids and the typed resolver.

#[cfg(not(feature = "std"))]
use alloc::{string::String, vec::Vec};

/// The kind of an external component listed by an indirect `FORM:DJVM`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum ComponentKind {
    /// A renderable `FORM:DJVU` page.
    Page,
    /// A shared `FORM:DJVI` component, such as a JB2 symbol dictionary.
    Shared,
    /// A `FORM:THUM` thumbnail component.
    Thumbnail,
}

/// Stable identity of one component in an indirect `FORM:DJVM` directory.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ComponentId {
    /// Resolver key from the DIRM directory.
    pub name: String,
    /// DIRM classification for this component.
    pub kind: ComponentKind,
}

impl ComponentId {
    /// Construct a component identity from its resolver key and DIRM kind.
    pub fn new(name: impl Into<String>, kind: ComponentKind) -> Self {
        Self {
            name: name.into(),
            kind,
        }
    }
}

/// Typed failures returned by a [`ComponentResolver`].
#[derive(Debug, Clone, thiserror::Error)]
#[non_exhaustive]
pub enum ComponentResolveError {
    /// The requested component is not available to the resolver.
    #[error("indirect component {component:?} is missing")]
    Missing {
        /// Identity of the missing component.
        component: ComponentId,
    },

    /// The resolver could not read or construct the requested component.
    #[error("failed to resolve indirect component {component:?}: {reason}")]
    Failed {
        /// Identity of the component that could not be resolved.
        component: ComponentId,
        /// Human-readable resolver detail.
        reason: String,
    },
}

/// Synchronous resolver contract for indirect DJVM components.
///
/// The resolver is called once for every DIRM entry, including pages, shared
/// components, and thumbnails. The typed [`ComponentId`] keeps the component
/// identity and its DIRM classification together so sync, async, and mutable
/// adapters can share the same vocabulary as they are added.
pub trait ComponentResolver {
    /// Return the complete IFF bytes for one external component.
    fn resolve(&self, component: &ComponentId) -> Result<Vec<u8>, ComponentResolveError>;
}

impl<F> ComponentResolver for F
where
    F: Fn(&ComponentId) -> Result<Vec<u8>, ComponentResolveError>,
{
    fn resolve(&self, component: &ComponentId) -> Result<Vec<u8>, ComponentResolveError> {
        self(component)
    }
}

/// One entry from a document `DIRM` directory (or a synthesized single-page view).
///
/// Kind letters follow DjVuLibre `djvused ls`: `P` page, `I` shared/include,
/// `T` thumbnail.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ComponentDirectoryEntry {
    /// Component classification letter (`P`, `I`, or `T`).
    pub kind: char,
    /// Resolver / directory id string.
    pub id: String,
}

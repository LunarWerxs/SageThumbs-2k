# Typed Indirect DJVM Resolver

An indirect `FORM:DJVM` stores its components outside the index file. The
sync document reader exposes a typed resolver entry point for applications
that need to distinguish pages from shared components and thumbnails:

```rust,no_run
use djvu_rs::{
    ComponentId, ComponentKind, ComponentResolveError, ComponentResolver, DjVuDocument,
};

struct Resolver;

impl ComponentResolver for Resolver {
    fn resolve(&self, component: &ComponentId) -> Result<Vec<u8>, ComponentResolveError> {
        let bytes = std::fs::read(&component.name).map_err(|_| ComponentResolveError::Missing {
            component: component.clone(),
        })?;
        Ok(bytes)
    }
}

fn open(index: &[u8]) -> Result<DjVuDocument, djvu_rs::DocError> {
    DjVuDocument::parse_with_component_resolver(index, &Resolver)
}

fn kind_is_page(component: &ComponentId) -> bool {
    component.kind == ComponentKind::Page
}
```

The resolver is called once per DIRM entry, in directory order. It receives
the stable DIRM name and its classification (`Page`, `Shared`, or
`Thumbnail`). Resolved page components must be `FORM:DJVU`; shared components
must be `FORM:DJVI`; thumbnails must be `FORM:THUM`. A mismatch is reported as
`DocError::ComponentKindMismatch`, while a resolver failure remains typed as
`DocError::ComponentResolve`.

Shared `Djbz` dictionaries are indexed and attached to pages that reference
them through `INCL`, matching bundled-document behavior. Other shared payloads
and thumbnails are resolved and format-checked but are not yet exposed by the
high-level page model.

The existing `parse_with_resolver` callback, which receives only `&str`, is
kept for compatibility. Async/lazy loading and mutable-document adapters will
adopt the same component vocabulary in follow-up slices of issue #687.

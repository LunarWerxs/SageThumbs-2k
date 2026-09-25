//! Public sync resolver contract for indirect DJVM documents.

use std::cell::RefCell;

use djvu_rs::{
    ComponentId, ComponentKind, ComponentResolveError, ComponentResolver, DjVuDocument, DocError,
};

struct FixtureResolver {
    page: Vec<u8>,
    seen: RefCell<Vec<ComponentId>>,
}

impl ComponentResolver for FixtureResolver {
    fn resolve(&self, component: &ComponentId) -> Result<Vec<u8>, ComponentResolveError> {
        self.seen.borrow_mut().push(component.clone());
        if component.name == "chicken.djvu" {
            Ok(self.page.clone())
        } else {
            Err(ComponentResolveError::Missing {
                component: component.clone(),
            })
        }
    }
}

#[test]
fn sync_resolver_receives_typed_page_identity() {
    let index = djvu_rs::djvm::create_indirect(&["chicken.djvu"]).unwrap();
    let page = std::fs::read("tests/fixtures/chicken.djvu").unwrap();
    let resolver = FixtureResolver {
        page,
        seen: RefCell::new(Vec::new()),
    };

    let doc = DjVuDocument::parse_with_component_resolver(&index, &resolver).unwrap();
    assert_eq!(doc.page_count(), 1);
    assert_eq!(doc.page(0).unwrap().dimensions(), (181, 240));
    assert_eq!(
        resolver.seen.borrow().as_slice(),
        &[ComponentId {
            name: "chicken.djvu".into(),
            kind: ComponentKind::Page,
        }]
    );
}

#[test]
fn sync_resolver_surfaces_typed_missing_component() {
    let index = djvu_rs::djvm::create_indirect(&["missing.djvu"]).unwrap();
    let resolver = |component: &ComponentId| {
        Err(ComponentResolveError::Missing {
            component: component.clone(),
        })
    };

    let err = DjVuDocument::parse_with_component_resolver(&index, &resolver).unwrap_err();
    assert!(matches!(
        err,
        DocError::ComponentResolve(ComponentResolveError::Missing { component })
            if component.name == "missing.djvu" && component.kind == ComponentKind::Page
    ));
}

#[test]
fn sync_resolver_rejects_component_kind_mismatch() {
    let index = djvu_rs::djvm::create_indirect(&["page.djvu"]).unwrap();
    let wrong_form = djvu_rs::iff::partial_emit(*b"DJVI", &[]).unwrap();
    let resolver = |_component: &ComponentId| Ok::<_, ComponentResolveError>(wrong_form.clone());

    let err = DjVuDocument::parse_with_component_resolver(&index, &resolver).unwrap_err();
    assert!(matches!(
        err,
        DocError::ComponentKindMismatch {
            component,
            expected: ComponentKind::Page,
            found,
        } if component.name == "page.djvu" && found == *b"DJVI"
    ));
}

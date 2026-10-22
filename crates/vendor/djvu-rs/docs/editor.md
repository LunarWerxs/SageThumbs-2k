# Typed Document Editor

Issue: #688

`DocumentEditor` is the validated, library-side operation model for editing a
DjVu document. A caller builds an `EditRequest`, previews its semantic
`EditPlan`, and then applies the same request either to bytes or to a path.

## Supported operation slice

Schema version `1` supports:

- replacing or removing a page `TXTz` text layer;
- replacing or removing page `ANTz` annotations;
- replacing or removing page-level metadata;
- replacing or removing document-level metadata;
- replacing or removing bundled-document `NAVM` bookmarks.

Both single-page `FORM:DJVU` documents and bundled `FORM:DJVM` documents with
at least one page are supported. An indirect or empty `FORM:DJVM` is rejected
because editing it needs a resolver-aware multi-file commit model.

Metadata is written where DjVuLibre reads it, as a `(metadata …)` block in
an `ANTz` annotation chunk. Other annotations in that chunk are kept.

- Page metadata goes into the page's own `ANTz`.
- Document metadata of a single-page file goes into the page's `ANTz`.
- Document metadata of a bundled file goes into the shared annotation
  component (`FORM:DJVI`). The first edit adds that component, its `DIRM`
  entry and an `INCL` reference in every page, as `djvused set-meta` does.
- Older `METa`/`METz` chunks are removed, because DjVuLibre ignores them.
- Replacing or removing page annotations keeps the `(metadata …)` block of
  that chunk, unless the new annotation carries its own block.

## Validation and commit behavior

`DocumentEditor::plan` validates the schema, document shape, page indexes, and
the mutation of every operation against a private clone. `DocumentEditor::apply`
performs the same validation before returning edited bytes.

`DocumentEditor::apply_to_path` reads the input, computes the complete edited
result in memory, creates a sibling temporary file, writes and syncs it, and
then renames it over the destination. A rejected request therefore leaves an
existing destination unchanged. Input and output paths that resolve to the
same file are rejected.

`DocumentEditor::commit_bytes` is that commit step on its own: it validates
edited bytes (for example from `DjVuDocumentMut`) and atomically replaces the
destination. It may replace the file the bytes were read from; the Python
`Editor.save` uses it.

Operations are applied in order. On a single-page document, page-level and
document-level metadata refer to the same root `FORM:DJVU`; use a bundled
document when those scopes must remain distinct.

## JSON schema

With the `serde` feature, `EditRequest` and `EditPlan` derive JSON
serialization. The request carries `version`, and operation and plan enum
names use `snake_case` so callers can persist or generate the schema without
depending on Rust naming conventions.

The current slice deliberately leaves the following for follow-up operations:

- declarative CLI input and semantic diff output;
- XMP, thumbnails, page properties, page order, insertion/deletion, and page
  extraction;
- indirect-DJVM component graphs and atomic multi-file commits;
- unknown-chunk preservation policy beyond the byte-preserving mutation
  primitive.

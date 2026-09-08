# Indirect DJVM Mutation Decision

Issue: #303

Date: 2026-05-17

## Context

An indirect `FORM:DJVM` stores only a `DIRM` index in the root file. Page
component bytes live in external files named by the directory entries and are
resolved by callers at parse time. The current mutation API,
`DjVuDocumentMut::from_bytes`, owns only the root bytes and parsed root IFF tree;
it does not own a resolver, component byte buffers, or an output directory.

Current behavior is intentionally unsupported: `page_mut` returns
`MutError::IndirectDjvmUnsupported` for indirect DJVM documents before page
index range checks. This remains the correct behavior until an explicit API can
describe where edited bytes should be written.

## Strategies Considered

### Rewrite external page files in place

This keeps the input document shape: the root DJVM stays indirect, edited page
components are written back to their original external page files, and unedited
component files are left untouched.

API shape:

- Open with the root bytes plus a resolver that returns both bytes and a stable
  write target for each `DIRM` entry.
- Commit through an output directory or writer callback, not by mutating files
  during setter calls.
- Report a per-component write plan before commit so callers can audit which
  external paths will change.

Failure modes:

- A resolver may synthesize bytes without a writable backing path.
- A DIRM entry can contain duplicate, relative, absolute, or unsafe names.
- Partial writes can corrupt a document set unless every component write is
  staged and atomically renamed.
- Cross-device renames and existing-file replacement semantics vary by
  platform.

Atomicity expectations:

- Never write during mutation.
- Write edited components to temporary files in the destination directory.
- Preserve unedited components by leaving them in place or copying them only
  when exporting to a new directory.
- Replace the root DJVM last.

This strategy is useful for applications that manage a directory of component
files, but it requires a larger API and a careful commit protocol.

### Re-bundle into one bundled DJVM output

This resolves every component through the caller-provided resolver, applies the
same page-level mutation model used by bundled DJVM, and emits a single bundled
`FORM:DJVM` with a fresh bundled `DIRM` offset table.

API shape:

- Keep `DjVuDocumentMut::from_bytes` unchanged and unsupported for indirect
  mutation.
- Add a separate constructor or conversion helper, for example
  `DjVuDocumentMut::from_indirect_resolved(root, resolver)`, that loads the
  root plus component bytes into an owned bundled mutation tree.
- `try_into_bytes` returns one bundled DJVM byte stream; no output directory is
  needed.

Failure modes:

- Missing or invalid resolver output fails before mutation begins.
- Documents with shared dictionaries, thumbnails, or unsupported component
  types must either be preserved as components in the bundled output or rejected
  with an explicit error.
- Output size may increase because the result embeds all page bytes.

Atomicity expectations:

- The API returns bytes only. Filesystem atomicity is the caller's
  responsibility, matching the existing mutation API.
- No external component file is modified.

This strategy fits the existing owned-byte API, avoids multi-file partial-write
risk, and gives OCR/text injection a deterministic output path.

## Decision

Implement re-bundled output first.

The first indirect mutation implementation should resolve an indirect DJVM into
an owned bundled mutation tree and return a single bundled DJVM byte stream from
`try_into_bytes`. This keeps mutation side-effect-free, preserves the current
`DjVuDocumentMut` commit model, and avoids defining filesystem atomicity in the
same change as page editing.

External page-file rewriting remains a later, explicit API. It should not be
hidden behind `try_into_bytes`, because callers must opt into a multi-file write
plan, destination policy, and atomic replacement behavior.

## Follow-Up Scope

1. Add an indirect-to-bundled mutation constructor. **(Implemented — #325.)**
   - Input: root DJVM bytes plus a resolver for DIRM component names.
   - Output: `DjVuDocumentMut` backed by a bundled DJVM tree.
   - Preserve current `IndirectDjvmUnsupported` behavior for plain
     `from_bytes`.
   - Cover resolver errors, missing components, and successful page metadata or
     text-layer mutation.
   - Shipped as
     [`DjVuDocumentMut::from_indirect_resolved`](../src/djvu_mut.rs). It resolves
     every `DIRM` component, validates each is a `FORM:DJVU`/`DJVI`/`THUM`,
     reuses the original BZZ directory metadata while flipping the bundled bit
     and recomputing the offset table, and returns a side-effect-free bundled
     byte stream from `try_into_bytes`.

2. Add an explicit external-file rewrite API only after the rebundling path is
   shipped. **(Implemented — #326.)**
   - Expose a write plan before commit.
   - Require a destination directory or per-component writer.
   - Stage temporary files and document atomicity guarantees.
   - Reject unsafe or duplicate DIRM names.
   - Shipped as
     [`IndirectRewritePlan`](../src/djvu_mut.rs); see
     [External-File Rewrite Path](#external-file-rewrite-path-326) below.

## External-File Rewrite Path (#326)

`IndirectRewritePlan` implements the second strategy: it keeps the document
indirect and writes edits back to per-component files, rather than collapsing
everything into one bundled stream.

How it differs from the rebundled-output path:

| | `DjVuDocumentMut::from_indirect_resolved` | `IndirectRewritePlan` |
|---|---|---|
| Output | one bundled `FORM:DJVM` byte stream | a directory of component files + index |
| Side effects | none (`try_into_bytes` returns bytes) | files written by `commit_to_dir` |
| Caller policy | none | destination dir, file names, atomicity |
| Document shape | becomes bundled | stays indirect |

Commit protocol:

- Edits are staged in memory (`edit_page`, `set_bookmarks`); nothing is written
  during setters.
- `plan(root_name)` previews every file a commit will write and flags which
  changed.
- `commit_to_dir(dir, root_name)` validates all names first (so an invalid
  request leaves the destination untouched), then writes each component and the
  root index by staging a sibling temp file and atomically renaming it into
  place. The root index is written last.

Name safety: component ids and the root name must be safe *flat* relative names
— no path separators, `.`/`..`, drive/stream colon, or NUL — so a rewrite can
never escape the destination directory. Duplicate names (including a root name
colliding with a component) are rejected.

Atomicity: each individual file is replaced atomically on platforms with
same-directory atomic rename. The multi-file commit as a whole is **not**
transactional — a crash mid-commit can leave a mix of old and new component
files. Because indirect components are independent, self-describing page files,
each remains individually valid; callers needing all-or-nothing semantics should
commit to a fresh directory and swap it in.

## Current Unsupported Behavior

In-place mutation through the plain `DjVuDocumentMut::from_bytes` entry point
remains unsupported for indirect DJVM documents (`page_mut` returns
`IndirectDjvmUnsupported`). Use `from_indirect_resolved` for a bundled result,
or `IndirectRewritePlan` to rewrite the external component files.

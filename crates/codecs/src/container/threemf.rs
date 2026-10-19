//! 3MF packages with no picture of their own: the model is DRAWN, like an STL (`decode::mesh`).
//! A slicer's saved picture always wins (`project::extract` runs first); this is for the CAD
//! exports and bare model files that carry only geometry, which went without a thumbnail.
//!
//! What is read (3MF Core, plus the Production extension's separate model parts): the root model
//! part `_rels/.rels` names, each `<object>`'s `<mesh>` (vertices and triangles) and
//! `<components>`, and the `<build>` items with their transforms. Bambu Studio and OrcaSlicer
//! keep each mesh in its own `3D/Objects/*.model` part and point at it from a component's
//! `p:path`; those parts are read too.
//!
//! Hostile bytes, as everywhere here: the XML is scanned as a stream and never held whole, every
//! count is capped, and the component graph is walked to a fixed depth with a fixed amount of
//! work, so a cycle or a fan-out bomb costs a bounded amount and draws what it reached. A vertex
//! that is not a finite number keeps its index (the triangles after it must not shift) and only
//! the triangles that use it are dropped.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Seek};

use image::DynamicImage;
use zip::ZipArchive;

/// Vertices held across every part of one package (12 bytes each).
const MAX_VERTS: usize = 16_000_000;
/// Triangles held across every part (12 bytes each).
const MAX_TRIS: usize = 8_000_000;
/// Component and build-item placements held across every part.
const MAX_PLACEMENTS: usize = 1_000_000;
/// Decompressed XML scanned across every part; past it the model is drawn as far as it was read.
const MAX_XML_BYTES: u64 = 512 * 1024 * 1024;
/// Distinct model parts read: the root and the ones components point at.
const MAX_PARTS: usize = 256;
/// How deep components may nest.
const MAX_DEPTH: u32 = 8;
/// Placements visited plus triangles emitted in one walk of the model: an object placed a
/// thousand times, each placing another a thousand times, must not multiply without end.
const MAX_EMIT: u64 = 32_000_000;
/// The longest tag read in one piece; a longer one (no real 3MF writes one) is stepped over.
const MAX_TAG: usize = 64 * 1024;
/// Where every writer puts the root model part, for a package whose `_rels/.rels` does not say.
const ROOT_DEFAULT: &str = "3D/3dmodel.model";

/// A 3MF transform, row-vector convention (`p' = p·M`): `m00 m01 m02 m10 m11 m12 m20 m21 m22
/// m30 m31 m32`, the last three the translation.
type Mat = [f32; 12];
const IDENTITY: Mat = [1., 0., 0., 0., 1., 0., 0., 0., 1., 0., 0., 0.];

fn apply(m: &Mat, p: [f32; 3]) -> [f32; 3] {
    let [x, y, z] = p;
    [
        x * m[0] + y * m[3] + z * m[6] + m[9],
        x * m[1] + y * m[4] + z * m[7] + m[10],
        x * m[2] + y * m[5] + z * m[8] + m[11],
    ]
}

/// `a`, then `b`.
fn then(a: &Mat, b: &Mat) -> Mat {
    let linear = |r: usize| {
        let p = apply(b, [a[r], a[r + 1], a[r + 2]]);
        [p[0] - b[9], p[1] - b[10], p[2] - b[11]]
    };
    let (r0, r1, r2, t) = (
        linear(0),
        linear(3),
        linear(6),
        apply(b, [a[9], a[10], a[11]]),
    );
    [
        r0[0], r0[1], r0[2], r1[0], r1[1], r1[2], r2[0], r2[1], r2[2], t[0], t[1], t[2],
    ]
}

/// An object placed by a build item or a component: which part (`None`: the one placing it),
/// which object, and where.
struct Placement {
    part: Option<String>,
    id: u32,
    m: Mat,
}

#[derive(Default)]
struct Object {
    verts: Vec<[f32; 3]>,
    tris: Vec<[u32; 3]>,
    components: Vec<Placement>,
}

#[derive(Default)]
struct Part {
    objects: HashMap<u32, Object>,
    build: Vec<Placement>,
}

/// What one package may still hold and read.
struct Caps {
    verts: usize,
    tris: usize,
    placements: usize,
    bytes: u64,
}

/// The tags of an XML stream, one at a time, in `buf` (what lies between `<` and `>`). Text,
/// comments and declarations need no special case: nothing in them is named like the elements
/// read here, and a hostile file could as well write real ones.
struct Tags<R> {
    r: R,
    buf: Vec<u8>,
    left: u64,
}

impl<R: BufRead> Tags<R> {
    fn new(r: R, left: u64) -> Self {
        Tags {
            r,
            buf: Vec::new(),
            left,
        }
    }

    /// The next tag into `buf`; `None` at the end of the stream, a read error, or the byte budget.
    fn next(&mut self) -> Option<()> {
        loop {
            self.scan(b'<', false)?;
            self.buf.clear();
            if self.scan(b'>', true)? {
                return Some(());
            }
        }
    }

    /// Read up to and past `stop`, keeping what comes before it in `buf` when `keep`. `false`
    /// when a kept tag ran past [`MAX_TAG`] (it is read to its end and dropped).
    fn scan(&mut self, stop: u8, keep: bool) -> Option<bool> {
        let mut fits = true;
        loop {
            let avail = self.r.fill_buf().ok()?;
            if avail.is_empty() || self.left == 0 {
                return None;
            }
            let at = avail.iter().position(|&b| b == stop);
            let take = at.unwrap_or(avail.len());
            if keep && fits {
                fits = self.buf.len() + take <= MAX_TAG;
                if fits {
                    self.buf.extend_from_slice(&avail[..take]);
                }
            }
            let used = at.map_or(take, |i| i + 1);
            self.r.consume(used);
            self.left = self.left.saturating_sub(used as u64);
            if at.is_some() {
                return Some(fits);
            }
        }
    }
}

/// A tag's parts: whether it closes an element (`</x>`), its name without a namespace prefix,
/// and the attribute text after it.
fn split_tag(t: &[u8]) -> (bool, &[u8], &[u8]) {
    let (close, body) = match t.strip_prefix(b"/") {
        Some(rest) => (true, rest),
        None => (false, t),
    };
    let end = body
        .iter()
        .position(|b| b.is_ascii_whitespace() || *b == b'/')
        .unwrap_or(body.len());
    (close, local(&body[..end]), &body[end..])
}

/// A name without its namespace prefix.
fn local(name: &[u8]) -> &[u8] {
    name.iter()
        .rposition(|&b| b == b':')
        .map_or(name, |i| &name[i + 1..])
}

/// Every `name="value"` (or single-quoted) attribute in `rest`, names without their prefix.
fn for_each_attr<'a>(mut rest: &'a [u8], mut f: impl FnMut(&'a [u8], &'a [u8])) {
    while let Some(eq) = rest.iter().position(|&b| b == b'=') {
        let name = local(rest[..eq].trim_ascii());
        let after = rest[eq + 1..].trim_ascii_start();
        let Some((&quote, value)) = after.split_first() else {
            return;
        };
        if quote != b'"' && quote != b'\'' {
            return;
        }
        let Some(end) = value.iter().position(|&b| b == quote) else {
            return;
        };
        f(name, &value[..end]);
        rest = &value[end + 1..];
    }
}

fn num<T: std::str::FromStr>(v: &[u8]) -> Option<T> {
    std::str::from_utf8(v).ok()?.trim().parse().ok()
}

/// A `<vertex>`'s position; a coordinate that is missing or not a number is NaN, so the vertex
/// keeps its index and only the triangles using it drop.
fn vertex(rest: &[u8]) -> [f32; 3] {
    let mut p = [f32::NAN; 3];
    for_each_attr(rest, |name, value| {
        let slot = match name {
            b"x" => 0,
            b"y" => 1,
            b"z" => 2,
            _ => return,
        };
        p[slot] = num(value).unwrap_or(f32::NAN);
    });
    p
}

fn triangle(rest: &[u8]) -> Option<[u32; 3]> {
    let mut v = [None; 3];
    for_each_attr(rest, |name, value| {
        let slot = match name {
            b"v1" => 0,
            b"v2" => 1,
            b"v3" => 2,
            _ => return,
        };
        v[slot] = num(value);
    });
    Some([v[0]?, v[1]?, v[2]?])
}

/// A part name as the zip stores it: a 3MF path is absolute within the package.
fn part_name(path: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(path).ok()?.trim();
    let s = s.strip_prefix('/').unwrap_or(s);
    (!s.is_empty()).then(|| s.to_owned())
}

/// A transform's twelve numbers; anything else (missing, short, not finite) is no transform.
fn transform(v: &[u8]) -> Mat {
    let Ok(text) = std::str::from_utf8(v) else {
        return IDENTITY;
    };
    let mut m = IDENTITY;
    let mut n = 0;
    for word in text.split_ascii_whitespace() {
        match (m.get_mut(n), word.parse::<f32>()) {
            (Some(slot), Ok(x)) if x.is_finite() => *slot = x,
            _ => return IDENTITY,
        }
        n += 1;
    }
    if n == 12 {
        m
    } else {
        IDENTITY
    }
}

/// A `<component>`'s or build `<item>`'s placement; `None` without an object id.
fn placement(rest: &[u8]) -> Option<Placement> {
    let (mut id, mut part, mut m) = (None, None, IDENTITY);
    for_each_attr(rest, |name, value| match name {
        b"objectid" => id = num(value),
        b"path" => part = part_name(value),
        b"transform" => m = transform(value),
        _ => {}
    });
    Some(Placement { part, id: id?, m })
}

fn object_id(rest: &[u8]) -> Option<u32> {
    let mut id = None;
    for_each_attr(rest, |name, value| {
        if name == b"id" {
            id = num(value);
        }
    });
    id
}

/// One part's objects and build items, read off its tags.
struct PartReader {
    part: Part,
    current: Option<(u32, Object)>,
    in_build: bool,
}

impl PartReader {
    fn tag(&mut self, t: &[u8], caps: &mut Caps) {
        match split_tag(t) {
            (false, b"object", rest) => {
                self.finish_object();
                self.current = object_id(rest).map(|id| (id, Object::default()));
            }
            (true, b"object", _) => self.finish_object(),
            (false, b"build", rest) => self.in_build = !rest.ends_with(b"/"),
            (true, b"build", _) => self.in_build = false,
            (false, b"item", rest) if self.in_build => {
                if let Some(p) = take(&mut caps.placements).and_then(|()| placement(rest)) {
                    self.part.build.push(p);
                }
            }
            (false, name, rest) => self.object_tag(name, rest, caps),
            _ => {}
        }
    }

    /// A tag inside the current `<object>`: a vertex, a triangle or a component.
    fn object_tag(&mut self, name: &[u8], rest: &[u8], caps: &mut Caps) {
        let Some((_, obj)) = self.current.as_mut() else {
            return;
        };
        match name {
            b"vertex" if take(&mut caps.verts).is_some() => obj.verts.push(vertex(rest)),
            b"triangle" => {
                if let Some(t) = triangle(rest).filter(|_| take(&mut caps.tris).is_some()) {
                    obj.tris.push(t);
                }
            }
            b"component" => {
                if let Some(p) = take(&mut caps.placements).and_then(|()| placement(rest)) {
                    obj.components.push(p);
                }
            }
            _ => {}
        }
    }

    fn finish_object(&mut self) {
        if let Some((id, obj)) = self.current.take() {
            self.part.objects.insert(id, obj);
        }
    }
}

/// Spend one of a cap; `None` once it is gone.
fn take(left: &mut usize) -> Option<()> {
    *left = left.checked_sub(1)?;
    Some(())
}

/// Read one model part off `r`, spending `caps`.
fn read_part(r: impl BufRead, caps: &mut Caps) -> Part {
    let mut tags = Tags::new(r, caps.bytes);
    let mut reader = PartReader {
        part: Part::default(),
        current: None,
        in_build: false,
    };
    while tags.next().is_some() {
        reader.tag(&tags.buf, caps);
    }
    // An object cut off by the end of the budget keeps what was read of it.
    reader.finish_object();
    caps.bytes = tags.left;
    reader.part
}

/// The zip entry for a part name: exact, else the same name in another case (OPC part names
/// are case-insensitive), else the name with its `%XX` escapes decoded (a part URI may carry
/// them; OrcaSlicer writes its spaces bare).
fn entry_name<R: Read + Seek>(zip: &ZipArchive<R>, name: &str) -> Option<String> {
    if zip.index_for_name(name).is_some() {
        return Some(name.to_owned());
    }
    let unescaped = percent_decoded(name);
    zip.file_names()
        .take(super::MAX_LIST_ENTRIES)
        .flatten()
        .find(|n| n.eq_ignore_ascii_case(name) || unescaped.as_deref() == Some(&**n))
        .map(|n| n.into_owned())
}

/// `name` with its `%XX` escapes decoded; `None` when it has none or they do not decode.
fn percent_decoded(name: &str) -> Option<String> {
    if !name.contains('%') {
        return None;
    }
    let (b, mut out, mut i) = (name.as_bytes(), Vec::with_capacity(name.len()), 0);
    while i < b.len() {
        let hex = match b[i] {
            b'%' => b
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(std::str::from_utf8(h).ok()?, 16).ok()),
            _ => None,
        };
        if let Some(byte) = hex {
            out.push(byte);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// The root model part: the target of `_rels/.rels`'s 3D-model relationship, else
/// [`ROOT_DEFAULT`]. `None` when the package has neither, which is to say it is not a 3MF.
pub(super) fn root_part<R: Read + Seek>(zip: &mut ZipArchive<R>) -> Option<String> {
    let named = super::zipfmt::read_named(zip, "_rels/.rels").and_then(|rels| rels_root(&rels));
    named
        .and_then(|n| entry_name(zip, &n))
        .or_else(|| entry_name(zip, ROOT_DEFAULT))
}

/// The `Target` of the relationship whose `Type` is the 3MF model's.
fn rels_root(rels: &[u8]) -> Option<String> {
    let mut tags = Tags::new(rels, rels.len() as u64);
    while tags.next().is_some() {
        let (close, name, rest) = split_tag(&tags.buf);
        if close || name != b"Relationship" {
            continue;
        }
        let (mut is_model, mut target) = (false, None);
        for_each_attr(rest, |name, value| match name {
            b"Type" => is_model = value.ends_with(b"/3dmodel"),
            b"Target" => target = part_name(value),
            _ => {}
        });
        if is_model {
            return target;
        }
    }
    None
}

/// Every part the model reaches from its root, read once each.
struct Model {
    root: String,
    parts: HashMap<String, Part>,
}

impl Model {
    fn load<R: Read + Seek>(zip: &mut ZipArchive<R>, root: String) -> Model {
        let mut caps = Caps {
            verts: MAX_VERTS,
            tris: MAX_TRIS,
            placements: MAX_PLACEMENTS,
            bytes: MAX_XML_BYTES,
        };
        let mut parts = HashMap::new();
        let mut seen = HashSet::from([root.clone()]);
        let mut queue = vec![root.clone()];
        while let Some(name) = queue.pop() {
            let part = entry_name(zip, &name)
                .and_then(|entry| {
                    zip.by_name(&entry)
                        .ok()
                        .map(|f| read_part(BufReader::new(f), &mut caps))
                })
                .unwrap_or_default();
            for path in part.paths() {
                if seen.len() < MAX_PARTS && seen.insert(path.to_owned()) {
                    queue.push(path.to_owned());
                }
            }
            parts.insert(name, part);
        }
        Model { root, parts }
    }

    /// Every triangle of the model, placed, to `push` until it answers `false`.
    fn emit(&self, push: &mut dyn FnMut([f32; 9]) -> bool) {
        let Some(root) = self.parts.get(&self.root) else {
            return;
        };
        let mut walk = Walk {
            model: self,
            push,
            left: MAX_EMIT,
        };
        if root.build.is_empty() {
            // No build list (not valid 3MF, but written): every object of the root, as it is.
            for obj in root.objects.values() {
                if !walk.mesh(obj, &IDENTITY) {
                    return;
                }
            }
            return;
        }
        for item in &root.build {
            if !walk.place(&self.root, item, &IDENTITY, 0) {
                return;
            }
        }
    }
}

impl Part {
    /// The other parts this one's components and build items point at.
    fn paths(&self) -> impl Iterator<Item = &str> {
        let components = self.objects.values().flat_map(|o| &o.components);
        components
            .chain(&self.build)
            .filter_map(|p| p.part.as_deref())
    }
}

/// One walk of the placement graph, with what it may still visit and emit.
struct Walk<'m, 'p> {
    model: &'m Model,
    push: &'p mut dyn FnMut([f32; 9]) -> bool,
    left: u64,
}

impl Walk<'_, '_> {
    /// Draw the object `p` places, inside `parent`, from the part `here`; `false` once the
    /// walk must stop (the sink is full or the work is spent).
    fn place(&mut self, here: &str, p: &Placement, parent: &Mat, depth: u32) -> bool {
        if depth > MAX_DEPTH {
            return true;
        }
        let Some(left) = self.left.checked_sub(1) else {
            return false;
        };
        self.left = left;
        let model = self.model;
        let part = p.part.as_deref().unwrap_or(here);
        let Some(obj) = model.parts.get(part).and_then(|q| q.objects.get(&p.id)) else {
            return true;
        };
        let m = then(&p.m, parent);
        if !self.mesh(obj, &m) {
            return false;
        }
        obj.components
            .iter()
            .all(|c| self.place(part, c, &m, depth + 1))
    }

    fn mesh(&mut self, obj: &Object, m: &Mat) -> bool {
        for t in &obj.tris {
            let Some(left) = self.left.checked_sub(1) else {
                return false;
            };
            self.left = left;
            if let Some(tri) = placed(obj, t, m) {
                if !(self.push)(tri) {
                    return false;
                }
            }
        }
        true
    }
}

/// One triangle's corners, placed by `m`; `None` when an index is out of range or a corner is
/// not a finite point.
fn placed(obj: &Object, t: &[u32; 3], m: &Mat) -> Option<[f32; 9]> {
    let mut out = [0f32; 9];
    for (k, &i) in t.iter().enumerate() {
        let v = apply(m, *obj.verts.get(usize::try_from(i).ok()?)?);
        if !v.iter().all(|c| c.is_finite()) {
            return None;
        }
        out[k * 3..k * 3 + 3].copy_from_slice(&v);
    }
    Some(out)
}

/// The model of a 3MF package, drawn at `edge` px; `None` when the zip is not a 3MF or nothing
/// in it draws.
pub(super) fn draw<R: Read + Seek>(zip: &mut ZipArchive<R>, edge: u32) -> Option<DynamicImage> {
    let root = root_part(zip)?;
    let model = Model::load(zip, root);
    crate::decode::draw_triangles(edge, |push| model.emit(push))
}

/// The fuzz seed: a picture-less package in Bambu's and OrcaSlicer's shape (`tests`).
#[cfg(test)]
pub(super) fn seed() -> Vec<u8> {
    tests::bambu_shaped_package()
}

#[cfg(test)]
mod tests;

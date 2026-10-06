"""Growers for 3D models whose size is their triangles: a scan or a sculpt (see ballast.py).

A `tail` twin is the sample's few triangles and then ballast, so it never asked the reader to
parse millions of faces, and never had the shape every big exporter writes: Blender, Maya,
MeshLab and ZBrush put every vertex of an OBJ before its first face, which is how an OBJ past
64 KB stopped being recognised as one with no gate noticing. These growers cut each of the
sample's triangles into a fine grid of coplanar triangles, so the model draws the same picture,
and write it the way the exporters do: an OBJ's vertices first, a binary STL, a
binary_little_endian PLY. Real text and numbers, so they are PHYSICAL (ballast.py)."""

import math
import re
import struct


def _obj_tris(data):
    verts, tris = [], []
    for line in data.decode("utf-8", "replace").splitlines():
        parts = line.split()
        if not parts:
            continue
        if parts[0] == "v":
            verts.append(tuple(float(x) for x in parts[1:4]))
        elif parts[0] == "f":
            idx = [int(p.split("/")[0]) for p in parts[1:]]
            idx = [i - 1 if i > 0 else len(verts) + i for i in idx]
            tris += [(verts[idx[0]], verts[idx[k]], verts[idx[k + 1]]) for k in range(1, len(idx) - 1)]
    return tris


def _stl_tris(data):
    count = struct.unpack("<I", data[80:84])[0] if len(data) >= 84 else 0
    if len(data) == 84 + 50 * count:
        return [tuple(struct.unpack("<3f", data[at + 12 + 12 * k:at + 24 + 12 * k]) for k in range(3))
                for at in range(84, len(data), 50)]
    pts = [tuple(float(x) for x in m.groups())
           for m in re.finditer(rb"vertex\s+(\S+)\s+(\S+)\s+(\S+)", data)]
    return [tuple(pts[i:i + 3]) for i in range(0, len(pts) - 2, 3)]


def _ply_tris(data):
    head, _, body = data.partition(b"end_header")
    if b"format ascii" not in head:
        raise ValueError("only an ASCII PLY sample is read")
    nv = int(re.search(rb"element vertex (\d+)", head).group(1))
    nf = int(re.search(rb"element face (\d+)", head).group(1))
    lines = body.decode("ascii").split("\n")[1:]
    verts = [tuple(float(x) for x in line.split()[:3]) for line in lines[:nv]]
    tris = []
    for line in lines[nv:nv + nf]:
        nums = [int(x) for x in line.split()]
        idx = nums[1:1 + nums[0]]
        tris += [(verts[idx[0]], verts[idx[k]], verts[idx[k + 1]]) for k in range(1, len(idx) - 1)]
    return tris


class _Grid:
    """Triangle `tri` cut into n*n coplanar triangles with its winding, produced a row at a time
    (a 300 MB twin is millions of them). Point (i, j) is a + i*u + j*v, numbered row by row."""

    def __init__(self, tri, n):
        a, b, c = tri
        self.a, self.n = a, n
        self.u = [(b[k] - a[k]) / n for k in range(3)]
        self.v = [(c[k] - a[k]) / n for k in range(3)]
        self.points = (n + 1) * (n + 2) // 2
        self.faces = n * n

    def at(self, i, j):
        return i * (self.n + 1) - i * (i - 1) // 2 + j

    def point(self, i, j):
        a, u, v = self.a, self.u, self.v
        return (a[0] + u[0] * i + v[0] * j, a[1] + u[1] * i + v[1] * j, a[2] + u[2] * i + v[2] * j)

    def point_rows(self):
        for i in range(self.n + 1):
            yield [self.point(i, j) for j in range(self.n + 1 - i)]

    def face_rows(self):
        """Each row's faces as (i, j) corner triples."""
        n = self.n
        for i in range(n):
            row = []
            for j in range(n - i):
                row.append(((i, j), (i + 1, j), (i, j + 1)))
                if i + j < n - 1:
                    row.append(((i + 1, j), (i + 1, j + 1), (i, j + 1)))
            yield row


def _cuts(tris, per_face, per_point, size):
    """The grid size that makes `tris` at least `size` bytes, at about `per_face` bytes a face
    and `per_point` a vertex (a grid of n has n*n faces and (n+1)(n+2)/2 vertices)."""
    unit = len(tris) * (per_face + per_point / 2)
    return max(1, math.ceil(math.sqrt(size / unit) * 1.05))


def obj_dense(src, dst, size):
    """Every vertex first, then every face: the exporters' shape."""
    tris = _obj_tris(open(src, "rb").read())
    n = _cuts(tris, 25, 34, size)
    grids = [_Grid(t, n) for t in tris]
    with open(dst, "w", encoding="ascii", newline="\n") as out:
        out.write("# grown by the big-file gate: each triangle of the sample, cut into a grid\n")
        for g in grids:
            for row in g.point_rows():
                out.write("".join("v %.6f %.6f %.6f\n" % p for p in row))
        base = 1
        for g in grids:
            for row in g.face_rows():
                out.write("".join("f %d %d %d\n" % tuple(g.at(*c) + base for c in f) for f in row))
            base += g.points


def stl_dense(src, dst, size):
    """A binary STL, as a scanner or slicer writes a big one."""
    tris = _stl_tris(open(src, "rb").read())
    n = _cuts(tris, 50, 0, size)
    grids = [_Grid(t, n) for t in tris]
    facet = struct.Struct("<12fH")
    with open(dst, "wb") as out:
        out.write(b"grown by the big-file gate".ljust(80, b" "))
        out.write(struct.pack("<I", sum(g.faces for g in grids)))
        for g, tri in zip(grids, tris):
            normal = _normal(*tri)
            for row in g.face_rows():
                out.write(b"".join(facet.pack(*normal, *(x for c in f for x in g.point(*c)), 0) for f in row))


def ply_dense(src, dst, size):
    """A binary_little_endian PLY, as photogrammetry tools write one."""
    tris = _ply_tris(open(src, "rb").read())
    n = _cuts(tris, 13, 12, size)
    grids = [_Grid(t, n) for t in tris]
    head = ("ply\nformat binary_little_endian 1.0\ncomment grown by the big-file gate\n"
            f"element vertex {sum(g.points for g in grids)}\n"
            "property float x\nproperty float y\nproperty float z\n"
            f"element face {sum(g.faces for g in grids)}\n"
            "property list uchar int vertex_index\nend_header\n")
    point, face = struct.Struct("<3f"), struct.Struct("<B3i")
    with open(dst, "wb") as out:
        out.write(head.encode("ascii"))
        for g in grids:
            for row in g.point_rows():
                out.write(b"".join(point.pack(*p) for p in row))
        base = 0
        for g in grids:
            for row in g.face_rows():
                out.write(b"".join(face.pack(3, *(g.at(*c) + base for c in f)) for f in row))
            base += g.points


def _normal(a, b, c):
    u = [b[k] - a[k] for k in range(3)]
    v = [c[k] - a[k] for k in range(3)]
    x = (u[1] * v[2] - u[2] * v[1], u[2] * v[0] - u[0] * v[2], u[0] * v[1] - u[1] * v[0])
    length = math.sqrt(sum(k * k for k in x)) or 1.0
    return tuple(k / length for k in x)

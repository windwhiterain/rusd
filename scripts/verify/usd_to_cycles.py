"""USD -> Cycles XML converter (throwaway M6 verification tooling, target/).

The local Cycles 5.3.0 standalone build's direct-USD mode produces an empty
film (integration bug), while its XML mode renders correctly. So: read the
exported stage with the REFERENCE pxr implementation, tessellate gprims, and
emit Cycles XML.

Calibrated empirically against this build:
- XML camera looks along local +Z (NOT -Z like USD); translate = position;
  rotate = right-handed column-convention axis-angle in degrees.
- XML lights need an emission shader bound via an enclosing <state>;
  the old examples' bare <light strength=...> yields 0 lights (probe_sun.xml
  renders black in this build). Sun shines along local -Z of its transform.

usage: python usd_to_cycles.py <input.usd[c]> <output.xml> [width height]
"""

import math
import sys

from pxr import Gf, Usd, UsdGeom

src, dst = sys.argv[1], sys.argv[2]
W = int(sys.argv[3]) if len(sys.argv) > 3 else 960
H = int(sys.argv[4]) if len(sys.argv) > 4 else 540

stage = Usd.Stage.Open(src)


def world_matrix(prim):
    return UsdGeom.Xformable(prim).ComputeLocalToWorldTransform(Usd.TimeCode.Default())


def rot3(m):
    r = Gf.Matrix3d(*[m.GetRow(i)[j] for i in range(3) for j in range(3)])
    for i in range(3):
        row = r.GetRow(i)
        n = row.GetLength()
        if n > 0:
            r.SetRow(i, row / n)
    return r


def xp(m, p):
    return m.Transform(Gf.Vec3d(*p))


def xn(r, n):
    return Gf.Vec3d(*n) * r


def fmt_vecs(vs):
    return " ".join(f"{v[0]:.6f} {v[1]:.6f} {v[2]:.6f}" for v in vs)


parts = []  # (shader, P, N, nverts, verts)


def add_mesh(shader, P, N, nverts, verts):
    parts.append((shader, P, N, nverts, verts))


for prim in stage.Traverse():
    t = str(prim.GetTypeName())
    m = world_matrix(prim)
    r = rot3(m)
    if t == "Cube":
        s = UsdGeom.Cube(prim).GetSizeAttr().Get() or 2.0
        h = s / 2.0
        corners = [(-h, -h, -h), (h, -h, -h), (h, h, -h), (-h, h, -h),
                   (-h, -h, h), (h, -h, h), (h, h, h), (-h, h, h)]
        faces = [(0, 1, 2, 3), (4, 7, 6, 5), (0, 4, 5, 1),
                 (1, 5, 6, 2), (2, 6, 7, 3), (4, 0, 3, 7)]
        normals = [(0, 0, -1), (0, 0, 1), (0, -1, 0), (1, 0, 0), (0, 1, 0), (-1, 0, 0)]
        P, N, nv, vi = [], [], [], []
        for f, n in zip(faces, normals):
            base = len(P)
            for i in f:
                P.append(xp(m, corners[i]))
                N.append(xn(r, n))
            nv.append(4)
            vi.extend([base, base + 1, base + 2, base + 3])
        add_mesh("matte_gray", P, N, nv, vi)
    elif t == "Sphere":
        rad = UsdGeom.Sphere(prim).GetRadiusAttr().Get() or 1.0
        seg, ring = 16, 8
        P, N = [], []
        for i in range(ring + 1):
            phi = math.pi * i / ring
            for j in range(seg):
                theta = 2 * math.pi * j / seg
                n = (math.sin(phi) * math.cos(theta), math.sin(phi) * math.sin(theta), math.cos(phi))
                P.append(xp(m, tuple(rad * c for c in n)))
                N.append(xn(r, n))
        nv, vi = [], []
        for i in range(ring):
            for j in range(seg):
                a = i * seg + j
                b = i * seg + (j + 1) % seg
                c = (i + 1) * seg + (j + 1) % seg
                d = (i + 1) * seg + j
                if i > 0:
                    nv.append(3)
                    vi.extend([a, b, c])
                if i < ring - 1:
                    nv.append(3)
                    vi.extend([a, c, d])
        add_mesh("matte_green", P, N, nv, vi)
    elif t == "Mesh":
        g = UsdGeom.Mesh(prim)
        pts = g.GetPointsAttr().Get() or []
        counts = g.GetFaceVertexCountsAttr().Get() or []
        idx = g.GetFaceVertexIndicesAttr().Get() or []
        P = [xp(m, p) for p in pts]
        nrm = g.GetNormalsAttr().Get()
        N = [xn(r, n) for n in nrm] if nrm else None
        nv, vi = [], []
        k = 0
        for c in counts:
            face = idx[k:k + c]
            k += c
            for q in range(1, c - 1):
                nv.append(3)
                vi.extend([face[0], face[q], face[q + 1]])
        add_mesh("matte_gray", P, N, nv, vi)

# camera: first UsdGeomCamera (same rule as hdCycles' file reader), rebuilt as
# a +Z-forward look-at because Cycles XML cameras look along +Z.
cam_xml = ""
for prim in stage.Traverse():
    if prim.IsA(UsdGeom.Camera):
        c = UsdGeom.Camera(prim)
        m = world_matrix(prim)
        eye = m.ExtractTranslation()
        zback = Gf.Vec3d(*[m.GetRow(2)[j] for j in range(3)])
        up = Gf.Vec3d(*[m.GetRow(1)[j] for j in range(3)])
        f = (-zback).GetNormalized()
        x = Gf.Cross(up, f)
        if x.GetLength() < 1e-6:
            x = Gf.Vec3d(1, 0, 0)
        x = x.GetNormalized()
        y = Gf.Cross(f, x)
        tr = x[0] + y[1] + f[2]
        cosang = max(-1.0, min(1.0, (tr - 1.0) / 2.0))
        ang = math.degrees(math.acos(cosang))
        s = 2 * math.sin(math.radians(ang))
        if s < 1e-8:
            axis, ang = Gf.Vec3d(0, 0, 1), 0.0
        else:
            axis = Gf.Vec3d((y[2] - f[1]) / s, (f[0] - x[2]) / s, (x[1] - y[0]) / s)
        focal = c.GetFocalLengthAttr().Get() or 35.0
        vap = c.GetVerticalApertureAttr().Get() or 24.0
        fov = 2 * math.atan(vap / (2 * focal))
        cam_xml = (
            f'<transform translate="{eye[0]:.6f} {eye[1]:.6f} {eye[2]:.6f}" '
            f'rotate="{ang:.6f} {axis[0]:.6f} {axis[1]:.6f} {axis[2]:.6f}">\n'
            f'\t<camera width="{W}" height="{H}" type="perspective" fov="{fov:.9f}" />\n'
            f"</transform>"
        )
        break

out = ['<cycles>', '<film exposure="1" />',
       '<integrator seed="7" use_denoise="false" max_bounce="4" />', cam_xml,
       '<background>',
       '\t<background name="world" color="0.35, 0.45, 0.6" strength="0.3" />',
       '\t<connect from="world background" to="output surface" />',
       '</background>',
       '<shader name="matte_gray">',
       '\t<diffuse_bsdf name="b" color="0.75, 0.75, 0.78" />',
       '\t<connect from="b bsdf" to="output surface" />',
       '</shader>',
       '<shader name="matte_green">',
       '\t<diffuse_bsdf name="b" color="0.25, 0.5, 0.25" />',
       '\t<connect from="b bsdf" to="output surface" />',
       '</shader>',
       '<shader name="sun_key">',
       '\t<emission name="e" color="1, 0.98, 0.95" strength="1" />',
       '\t<connect from="e emission" to="output surface" />',
       '</shader>',
       '<shader name="sun_fill">',
       '\t<emission name="e" color="0.85, 0.9, 1" strength="0.3" />',
       '\t<connect from="e emission" to="output surface" />',
       '</shader>',
       '<state shader="sun_key">',
       '\t<transform rotate="30 1 0 0">',
       '\t\t<light name="key" light_type="sun" angle="0.53" />',
       '\t</transform>',
       '</state>',
       '<state shader="sun_fill">',
       '\t<transform rotate="-60 0 1 0">',
       '\t\t<light name="fill" light_type="sun" angle="5" />',
       '\t</transform>',
       '</state>']
for shader, P, N, nv, vi in parts:
    out.append(f'<state shader="{shader}">')
    attrs = f'P="{fmt_vecs(P)}" nverts="{" ".join(map(str, nv))}" verts="{" ".join(map(str, vi))}"'
    if N:
        attrs += f' standard_vertex_normal="{fmt_vecs(N)}"'
    out.append(f"\t<mesh {attrs} />")
    out.append("</state>")
out.append("</cycles>")

with open(dst, "w", encoding="utf-8") as f:
    f.write("\n".join(out) + "\n")
print(f"wrote {dst}: {len(parts)} meshes, camera={'yes' if cam_xml else 'NO'}")

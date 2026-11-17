"""Generate a review wrapper usda for an exported rusd stage: references the
export unchanged, overrides the camera with a bbox-fitted look-at rig, and adds
a sun + fill DistantLight (the export carries geometry + camera but no lights;
Cycles is a path tracer and renders black without them). Throwaway verification
tooling, lives in target/ (gitignored).

usage: python make_review.py <input.usd[c]> <output.usda>
"""

import os
import sys

from pxr import Gf, Usd, UsdGeom


def look_at_rows(eye, target, up=Gf.Vec3d(0, 0, 1)):
    z = (eye - target).GetNormalized()
    x = Gf.Cross(up, z).GetNormalized()
    y = Gf.Cross(z, x)
    return x, y, z


def mat4(rows, t):
    x, y, z = rows
    return Gf.Matrix4d(
        x[0], x[1], x[2], 0,
        y[0], y[1], y[2], 0,
        z[0], z[1], z[2], 0,
        t[0], t[1], t[2], 1,
    )


def fmt(m):
    rows = []
    for i in range(4):
        row = m.GetRow(i)
        rows.append("(" + ", ".join(f"{row[j]:.6f}" for j in range(4)) + ")")
    return "(" + ", ".join(rows) + ")"


def light_mat(dir_from):
    z = Gf.Vec3d(*dir_from).GetNormalized()
    up = Gf.Vec3d(0, 0, 1)
    if abs(Gf.Dot(up, z)) > 0.99:
        up = Gf.Vec3d(0, 1, 0)
    x = Gf.Cross(up, z).GetNormalized()
    y = Gf.Cross(z, x)
    return mat4((x, y, z), Gf.Vec3d(0, 0, 0))


src, dst = sys.argv[1], sys.argv[2]
stage = Usd.Stage.Open(src)
bbc = UsdGeom.BBoxCache(Usd.TimeCode.Default(), [UsdGeom.Tokens.default_])
rng = bbc.ComputeWorldBound(stage.GetDefaultPrim()).ComputeAlignedRange()
center = (rng.GetMin() + rng.GetMax()) * 0.5
size = rng.GetSize()
d = max(size[0], size[1])
eye = center + Gf.Vec3d(0.85 * d, -0.85 * d, 0.62 * d)
cam = mat4(look_at_rows(eye, center), eye)
sun = light_mat((0.4, -0.5, 0.75))
fill = light_mat((-0.6, -0.4, 0.3))
src_name = os.path.basename(src)

usda = f"""#usda 1.0
(
    defaultPrim = "World"
    upAxis = "Z"
    metersPerUnit = 1
)

def "World" (
    references = @./{src_name}@</World>
)
{{
    over "Camera"
    {{
        matrix4d xformOp:transform = {fmt(cam)}
        token[] xformOpOrder = ["xformOp:transform"]
        float focalLength = 35
        float horizontalAperture = 36
        float verticalAperture = 20.25
        float2 clippingRange = (0.1, 1000000)
    }}
}}

def DistantLight "ReviewSun"
{{
    float inputs:intensity = 3
    float inputs:angle = 0.53
    color3f inputs:color = (1, 0.98, 0.95)
    matrix4d xformOp:transform = {fmt(sun)}
    token[] xformOpOrder = ["xformOp:transform"]
}}

def DistantLight "ReviewFill"
{{
    float inputs:intensity = 0.8
    float inputs:angle = 5
    color3f inputs:color = (0.85, 0.9, 1)
    matrix4d xformOp:transform = {fmt(fill)}
    token[] xformOpOrder = ["xformOp:transform"]
}}
"""

with open(dst, "w", encoding="utf-8") as f:
    f.write(usda)
print(f"wrote {dst}: center={tuple(center)} size={tuple(size)} eye={tuple(eye)}")

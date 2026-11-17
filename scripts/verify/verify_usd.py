"""Independent M6 verification: open exported USD with the reference pxr
implementation (usd-core), NOT our openusd port. The export bakes generators
into gprims (Cube/Sphere/...), so validity is checked per gprim schema, plus
stage metadata and world bounds. Lives in target/ (gitignored)."""

import sys

from pxr import Gf, Sdf, Usd, UsdGeom

GPRIMS = (UsdGeom.Mesh, UsdGeom.Cube, UsdGeom.Sphere, UsdGeom.Cylinder, UsdGeom.Cone, UsdGeom.Capsule)


def gprim_ok(prim) -> bool:
    if prim.IsA(UsdGeom.Mesh):
        pts = UsdGeom.Mesh(prim).GetPointsAttr().Get()
        return bool(pts) and len(pts) >= 3
    if prim.IsA(UsdGeom.Cube):
        s = UsdGeom.Cube(prim).GetSizeAttr().Get()
        return s is not None and s > 0
    if prim.IsA(UsdGeom.Sphere):
        r = UsdGeom.Sphere(prim).GetRadiusAttr().Get()
        return r is not None and r > 0
    if prim.IsA(UsdGeom.Cylinder):
        g = UsdGeom.Cylinder(prim)
        return g.GetRadiusAttr().Get() > 0 and g.GetHeightAttr().Get() > 0
    if prim.IsA(UsdGeom.Cone):
        g = UsdGeom.Cone(prim)
        return g.GetRadiusAttr().Get() > 0 and g.GetHeightAttr().Get() > 0
    if prim.IsA(UsdGeom.Capsule):
        g = UsdGeom.Capsule(prim)
        return g.GetRadiusAttr().Get() > 0 and g.GetHeightAttr().Get() > 0
    return False


def check(path: str, expect_prims: int) -> bool:
    stage = Usd.Stage.Open(path)
    if stage is None:
        print(f"== {path}\nCANNOT OPEN")
        return False
    prims = list(stage.Traverse())
    geo = [p for p in prims if any(p.IsA(t) for t in GPRIMS)]
    bad = [p.GetPath() for p in geo if not gprim_ok(p)]
    kinds = {}
    for p in geo:
        kinds[p.GetTypeName()] = kinds.get(p.GetTypeName(), 0) + 1
    print(f"== {path}")
    print(f"prims: {len(prims)} (expect {expect_prims})")
    dp = stage.GetDefaultPrim()
    print(f"defaultPrim: {dp.GetPath() if dp else None}")
    print(f"upAxis: {UsdGeom.GetStageUpAxis(stage)}  metersPerUnit: {UsdGeom.GetStageMetersPerUnit(stage)}")
    print(f"geometry prims: {len(geo)}  kinds: {kinds}  invalid: {len(bad)}")
    if bad:
        print("invalid paths:", bad[:10])

    # sample one gprim with its world transform, for the record
    if geo:
        s = geo[len(geo) // 2]
        xf = UsdGeom.Xformable(s).ComputeLocalToWorldTransform(Usd.TimeCode.Default())
        t = xf.ExtractTranslation()
        print(f"sample: {s.GetPath()} ({s.GetTypeName()}) world translation: {tuple(t)}")

    bbc = UsdGeom.BBoxCache(Usd.TimeCode.Default(), [UsdGeom.Tokens.default_, UsdGeom.Tokens.render])
    root = dp if dp else stage.GetPseudoRoot()
    rng = bbc.ComputeWorldBound(root).ComputeAlignedRange()
    mn, mx = rng.GetMin(), rng.GetMax()
    size = rng.GetSize()
    print(f"world bbox min: {tuple(mn)}  max: {tuple(mx)}")
    print(f"bbox size: {tuple(size)}")

    finite = all(all(abs(c) < 1e9 for c in v) for v in (mn, mx))
    nonempty = size[0] > 0 and size[1] > 0 and size[2] > 0
    ok = len(prims) == expect_prims and len(geo) > 0 and not bad and finite and nonempty
    print(f"VERDICT: {'PASS' if ok else 'FAIL'}")
    return ok


results = [check("target/city.usdc", 475), check("target/forest.usda", 505)]
sys.exit(0 if all(results) else 1)

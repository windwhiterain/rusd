"""D10 example: an agent builds a forest from one prompt, via the edit loop.

Starts from a minimal scene (a prototype sphere), then:
  query → patch(define a RusdScatter) → validate → expand → render → pick → undo/redo.

Run:  python example_build_forest.py
Needs a working Python 3 + the `rusd` binary (RUSD_BIN or target/debug). Rendering
needs the machine's GPU; the editing steps (open/patch/expand/undo) do not.
"""

from pathlib import Path

from rusd_client import RusdClient

MINIMAL = """\
[stage]
default_prim = "/World"
seed = 1
up_axis = "Z"

[World]
schema = "Xform"

[World.Prototypes]
schema = "Xform"

[World.Prototypes.Pine]
radius = 0.5
schema = "Sphere"
"""


def main():
    with RusdClient() as c:
        print("1. open minimal scene:", c.open(content=MINIMAL))
        print("   geo before:", c.expand()["geo_items"])

        print("2. patch: define a RusdScatter generator")
        r = c.define(
            "/World/Forest",
            "RusdScatter",
            count=500,
            prototype="/World/Prototypes/Pine",
            seed=42,
            max_prims=2000,
            domain={"dist": "rect", "w": 100.0, "h": 100.0},
            scale={"dist": "uniform", "min": 0.8, "max": 1.4},
            yaw={"dist": "uniform", "min": 0.0, "max": 360.0},
        )
        print("   applied:", r["applied"], "diags:", r["diags"])

        print("3. validate:", c.validate()["diags"])
        e = c.expand()
        print("4. expand: geo_items =", e["geo_items"], "(500 scatter + 1 prototype)")

        # Rendering needs a GPU; skip gracefully if unavailable.
        try:
            img = c.render(width=800, height=600)
            print("5. render:", img["beauty"], "foreground =", img["foreground"])
            print("6. pick(400,300):", c.pick(400, 300)["path"])
        except Exception as ex:  # noqa: BLE001
            print("5/6. render/pick skipped (no GPU?):", ex)

        print("7. undo -> geo:", end=" ")
        c.undo()
        print(c.expand()["geo_items"])
        print("   redo -> geo:", end=" ")
        c.redo()
        print(c.expand()["geo_items"])
        print("8. diff changed:", c.diff()["changed"])


if __name__ == "__main__":
    main()
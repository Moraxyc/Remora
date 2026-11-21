"""Emit a protoc-valid, nested gv_pb schema from protogen.py's JSON output."""
import json, sys

schema = json.load(open(sys.argv[1]))
header = sys.argv[3] if len(sys.argv) > 3 else ""

# Build a tree of nested names.
tree = {}
for full in schema:
    parts = full.split(".")
    node = tree
    for p in parts:
        node = node.setdefault(p, {})


def ftype(t):
    if t == "map<S2S>":
        return "map<string, string>"
    return t


def emit(name, path, indent):
    full = ".".join(path)
    pad = "  " * indent
    out = []
    info = schema.get(full)
    children = tree_at(path)
    if info is None:
        # container-only (should not happen)
        info = {"kind": "message", "fields": []}
    if info["kind"] == "enum":
        out.append(f"{pad}enum {name} {{")
        vals = info["values"]
        if not any(n == 0 for n, _ in vals):
            out.append(f"{pad}  // NOTE: no zero value in the binary's name map")
        for num, n in vals:
            out.append(f"{pad}  {n} = {num};")
        out.append(f"{pad}}}")
        return out
    out.append(f"{pad}message {name} {{")
    for child in sorted(children):
        out.extend(emit(child, path + [child], indent + 1))
    groups = {}
    for f in info["fields"]:
        if f.get("oneof"):
            groups.setdefault(f["oneof"], []).append(f)
    done_oneofs = set()
    for f in info["fields"]:
        o = f.get("oneof")
        if o:
            if o in done_oneofs:
                continue
            done_oneofs.add(o)
            snake = "".join("_" + c.lower() if c.isupper() else c for c in o).lstrip("_")
            out.append(f"{pad}  oneof {snake} {{")
            for g in groups[o]:
                out.append(f"{pad}    {ftype(g['type'])} {g['name']} = {g['num']};")
            out.append(f"{pad}  }}")
            continue
        label = "repeated " if f.get("repeated") else ("optional " if f.get("optional") else "")
        t = ftype(f["type"])
        if t.startswith("map<"):
            label = ""
        out.append(f"{pad}  {label}{t} {f['name']} = {f['num']};")
    out.append(f"{pad}}}")
    return out


def tree_at(path):
    node = tree
    for p in path:
        node = node[p]
    return node


lines = ['syntax = "proto3";', "", "package gv_pb;", ""]
if header:
    lines = [l for l in open(header).read().splitlines()] + lines
# keep binary order for top-level types
order = []
for full in schema:
    top = full.split(".")[0]
    if top not in order:
        order.append(top)
for top in order:
    lines.extend(emit(top, [top], 0))
    lines.append("")
open(sys.argv[2], "w").write("\n".join(lines))

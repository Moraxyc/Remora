"""Stage 2: reconstruct a .proto-like schema for the gv_pb package from a SwiftProtobuf binary.
Usage: protogen.py <macho> <out.proto> <out.json>"""
import sys, struct, re, json
import lief

path = sys.argv[1]
fat = lief.MachO.parse(path)
b = [fat.at(i) for i in range(fat.size) if fat.at(i).header.cpu_type == lief.MachO.Header.CPU_TYPE.X86_64][0]
sections = []
for seg in b.segments:
    for s in seg.sections:
        if s.size and s.type != lief.MachO.Section.TYPE.ZEROFILL:
            sections.append((s.virtual_address, s.virtual_address + s.size, bytes(s.content), seg.name, s.name))


def find(addr):
    for lo, hi, c, sg, sn in sections:
        if lo <= addr < hi:
            return c, addr - lo
    return None, None


def rd(addr, n):
    c, o = find(addr)
    return None if c is None else c[o:o + n]


def cstr(addr):
    c, o = find(addr)
    if c is None:
        return None
    return c[o:c.find(b"\0", o)]


def rel32(addr):
    return addr + struct.unpack("<i", rd(addr, 4))[0]


cs = [s for s in sections if s[4] == "__cstring"][0]
BIND = {}
for bi in b.bindings:
    try:
        if bi.has_symbol:
            BIND[bi.address] = bi.symbol.name
    except Exception:
        pass
print("binds", len(BIND), file=sys.stderr)
STD = {"$ss5Int32VMn": "s5Int32V", "$ss6UInt32VMn": "s6UInt32V", "$ss5Int64VMn": "s5Int64V", "$ss6UInt64VMn": "s6UInt64V",
       "$sSfMn": "Sf", "$sSdMn": "Sd", "$sSbMn": "Sb", "$sSSMn": "SS", "$s10Foundation4DataVMn": "10Foundation4DataV"}

# ---------- name maps
maps = []
for lo, hi, c, sg, sn in sections:
    if sg not in ("__DATA", "__DATA_CONST"):
        continue
    n, i = len(c), 0
    while i + 32 + 56 <= n:
        isa, rc, count, cap = struct.unpack_from("<QQQQ", c, i)
        if 0 < count < 2000 and cap == count * 2 and i + 32 + 56 * count <= n:
            ok, entries = True, []
            for k in range(count):
                q = struct.unpack_from("<7Q", c, i + 32 + 56 * k)
                if not (cs[0] <= q[1] < cs[1]):
                    ok = False
                    break
                s = cstr(q[1])
                if s is None or len(s) != q[2]:
                    ok = False
                    break
                num = q[0] - (1 << 64) if q[0] >= 1 << 63 else q[0]
                entries.append((num, s.decode(errors="replace")))
            if ok:
                maps.append(entries)
                i += 32 + 56 * count
                continue
        i += 8


# ---------- swift reflection
def ctx_name(addr, depth=0):
    flags = struct.unpack("<I", rd(addr, 4))[0]
    if flags & 0x1F == 0:
        return cstr(rel32(addr + 8)).decode()
    raw = struct.unpack("<i", rd(addr + 4, 4))[0]
    if flags & 0x1F in (1, 2):  # extension / anonymous context: no name, defer to parent
        p = addr + 4 + (raw & ~1)
        if raw & 1:
            p = struct.unpack("<Q", rd(p, 8))[0]
        return ctx_name(p, depth + 1) if depth < 6 else "?"
    name = (cstr(rel32(addr + 8)) or b"?").decode()
    if raw and depth < 6:
        p = addr + 4 + (raw & ~1)
        if raw & 1:
            p = struct.unpack("<Q", rd(p, 8))[0]
        try:
            if struct.unpack("<I", rd(p, 4))[0] & 0x1F != 0:
                return ctx_name(p, depth + 1) + "." + name
        except Exception:
            pass
    return name


def mangled(addr):
    out, a = [], addr
    while True:
        bt = rd(a, 1)
        if not bt or bt[0] == 0:
            break
        if 1 <= bt[0] <= 0x17:
            r = struct.unpack("<i", rd(a + 1, 4))[0]
            t = a + 1 + r
            if bt[0] == 2:
                sym = BIND.get(t)
                if sym is not None:
                    sym = sym.lstrip("_")
                    out.append(STD.get(sym, "<" + sym + ">"))
                    a += 5
                    continue
                t = struct.unpack("<Q", rd(t, 8))[0]
            try:
                out.append("<" + ctx_name(t) + ">")
            except Exception:
                out.append("<?>")
            a += 5
        else:
            out.append(chr(bt[0]))
            a += 1
    return "".join(out)


fm = [s for s in sections if s[4] == "__swift5_fieldmd"][0]
types = {}
a = fm[0]
while a < fm[1]:
    tn_rel = struct.unpack("<i", rd(a, 4))[0]
    kind, rsize, nf = struct.unpack("<HHI", rd(a + 8, 8))
    name = mangled(a + tn_rel) if tn_rel else "?"
    name = name[1:-1] if name.startswith("<") and name.endswith(">") and name.count("<") == 1 else name
    fields = []
    for k in range(nf):
        fa = a + 16 + k * rsize
        tr = struct.unpack("<i", rd(fa + 4, 4))[0]
        fnr = struct.unpack("<i", rd(fa + 8, 4))[0]
        fn = (cstr(fa + 8 + fnr) or b"").decode(errors="replace") if fnr else ""
        ft = mangled(fa + 4 + tr) if tr else ""
        fields.append((fn, ft))
    if name not in types:
        types[name] = (kind, fields)
    a += 16 + nf * rsize

SCALAR = {"s5Int32V": "int32", "s6UInt32V": "uint32", "s5Int64V": "int64", "s6UInt64V": "uint64", "SS": "string",
          "Sb": "bool", "Sf": "float", "Sd": "double", "10Foundation4DataV": "bytes", "SiSg": "int?"}


def is_opt(ft):
    return ft.endswith("Sg") and (ft[:-2] in SCALAR or ft[:-2].startswith("<GvPb_") and types.get(ft[:-3][1:], (0,))[0] == 2)


def pt(ft):
    rep = False
    opt = False
    m = re.fullmatch(r"Say(.*)G", ft)
    if m:
        rep, ft = True, m.group(1)
    if ft.endswith("Sg"):
        opt, ft = True, ft[:-2]
    m = re.fullmatch(r"SDy(.*)G", ft)
    if m:
        return "map<" + m.group(1) + ">", rep
    if ft in SCALAR:
        t = SCALAR[ft]
    elif ft.startswith("<") and ft.endswith(">"):
        t = ft[1:-1].replace("GvPb_", "")
    else:
        t = "?" + ft
    return t, rep


norm = lambda s: re.sub(r"[^a-z0-9]", "", re.sub(r"_p$", "", s).lower())


def type_fields(t):
    """name(normalized) -> (swift field name, proto-ish type, repeated) for message t, including storage + oneof."""
    res = {}
    srcs = [t, t + "._StorageClass"] + [x for x in types if x.startswith(t + ".OneOf_")]
    for s in srcs:
        if s not in types:
            continue
        for fn, ft in types[s][1]:
            if fn in ("unknownFields", "_storage") or not fn:
                continue
            if ft.startswith("<" + t + ".OneOf_"):
                continue
            res[norm(fn.lstrip("_"))] = (fn, ft)
    return res


order = [t for t in types if t.startswith("GvPb_") and not t.endswith("._StorageClass") and ".OneOf_" not in t]


def score(entries, t):
    tf = type_fields(t)
    keys = set(tf) - {"unrecognized"}
    names = [norm(n) for _, n in entries]
    inter = sum(1 for n in names if n in keys or any(n.endswith(k) and len(k) >= 2 for k in keys))
    u = len(names) + len(keys) - inter
    return inter / u if u else 0


# keep only gv maps: those before the first google descriptor map (heuristic: stop at 'paths' FieldMask or descriptor names)
gv_maps = []
for e in maps:
    if [n for _, n in e] in (["paths"],):
        break
    gv_maps.append(e)

# DP alignment
N, M = len(gv_maps), len(order)
NEG = -1e9
dp = [[NEG] * (M + 1) for _ in range(N + 1)]
bk = [[None] * (M + 1) for _ in range(N + 1)]
dp[0] = [0.0] * (M + 1)
for j in range(1, M + 1):
    bk[0][j] = "skipT"
for i in range(1, N + 1):
    dp[i][0] = dp[i - 1][0] - 1
    bk[i][0] = "skipM"
    for j in range(1, M + 1):
        best, how = dp[i - 1][j - 1] + score(gv_maps[i - 1], order[j - 1]) * 2 - 0.5, "match"
        if dp[i][j - 1] > best:
            best, how = dp[i][j - 1], "skipT"
        if dp[i - 1][j] - 1 > best:
            best, how = dp[i - 1][j] - 1, "skipM"
        dp[i][j], bk[i][j] = best, how
i, j = N, M
assign = {}
unassigned = []
while i > 0 or j > 0:
    how = bk[i][j]
    if how == "match":
        assign[order[j - 1]] = (gv_maps[i - 1], score(gv_maps[i - 1], order[j - 1]))
        i, j = i - 1, j - 1
    elif how == "skipT":
        j -= 1
    else:
        unassigned.append(gv_maps[i - 1])
        i -= 1

# emit
lines = ['syntax = "proto3";', "package gv_pb;", "",
         "// Reconstructed from SwiftProtobuf metadata of the official macOS UURemoteServer (4.48.1).",
         "// Field numbers and names are exact (from _protobuf_nameMap). Scalar types come from Swift",
         "// property types: int32 may really be sint32/sfixed32 (same Swift type) - see docs.", ""]
js = {}
for t in order:
    kind = types[t][0]
    entries, sc = assign.get(t, ([], None))
    short = t.replace("GvPb_", "")
    is_enum = kind == 2 and not types[t][1] or (entries and all(re.fullmatch(r"[A-Za-z0-9_]+", n) for _, n in entries) and kind == 2)
    # kind: 0 struct, 1 class, 2 enum, 3 multipayload enum
    if kind in (2, 3) and ".OneOf_" not in t:
        lines.append(f"enum {short} {{")
        for num, n in entries:
            lines.append(f"  {n} = {num};")
        lines.append("}")
        js[short] = {"kind": "enum", "values": [[num, n] for num, n in entries]}
    else:
        tf = type_fields(t)
        oneofs = {}
        for x in types:
            if x.startswith(t + ".OneOf_"):
                for fn, ft in types[x][1]:
                    oneofs[norm(fn)] = (x.split(".OneOf_")[1], ft)
        flds = []
        lines.append(f"message {short} {{" + (f"  // match={sc:.2f}" if sc is not None and sc < 0.99 else ""))
        for num, n in sorted(entries):
            k = norm(n)
            if k in oneofs:
                oname, ft = oneofs[k]
                ty, rep = pt(ft)
                flds.append({"num": num, "name": n, "type": ty, "repeated": rep, "oneof": oname})
                lines.append(f"  /*oneof {oname}*/ {ty} {n} = {num};")
            elif k in tf:
                ty, rep = pt(tf[k][1])
                op = is_opt(tf[k][1])
                flds.append({"num": num, "name": n, "type": ty, "repeated": rep, "optional": op})
                lines.append(f"  {'repeated ' if rep else ''}{'optional ' if op else ''}{ty} {n} = {num};")
            else:
                flds.append({"num": num, "name": n, "type": "?"})
                lines.append(f"  ? {n} = {num};")
        lines.append("}")
        js[short] = {"kind": "message", "fields": flds}
    lines.append("")
open(sys.argv[2], "w").write("\n".join(lines))
json.dump(js, open(sys.argv[3], "w"), indent=1, ensure_ascii=False)
print("types", len(order), "assigned", len(assign), "unassigned maps", len(unassigned))
for e in unassigned:
    print("UNASSIGNED", e[:6])
low = [(t, s) for t, (e, s) in assign.items() if s < 0.99]
for t, s in low:
    print("LOW", round(s, 2), t, [n for _, n in assign[t][0]][:6])
missing = [t for t in order if t not in assign]
print("no-map types:", missing)

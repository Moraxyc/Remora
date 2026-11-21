# Tools

Static-analysis helpers used to produce [../proto/gv_pb.proto](../proto/gv_pb.proto). They read an
official Mach-O binary; they never execute it.

| File | Purpose |
|---|---|
| `protogen.py` | Mach-O (fat or thin, uses the x86_64 slice) → raw `.proto` sketch + JSON schema. Needs `lief` |
| `emit_proto.py` | JSON schema → nested, protoc-valid `.proto` (pure Python) |
| `proto_header.txt` | provenance header prepended to the generated file |
| `run.sh` | runs a script with `lief` available via Nix |

## Usage

```sh
# 1. extract the official installer (xar → cpio)
mkdir -p /tmp/uupkg && cd /tmp/uupkg
xar -xf /path/to/uuyc_<version>.pkg
mkdir payload && cd payload && gzip -dc ../UURemote.pkg/Payload | cpio -id

# 2. reconstruct
cd <repo>/docs/tools
./run.sh protogen.py /tmp/uupkg/payload/Applications/UURemote.app/Contents/Helpers/UURemoteServer \
    /tmp/raw.proto /tmp/schema.json
python3 emit_proto.py /tmp/schema.json /tmp/gv_pb.proto proto_header.txt
protoc --proto_path=/tmp --descriptor_set_out=/dev/null /tmp/gv_pb.proto   # sanity check
diff ../proto/gv_pb.proto /tmp/gv_pb.proto
```

`protogen.py` prints `UNASSIGNED` maps (names it could not attach to a `GvPb_` type) and `LOW`
scores (weak matches) — inspect those after a version bump. The cut-off between UU's own name maps
and Google's descriptor/well-known types is the first map named `["paths"]` (`FieldMask`); adjust
`gv_maps` in the script if a future build reorders them. Re-running on 4.48.1 reproduces the
committed `gv_pb.proto` byte for byte.

Other useful one-liners for the same binaries:

```sh
strings -n 4 Helpers/UURemoteServer | grep -oE '_TtC14UURemoteServer[0-9]+[A-Za-z]+' | sort -u   # Swift classes
strings -n 4 Frameworks/libstreamer.dylib | grep -E '_DATA_CHANNEL|socket.io|X-NRD|soac|rtp-hdrext'
strings -n 3 MacOS/UURemote | grep -E '^/api/v[0-9]'                                             # REST paths
```

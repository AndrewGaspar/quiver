#!/usr/bin/env python3
"""Extract embedded protobuf FileDescriptorProtos from a binary and print them as .proto source.

    extract.py <binary> [name-substring]

Ubisoft's DLLs/EXEs embed the descriptors of the protocols they speak (serialized
FileDescriptorProto, starting with field 1 = file name). This finds each one by its
file name, decodes it with a minimal wire-format parser, and renders proto2/proto3 source.
"""
import re
import sys

LABEL = {1: "optional", 2: "required", 3: "repeated"}
TYPE = {1: "double", 2: "float", 3: "int64", 4: "uint64", 5: "int32", 6: "fixed64", 7: "fixed32",
        8: "bool", 9: "string", 10: "group", 11: None, 12: "bytes", 13: "uint32", 14: None,
        15: "sfixed32", 16: "sfixed64", 17: "sint32", 18: "sint64"}


def varint(b, i):
    r = s = 0
    while True:
        x = b[i]; i += 1
        r |= (x & 0x7F) << s; s += 7
        if x < 0x80:
            return r, i


def fields(b):
    """Yield (field_number, wire_type, value) for a complete message buffer."""
    i = 0
    while i < len(b):
        tag, i = varint(b, i)
        fn, wt = tag >> 3, tag & 7
        if wt == 0:
            v, i = varint(b, i)
        elif wt == 2:
            n, i = varint(b, i); v = b[i:i + n]; i += n
        elif wt == 1:
            v = b[i:i + 8]; i += 8
        elif wt == 5:
            v = b[i:i + 4]; i += 4
        else:
            raise ValueError(f"wire type {wt}")
        yield fn, wt, v


def message_end(b, start, max_field):
    """Length of the longest prefix of b[start:] that parses as fields numbered 1..max_field."""
    i = start
    last_good = start
    while i < len(b):
        try:
            tag, j = varint(b, i)
            fn, wt = tag >> 3, tag & 7
            if not (1 <= fn <= max_field) or wt not in (0, 2):
                break
            if wt == 0:
                _, j = varint(b, j)
            else:
                n, j = varint(b, j)
                j += n
                if j > len(b):
                    break
            i = last_good = j
        except IndexError:
            break
    return last_good


def render_enum(e, ind):
    name = next(v.decode() for f, _, v in fields(e) if f == 1)
    out = [f"{ind}enum {name} {{"]
    for f, _, v in fields(e):
        if f == 2:
            vals = {k: x for k, _, x in fields(v)}
            out.append(f"{ind}    {vals[1].decode()} = {vals.get(2, 0)};")
    out.append(f"{ind}}}")
    return out


def render_message(m, ind, syntax):
    name = next(v.decode() for f, _, v in fields(m) if f == 1)
    out = [f"{ind}message {name} {{"]
    for f, _, v in fields(m):
        if f == 2:
            fd = {}
            for k, _, x in fields(v):
                fd[k] = x
            label = LABEL.get(fd.get(4, 1), "optional")
            if syntax == "proto3" and label == "optional":
                label = ""
            t = TYPE.get(fd.get(5)) or fd.get(6, b"?").decode().lstrip(".")
            lbl = f"{label} " if label else ""
            out.append(f"{ind}    {lbl}{t} {fd[1].decode()} = {fd[3]};")
        elif f == 3:
            out += render_message(v, ind + "    ", syntax)
        elif f == 4:
            out += render_enum(v, ind + "    ")
    out.append(f"{ind}}}")
    return out


def render_file(fdp):
    syntax = "proto2"
    for f, _, v in fields(fdp):
        if f == 12:
            syntax = v.decode()
    out = [f'syntax = "{syntax}";', ""]
    for f, _, v in fields(fdp):
        if f == 1:
            out.insert(0, f"// extracted: {v.decode()}")
        elif f == 2:
            out += [f"package {v.decode()};", ""]
        elif f == 3:
            out.append(f'import "{v.decode()}";')
    for f, _, v in fields(fdp):
        if f == 4:
            out += render_message(v, "", syntax) + [""]
        elif f == 5:
            out += render_enum(v, "") + [""]
    return "\n".join(out)


def main():
    data = open(sys.argv[1], "rb").read()
    want = sys.argv[2] if len(sys.argv) > 2 else ""
    for m in re.finditer(rb"\x0a([\x05-\x7f])([A-Za-z0-9_/.]+\.proto)", data):
        if m.group(1)[0] != len(m.group(2)) or want.encode() not in m.group(2):
            continue
        start = m.start()
        end = message_end(data, start, 14)
        try:
            print(render_file(data[start:end]))
        except Exception as e:  # not a real descriptor
            print(f"// skipped {m.group(2).decode()} at {start:#x}: {e}", file=sys.stderr)


if __name__ == "__main__":
    main()

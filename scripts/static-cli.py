#!/usr/bin/env python3
import os
import shlex
import struct
import subprocess
import sys

CIE_ID = 0
DW_EH_PE_PCREL_SDATA4 = 0x1B
DW_EH_PE_DATAREL_SDATA4 = 0x3B
DW_EH_PE_UDATA4 = 0x03
INT3 = 0xCC
NOP = 0x90
SECTION = struct.Struct("<IIQQQQIIQQ")
STT_FUNC = 2
SYMBOL = struct.Struct("<IBBHQQ")
UNWIND_STARTS = ("__restore_rt", "_dl_tlsdesc_undefweak")
DYNAMIC_ONLY = ("-Wl,-Bdynamic", "-Wl,-Bstatic", "-lgcc_s", "-lgcc_eh", "-lutil", "-lrt", "-lpthread", "-lm", "-ldl")
GLIBC_SPLIT = ("-lpthread", "-ldl", "-lrt", "-lm", "-lutil")

link, layout, order, output, *reserve = sys.argv[1:]


def compiler_file(name):
    return subprocess.run(["cc", "-print-file-name=" + name], capture_output=True, text=True, check=True).stdout.strip()


args = shlex.split(open(link).read())
env = dict(os.environ)
while "=" in args[0] and not args[0].startswith("-"):
    key, value = args.pop(0).split("=", 1)
    env[key] = value
args[args.index("-o") + 1] = output
linked = []
for arg in args:
    if arg.startswith("-Wl,-Map=") or arg in DYNAMIC_ONLY:
        continue
    if arg == "-lc":
        linked += ["-Wl,--start-group", *GLIBC_SPLIT, "-lc", "-lgcc", "-lgcc_eh", "-Wl,--end-group"]
    elif arg == "-no-pie":
        linked += ["-no-pie", "-static"]
    else:
        linked.append(arg)
script = output + ".ld"
tail = "  *(.rodata .rodata.*)\n} } INSERT BEFORE .eh_frame;\n"
text = open(layout).read()
if not text.endswith(tail):
    sys.exit(f"static-cli.py: {layout} does not end with the .rodata catch-all")
with open(script, "w") as out:
    out.write(text[: -len(tail)] + "".join("  *%s:*(.rodata .rodata.*)\n" % archive for archive in ("libc.a", "libgcc_eh.a", "libgcc.a")) + tail)
start = [compiler_file(name) for name in ("crt1.o", "crti.o", "crtbegin.o")]
end = [compiler_file(name) for name in ("crtend.o", "crtn.o")]
if reserve:
    source = output + ".reserve.s"
    with open(source, "w") as out:
        out.write('.section .text.reserve,"axR",@progbits\n.fill %d,1,0x%x\n' % (int(reserve[0]), NOP))
    subprocess.run(["cc", "-c", "-o", output + ".reserve.o", source], check=True)
    end.insert(0, output + ".reserve.o")
subprocess.run([linked[0], "-nostartfiles", *start, *linked[1:], *end, "-Wl,-T," + script, "-Wl,--symbol-ordering-file=" + order, "-Wl,--no-warn-symbol-ordering"], env=env, check=True)

data = bytearray(open(output, "rb").read())
shoff, = struct.unpack_from("<Q", data, 0x28)
shentsize, shnum, shstrndx = struct.unpack_from("<HHH", data, 0x3A)
headers = [SECTION.unpack_from(data, shoff + i * shentsize) for i in range(shnum)]


def string(offset):
    return data[offset:data.index(0, offset)].decode()


sections = {string(headers[shstrndx][4] + header[0]): (index, header) for index, header in enumerate(headers)}
text_index, text_header = sections[".text"]
_, table = sections[".symtab"]
names = headers[table[6]][4]
functions = []
starts = {}
for k in range(table[5] // SYMBOL.size):
    name, info, _, shndx, value, size = SYMBOL.unpack_from(data, table[4] + k * SYMBOL.size)
    if info & 0xF == STT_FUNC:
        if string(names + name) in UNWIND_STARTS:
            starts[value] = string(names + name)
        if shndx == text_index and size:
            functions.append((value, value + size))


def uleb(offset):
    result = shift = 0
    while True:
        byte = data[offset]
        offset += 1
        result |= (byte & 0x7F) << shift
        shift += 7
        if byte < 0x80:
            return result, offset


_, frame = sections[".eh_frame"]
_, frame_header = sections[".eh_frame_hdr"]
encodings = {}
moved = {}
position = 0
while position < frame[5]:
    length, = struct.unpack_from("<I", data, frame[4] + position)
    if length == 0:
        break
    identifier, = struct.unpack_from("<I", data, frame[4] + position + 4)
    if identifier == CIE_ID:
        cursor = frame[4] + position + 9
        augmentation = string(cursor)
        cursor += len(augmentation) + 1
        _, cursor = uleb(cursor)
        _, cursor = uleb(cursor)
        cursor += 1
        encoding = None
        if augmentation.startswith("z"):
            _, cursor = uleb(cursor)
            for letter in augmentation[1:]:
                if letter == "R":
                    encoding = data[cursor]
                    cursor += 1
                elif letter == "P":
                    cursor += 1 + {0x00: 8, 0x03: 4, 0x1B: 4, 0x9B: 4}[data[cursor]]
                elif letter == "L":
                    cursor += 1
        encodings[position] = encoding
    elif encodings[position + 4 - identifier] == DW_EH_PE_PCREL_SDATA4:
        field = frame[4] + position + 8
        relative, extent = struct.unpack_from("<iI", data, field)
        begin = frame[3] + position + 8 + relative
        for symbol, name in starts.items():
            if begin < symbol < begin + 16 and symbol < begin + extent:
                struct.pack_into("<iI", data, field, relative + symbol - begin, extent - (symbol - begin))
                moved[begin] = (symbol, name)
    position += 4 + length
if sorted(name for _, name in moved.values()) != sorted(UNWIND_STARTS):
    sys.exit("static-cli.py: no FDE starts just before " + ", ".join(sorted(set(UNWIND_STARTS) - {name for _, name in moved.values()})))
table_offset = frame_header[4]
if data[table_offset + 2:table_offset + 4] != bytes((DW_EH_PE_UDATA4, DW_EH_PE_DATAREL_SDATA4)):
    sys.exit("static-cli.py: unexpected .eh_frame_hdr table encoding")
count, = struct.unpack_from("<I", data, table_offset + 8)
for i in range(count):
    location, = struct.unpack_from("<i", data, table_offset + 12 + 8 * i)
    if frame_header[3] + location in moved:
        struct.pack_into("<i", data, table_offset + 12 + 8 * i, moved[frame_header[3] + location][0] - frame_header[3])

low, offset = text_header[3], text_header[4]
functions.sort()
padded = 0
reach = functions[0][1]
for begin, finish in functions[1:]:
    cursor = begin
    while cursor > reach and data[offset + cursor - 1 - low] == INT3:
        cursor -= 1
    data[offset + cursor - low:offset + begin - low] = bytes((NOP,)) * (begin - cursor)
    padded += begin - cursor
    reach = max(reach, finish)
open(output, "wb").write(data)
print(f"static-cli.py: {len(moved)} FDEs moved, {padded} bytes of int3 padding set to nop", file=sys.stderr)

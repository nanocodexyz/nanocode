#!/usr/bin/env python3
import bisect
import os
import re
import shlex
import struct
import subprocess
import sys

EM_X86_64 = 62
X86_64_PC_RELATIVE = (2, 4, 9, 41, 42)
X86_64_DISPLACEMENT_BYTES = 4
SHT_RELA = 4
RELA = struct.Struct("<QQq")
SYMBOL = struct.Struct("<IBBHQQ")
SECTION = struct.Struct("<IIQQQQIIQQ")

binary, layout, link, output, *profiles = sys.argv[1:]
data = open(binary, "rb").read()
machine, = struct.unpack_from("<H", data, 0x12)
shoff, = struct.unpack_from("<Q", data, 0x28)
shentsize, shnum, shstrndx = struct.unpack_from("<HHH", data, 0x3A)
headers = [SECTION.unpack_from(data, shoff + i * shentsize) for i in range(shnum)]


def string(offset):
    return data[offset:data.index(b"\0", offset)].decode("utf8", "replace")


sections = {string(headers[shstrndx][4] + header[0]): header for header in headers}
rodata = sections[".rodata"]
low, high = rodata[3], rodata[3] + rodata[5]
table = sections[".symtab"]
names = headers[table[6]][4]
symbols = [SYMBOL.unpack_from(data, table[4] + k * SYMBOL.size) for k in range(table[5] // SYMBOL.size)]
functions = {}
for name, info, _, _, value, size in symbols:
    if info & 0xF == 2 and size:
        functions.setdefault(string(names + name), []).append((value, value + size))


def relocations(name):
    header = sections.get(name)
    if header is None or header[1] != SHT_RELA:
        return
    for k in range(header[5] // RELA.size):
        offset, info, addend = RELA.unpack_from(data, header[4] + k * RELA.size)
        target = symbols[info >> 32][4] + addend
        if machine == EM_X86_64 and info & 0xFFFFFFFF in X86_64_PC_RELATIVE:
            target += X86_64_DISPLACEMENT_BYTES
        if low <= target < high:
            yield offset, target


inputs = []
lines = open(layout).read().split("\n")
first = next(i for i, line in enumerate(lines) if re.match(r"^\s+%x\s+%x\s+%x\s+\d+\s\.rodata$" % (low, low, high - low), line))
for line in lines[first + 1:]:
    match = re.match(r"^\s+([0-9a-f]+)\s+[0-9a-f]+\s+([0-9a-f]+)\s+\d+\s{9}\S*:\((\S+)\)$", line)
    if match:
        inputs.append((int(match.group(1), 16), int(match.group(2), 16), match.group(3)))
    elif re.match(r"^\s+[0-9a-f]+\s+[0-9a-f]+\s+[0-9a-f]+\s+\d+\s\S", line):
        break
starts = [start for start, _, _ in inputs]


def containing(address):
    i = bisect.bisect_right(starts, address) - 1
    return i if i >= 0 and address < inputs[i][0] + inputs[i][1] else None


references = {}
for offset, target in relocations(".rela.rodata"):
    references.setdefault(containing(offset), set()).add(containing(target))
code = sorted(relocations(".rela.text"))
offsets = [offset for offset, _ in code]


def executed(directory):
    found = set()
    for entry in os.listdir(directory):
        for line in open(os.path.join(directory, entry)):
            fields = line.split()
            if len(fields) == 8 and int(fields[7]) > 0:
                found.update(re.sub(r"/\d+$", "", fields[i]) for i in (1, 4))
    return found


merged = (".rodata.cst", ".rodata.str")
order = []
placed = set()
for directory in profiles:
    pending = []
    for name in sorted(executed(directory)):
        for start, end in functions.get(name, []):
            pending.extend(containing(target) for _, target in code[bisect.bisect_left(offsets, start):bisect.bisect_left(offsets, end)])
    while pending:
        i = pending.pop()
        if i is None or i in placed:
            continue
        placed.add(i)
        if not inputs[i][2].startswith(merged):
            order.append(inputs[i][2])
        pending.extend(references.get(i, ()))
script = output + ".ld"
with open(script, "w") as out:
    out.write("SECTIONS { .rodata : {\n  *(%s)\n" % " ".join(prefix + "*" for prefix in merged))
    out.writelines("  *(%s)\n" % name for name in order)
    out.write("  *(.rodata .rodata.*)\n} } INSERT BEFORE .eh_frame;\n")
args = shlex.split(open(link).read())
env = dict(os.environ)
while "=" in args[0] and not args[0].startswith("-"):
    key, value = args.pop(0).split("=", 1)
    env[key] = value
args[args.index("-o") + 1] = output
args = [arg for arg in args if not arg.startswith("-Wl,-Map=")]
subprocess.run([*args, "-Wl,-T," + script], env=env, check=True)

import gzip, lzma, os, re, struct, subprocess, sys
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import BIN, ROOT, emit

TOP = int(os.environ.get("NANOCODE_UPHILL_TOP_CRATES", "15"))
SECTIONS = (".text", ".rodata", ".data.rel.ro", ".eh_frame", ".eh_frame_hdr", ".gcc_except_table", ".data", ".bss")
V0 = re.compile(r"C(?:s[0-9A-Za-z]*_)?(\d+)")


def elf(path):
    data = open(path, "rb").read()
    if data[:4] != b"\x7fELF" or data[4] != 2:
        raise SystemExit(f"{path} is not a 64-bit ELF")
    shoff, = struct.unpack_from("<Q", data, 0x28)
    shentsize, shnum, shstrndx = struct.unpack_from("<HHH", data, 0x3A)
    headers = [struct.unpack_from("<IIQQQQIIQQ", data, shoff + i * shentsize) for i in range(shnum)]
    names = headers[shstrndx]
    def name(offset):
        start = names[4] + offset
        return data[start:data.index(b"\0", start)].decode()
    return data, [(name(h[0]),) + h for h in headers]


def crate(symbol):
    if symbol.startswith("_ZN"):
        match = re.match(r"_ZN(\d+)", symbol)
        if match:
            length = int(match.group(1))
            start = match.end()
            return symbol[start:start + length].lstrip("_$")
    if symbol.startswith("_R"):
        match = V0.search(symbol, 2)
        if match:
            length = int(match.group(1))
            start = match.end()
            text = symbol[start:start + length]
            return text[1:] if text.startswith("_") else text
    return "[other]"


def crates(path):
    data, sections = elf(path)
    symtab = next((s for s in sections if s[0] == ".symtab"), None)
    if symtab is None:
        raise SystemExit(f"{path} has no symbols")
    strtab = sections[symtab[7]]
    sizes = {}
    for offset in range(symtab[5], symtab[5] + symtab[6], 24):
        name, info, _, _, _, size = struct.unpack_from("<IBBHQQ", data, offset)
        if info & 0xF not in (1, 2) or not size:
            continue
        start = strtab[5] + name
        symbol = data[start:data.index(b"\0", start)].decode(errors="replace")
        key = crate(symbol)
        sizes[key] = sizes.get(key, 0) + size
    return sizes


def dependencies():
    tree = subprocess.run(["cargo", "tree", "--locked", "-p", "nanocode-cli", "-e", "normal,build", "--prefix", "none"], check=True, capture_output=True, text=True).stdout
    packages = {line.split(" (")[0].replace(" (*)", "").strip() for line in tree.splitlines() if line.strip()}
    names = {}
    for package in packages:
        name = package.split(" v")[0]
        names[name] = names.get(name, 0) + 1
    duplicated = sum(1 for count in names.values() if count > 1)
    return len(packages), duplicated


def main():
    subprocess.run(["cargo", "build", "--locked", "--release", "-p", "nanocode-cli", "--bin", "nanocode"], check=True, stdout=sys.stderr)
    binary = os.path.join(BIN, "nanocode")
    data, sections = elf(binary)
    lines = [f"section-size {name} {size}B" for name, *rest in sections for size in [rest[5]] if name in SECTIONS]
    lines.append(f"download-size gzip {len(gzip.compress(data, 9))}B")
    lines.append(f"download-size xz {len(lzma.compress(data, preset=6))}B")
    symbols = os.path.join(ROOT, "target", "uphill-symbols")
    env = dict(os.environ, CARGO_TARGET_DIR=symbols, CARGO_PROFILE_RELEASE_STRIP="none")
    subprocess.run(["cargo", "build", "--locked", "--release", "-p", "nanocode-cli", "--bin", "nanocode"], check=True, stdout=sys.stderr, env=env)
    sizes = crates(os.path.join(symbols, "release", "nanocode"))
    for name, size in sorted(sizes.items(), key=lambda item: -item[1])[:TOP]:
        lines.append(f"crate-size {name} {size}B")
    packages, duplicated = dependencies()
    lines += [f"dependencies {packages}", f"duplicate-dependencies {duplicated}"]
    emit(lines)


main()

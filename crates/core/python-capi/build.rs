use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    thread,
};

fn sorted(dir: &Path, keep: impl Fn(&Path) -> bool) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap_or_else(|error| panic!("{}: {error}", dir.display()))
        .map(|entry| entry.expect("a directory entry").path())
        .filter(|path| keep(path))
        .collect();
    paths.sort();
    paths
}

fn listed(dir: &Path, file: &str) -> impl Iterator<Item = String> {
    fs::read_to_string(dir.join(file))
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>()
        .into_iter()
}

fn archive(objects: &[PathBuf]) -> Vec<u8> {
    let mut out = b"!<arch>\n".to_vec();
    for (index, object) in objects.iter().enumerate() {
        let bytes = fs::read(object).expect("a compiled object");
        let header = format!(
            "{:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n",
            format!("{index}.o/"),
            0,
            0,
            0,
            644,
            bytes.len()
        );
        out.extend_from_slice(header.as_bytes());
        out.extend_from_slice(&bytes);
        if bytes.len() % 2 == 1 {
            out.push(b'\n');
        }
    }
    out
}

fn leb(bytes: &[u8], at: &mut usize) -> u64 {
    let mut result = 0;
    let mut shift = 0;
    loop {
        let byte = bytes[*at];
        *at += 1;
        result |= u64::from(byte & 0x7f) << shift;
        shift += 7;
        if byte & 0x80 == 0 {
            return result;
        }
    }
}

fn index(bytes: &[u8], at: &mut usize) -> usize {
    usize::try_from(leb(bytes, at)).expect("an index that fits")
}

fn string(bytes: &[u8], at: &mut usize) -> String {
    let length = index(bytes, at);
    let text = String::from_utf8_lossy(&bytes[*at..*at + length]).into_owned();
    *at += length;
    text
}

fn limits(bytes: &[u8], at: &mut usize) {
    let flags = bytes[*at];
    *at += 1;
    leb(bytes, at);
    if flags & 1 != 0 {
        leb(bytes, at);
    }
}

const SYMBOL_TABLE: u8 = 8;
const UNDEFINED: u64 = 0x10;
const EXPLICIT_NAME: u64 = 0x40;
const TABLE_INDEX: [u8; 6] = [1, 2, 12, 18, 19, 24];
const ADDEND: [u8; 14] = [3, 4, 5, 8, 9, 11, 14, 15, 16, 17, 21, 22, 23, 25];

fn identifier(name: &str) -> bool {
    name.chars()
        .next()
        .is_some_and(|first| !first.is_ascii_digit())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn address_taken(object: &Path) -> Vec<(String, usize)> {
    let bytes = fs::read(object).expect("a compiled object");
    let mut at = 8;
    let mut types = Vec::new();
    let mut imported = 0;
    let mut functions = Vec::new();
    let mut symbols: Vec<(u8, u64, usize, String)> = Vec::new();
    let mut taken = std::collections::BTreeSet::new();
    while at < bytes.len() {
        let id = bytes[at];
        at += 1;
        let size = index(&bytes, &mut at);
        let end = at + size;
        let mut p = at;
        match id {
            1 => {
                for _ in 0..index(&bytes, &mut p) {
                    p += 1;
                    let params = index(&bytes, &mut p);
                    p += params;
                    let results = index(&bytes, &mut p);
                    p += results;
                    types.push(params);
                }
            }
            2 => {
                for _ in 0..index(&bytes, &mut p) {
                    string(&bytes, &mut p);
                    string(&bytes, &mut p);
                    let kind = bytes[p];
                    p += 1;
                    match kind {
                        0 => {
                            leb(&bytes, &mut p);
                            imported += 1;
                        }
                        1 => {
                            p += 1;
                            limits(&bytes, &mut p);
                        }
                        2 => limits(&bytes, &mut p),
                        3 => p += 2,
                        _ => {
                            p += 1;
                            leb(&bytes, &mut p);
                        }
                    }
                }
            }
            3 => {
                for _ in 0..index(&bytes, &mut p) {
                    functions.push(index(&bytes, &mut p));
                }
            }
            0 => {
                let section = string(&bytes, &mut p);
                if section == "linking" {
                    leb(&bytes, &mut p);
                    while p < end {
                        let kind = bytes[p];
                        p += 1;
                        let length = index(&bytes, &mut p);
                        let next = p + length;
                        if kind == SYMBOL_TABLE {
                            for _ in 0..index(&bytes, &mut p) {
                                let kind = bytes[p];
                                p += 1;
                                let flags = leb(&bytes, &mut p);
                                match kind {
                                    1 => {
                                        let name = string(&bytes, &mut p);
                                        if flags & UNDEFINED == 0 {
                                            leb(&bytes, &mut p);
                                            leb(&bytes, &mut p);
                                            leb(&bytes, &mut p);
                                        }
                                        symbols.push((kind, flags, 0, name));
                                    }
                                    3 => {
                                        let target = index(&bytes, &mut p);
                                        symbols.push((kind, flags, target, String::new()));
                                    }
                                    _ => {
                                        let target = index(&bytes, &mut p);
                                        let name = if flags & UNDEFINED == 0
                                            || flags & EXPLICIT_NAME != 0
                                        {
                                            string(&bytes, &mut p)
                                        } else {
                                            String::new()
                                        };
                                        symbols.push((kind, flags, target, name));
                                    }
                                }
                            }
                        }
                        p = next;
                    }
                } else if section.starts_with("reloc.") {
                    leb(&bytes, &mut p);
                    for _ in 0..index(&bytes, &mut p) {
                        let kind = bytes[p];
                        p += 1;
                        leb(&bytes, &mut p);
                        let symbol = index(&bytes, &mut p);
                        if ADDEND.contains(&kind) {
                            leb(&bytes, &mut p);
                        }
                        if TABLE_INDEX.contains(&kind) {
                            taken.insert(symbol);
                        }
                    }
                }
            }
            _ => {}
        }
        at = end;
    }
    taken
        .into_iter()
        .filter_map(|symbol| {
            let (kind, flags, function, name) = symbols.get(symbol)?;
            (*kind == 0 && flags & UNDEFINED == 0 && *function >= imported && identifier(name))
                .then(|| (name.clone(), types[functions[function - imported]]))
        })
        .collect()
}

fn apple(target: &str) -> Option<(String, PathBuf)> {
    let (arch, system) = target.split_once("-apple-")?;
    let (os, sdk, suffix, variable) = match system {
        "darwin" => ("macosx", "macosx", "", "MACOSX_DEPLOYMENT_TARGET"),
        "ios-sim" => (
            "ios",
            "iphonesimulator",
            "-simulator",
            "IPHONEOS_DEPLOYMENT_TARGET",
        ),
        "ios" if arch == "x86_64" => (
            "ios",
            "iphonesimulator",
            "-simulator",
            "IPHONEOS_DEPLOYMENT_TARGET",
        ),
        "ios" => ("ios", "iphoneos", "", "IPHONEOS_DEPLOYMENT_TARGET"),
        _ => return None,
    };
    let output = |command: &mut Command| {
        let output = command
            .output()
            .unwrap_or_else(|error| panic!("{command:?}: {error}"));
        assert!(output.status.success(), "{command:?} failed");
        String::from_utf8(output.stdout)
            .expect("UTF-8 output")
            .trim()
            .to_owned()
    };
    println!("cargo:rerun-if-env-changed={variable}");
    let printed = output(
        Command::new(env::var_os("RUSTC").expect("RUSTC"))
            .args(["--print", "deployment-target", "--target"])
            .arg(target),
    );
    let version = printed
        .split_once('=')
        .map_or(printed.as_str(), |(_, version)| version);
    let sysroot = output(Command::new("xcrun").args(["--sdk", sdk, "--show-sdk-path"]));
    Some((
        format!("{arch}-apple-{os}{version}{suffix}"),
        PathBuf::from(sysroot),
    ))
}

fn is_cxx(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension == "cpp" || extension == "cc" || extension == "cxx")
}

fn main() {
    let root = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    let target = env::var("TARGET").expect("TARGET");
    let host = env::var("HOST").expect("HOST");
    let families = env::var("CARGO_CFG_TARGET_FAMILY").unwrap_or_default();
    let families: Vec<&str> = families.split(',').collect();
    let wasm = families.contains(&"wasm");
    let windows = env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "windows");
    let variable = format!("CC_{}", target.replace('-', "_"));
    println!("cargo:rerun-if-env-changed=NANOCODE_CC");
    println!("cargo:rerun-if-env-changed={variable}");
    println!("cargo:rerun-if-changed=include");
    println!("cargo:rerun-if-changed=ext");
    println!("cargo:rerun-if-changed=lib");
    let compiler = env::var_os("NANOCODE_CC")
        .or_else(|| env::var_os(&variable))
        .unwrap_or_else(|| {
            if wasm || windows || target != host {
                "clang".into()
            } else {
                "cc".into()
            }
        });
    let include = root.join("include");
    let mut objects = Vec::new();
    let mut table = String::new();
    let mut entries = String::new();
    let mut arities = String::new();
    let mut tables = String::new();
    println!("cargo:rerun-if-changed=c");
    let jobs: usize = env::var("NUM_JOBS")
        .ok()
        .and_then(|jobs| jobs.parse().ok())
        .unwrap_or(1)
        .max(1);
    let runtime = root.join("cxx");
    println!("cargo:rerun-if-changed=cxx");
    let apple = apple(&target);
    let compile = |source: &Path, object: &Path, defines: &[String]| {
        let mut command = Command::new(&compiler);
        if let Some((triple, sysroot)) = &apple {
            command
                .arg(format!("--target={triple}"))
                .arg("-isysroot")
                .arg(sysroot);
        } else if target != host {
            command.arg(format!(
                "--target={}",
                target.replace("riscv64gc-", "riscv64-")
            ));
        }
        let cxx = is_cxx(source);
        if wasm {
            command.args(["-ffreestanding", "-nostdlibinc"]);
            if cxx {
                command
                    .args(["-std=c++17", "-fwasm-exceptions", "-nostdinc++", "-isystem"])
                    .arg(include.join("c++").join("v1"))
                    .arg("-isystem")
                    .arg(include.join("libc"));
            } else {
                command
                    .args(["-fwasm-exceptions", "-mllvm", "-wasm-enable-sjlj"])
                    .arg("-I")
                    .arg(include.join("libc"));
            }
        } else {
            if cxx {
                command.arg("-std=c++17");
            }
            if !windows {
                command.arg("-fPIC");
            }
        }
        command
            .args([
                "-O2",
                "-DNDEBUG",
                "-DCYTHON_LIMITED_API=1",
                "-fvisibility=hidden",
            ])
            .args(defines)
            .arg("-I")
            .arg(&include)
            .arg("-I")
            .arg(include.join("cpython"))
            .arg("-c")
            .arg(source)
            .arg("-o")
            .arg(object);
        let status = command
            .status()
            .unwrap_or_else(|error| panic!("{}: {error}", compiler.to_string_lossy()));
        assert!(status.success(), "compiling {} failed", source.display());
    };
    let mut cxx_runtime = false;
    for source in sorted(&root.join("c"), |path| {
        path.extension().is_some_and(|extension| extension == "c")
    }) {
        let object = out.join(format!("{}.o", objects.len()));
        compile(&source, &object, &[]);
        objects.push(object);
    }
    for dir in sorted(&root.join("lib"), |path| path.is_dir()) {
        let only: Vec<String> = listed(&dir, "families").collect();
        if !only.is_empty()
            && !only
                .iter()
                .any(|family| families.contains(&family.as_str()))
        {
            continue;
        }
        let mut flags = vec![format!("-I{}", dir.display())];
        if wasm {
            flags.extend(["-mllvm".to_owned(), "-wasm-enable-sjlj".to_owned()]);
        }
        flags
            .extend(listed(&dir, "includes").map(|line| format!("-I{}", dir.join(line).display())));
        flags.extend(listed(&dir, "defines").map(|line| format!("-D{line}")));
        flags.extend(listed(&dir, "flags"));
        let sources: Vec<PathBuf> = if dir.join("sources").exists() {
            listed(&dir, "sources").map(|line| dir.join(line)).collect()
        } else {
            sorted(&dir, |path| {
                path.extension().is_some_and(|extension| extension == "c")
            })
        };
        let first = objects.len();
        let mut built: Vec<(usize, PathBuf)> = thread::scope(|scope| {
            let workers: Vec<_> = (0..jobs.min(sources.len()))
                .map(|worker| {
                    let (sources, flags, compile, out) = (&sources, &flags, &compile, &out);
                    scope.spawn(move || {
                        sources
                            .iter()
                            .enumerate()
                            .skip(worker)
                            .step_by(jobs)
                            .map(|(offset, source)| {
                                let object = out.join(format!("{}.o", first + offset));
                                compile(source, &object, flags);
                                (first + offset, object)
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            workers
                .into_iter()
                .flat_map(|worker| worker.join().expect("a compile worker"))
                .collect()
        });
        built.sort_by_key(|(index, _)| *index);
        objects.extend(built.into_iter().map(|(_, object)| object));
    }
    for dir in sorted(&root.join("ext"), |path| path.is_dir()) {
        let name = dir
            .file_name()
            .and_then(|name| name.to_str())
            .expect("a module directory named by its dotted name")
            .to_owned();
        let symbol = format!("PyInit_{}", name.replace('.', "_"));
        let last = name.rsplit('.').next().unwrap_or(&name);
        let mut defines = vec![
            format!("-DPyInit_{last}={symbol}"),
            format!("-I{}", dir.display()),
        ];
        defines.extend(
            sorted(&dir, |path| path.is_dir())
                .iter()
                .map(|include| format!("-I{}", include.display())),
        );
        if let Ok(includes) = fs::read_to_string(dir.join("includes")) {
            defines.extend(
                includes
                    .lines()
                    .filter(|line| !line.is_empty())
                    .map(|line| format!("-I{}", dir.join(line).display())),
            );
        }
        if let Ok(macros) = fs::read_to_string(dir.join("defines")) {
            defines.extend(
                macros
                    .lines()
                    .filter(|line| !line.is_empty())
                    .map(|line| format!("-D{line}")),
            );
        }
        if let Ok(flags) = fs::read_to_string(dir.join("flags")) {
            defines.extend(
                flags
                    .lines()
                    .filter(|line| !line.is_empty())
                    .map(str::to_owned),
            );
        }
        let sources = match fs::read_to_string(dir.join("sources")) {
            Ok(list) => list
                .lines()
                .filter(|line| !line.is_empty())
                .map(|line| dir.join(line))
                .collect(),
            Err(_) => sorted(&dir, |path| {
                path.extension().is_some_and(|extension| extension == "c") || is_cxx(path)
            }),
        };
        cxx_runtime |= sources.iter().any(|source| is_cxx(source));
        let first = objects.len();
        let mut built: Vec<(usize, PathBuf, bool)> = thread::scope(|scope| {
            let workers: Vec<_> = (0..jobs.min(sources.len()))
                .map(|worker| {
                    let (sources, defines, compile, out) = (&sources, &defines, &compile, &out);
                    scope.spawn(move || {
                        let mut done = Vec::new();
                        for (offset, source) in
                            sources.iter().enumerate().skip(worker).step_by(jobs)
                        {
                            let index = first + offset;
                            let object = out.join(format!("{index}.o"));
                            compile(source, &object, defines);
                            let mut table = false;
                            if wasm {
                                let cxx = is_cxx(source);
                                let functions = address_taken(&object);
                                let mut wrapper =
                                    format!("#include {:?}\n", source.display().to_string());
                                if cxx {
                                    wrapper.push_str(&format!(
                                        "__asm__(\".section .rodata.nanocode_arity_{index},\\\"\\\",@\\n.globl nanocode_arity_{index}\\n.p2align 2\\nnanocode_arity_{index}:\\n\"\n"
                                    ));
                                    for (function, params) in &functions {
                                        wrapper.push_str(&format!(
                                            "\".int32 \\\"{function}\\\"\\n.int32 {params}\\n\"\n"
                                        ));
                                    }
                                    wrapper.push_str(&format!(
                                        "\".int32 0\\n.int32 0\\n.size nanocode_arity_{index}, {}\\n.text\\n\");\n",
                                        8 * (functions.len() + 1)
                                    ));
                                } else {
                                    wrapper.push_str(&format!(
                                        "__attribute__((used)) const void *const nanocode_arity_{index}[] = {{\n"
                                    ));
                                    for (function, params) in &functions {
                                        wrapper.push_str(&format!(
                                            "    (const void *){function}, (const void *){params},\n"
                                        ));
                                    }
                                    wrapper.push_str("    0, 0\n};\n");
                                }
                                let extension = source
                                    .extension()
                                    .and_then(|extension| extension.to_str())
                                    .unwrap_or("c");
                                if !functions.is_empty() {
                                    let path = out.join(format!("{index}.{extension}"));
                                    fs::write(&path, wrapper).expect("the arity wrapper");
                                    compile(&path, &object, defines);
                                    table = true;
                                }
                            }
                            done.push((index, object, table));
                        }
                        done
                    })
                })
                .collect();
            workers
                .into_iter()
                .flat_map(|worker| worker.join().expect("a compile worker"))
                .collect()
        });
        built.sort_by_key(|(index, _, _)| *index);
        for (index, object, table) in built {
            if table {
                arities.push_str(&format!("    static nanocode_arity_{index}: [usize; 0];\n"));
                tables.push_str(&format!(
                    "        &raw const nanocode_arity_{index} as *const usize,\n"
                ));
            }
            objects.push(object);
        }
        table.push_str(&format!("    fn {symbol}() -> *mut Object;\n"));
        entries.push_str(&format!(
            "    Extension {{ name: {name:?}, init: {symbol} }},\n"
        ));
    }
    if cxx_runtime && wasm {
        let list =
            fs::read_to_string(runtime.join("sources")).expect("the C++ runtime source list");
        let sources: Vec<PathBuf> = list
            .lines()
            .filter(|line| !line.is_empty())
            .map(|line| runtime.join(line))
            .chain([runtime.join("runtime.c")])
            .collect();
        let flags = [
            "-D_LIBUNWIND_HIDE_SYMBOLS".to_owned(),
            "-D_LIBCPP_BUILDING_LIBRARY".to_owned(),
            "-D_LIBCPP_HAS_NO_PRAGMA_SYSTEM_HEADER".to_owned(),
            "-DLIBCXX_BUILDING_LIBCXXABI".to_owned(),
            "-D_LIBCXXABI_BUILDING_LIBRARY".to_owned(),
            "-D_LIBCXXABI_HAS_NO_THREADS".to_owned(),
            "-std=c++23".to_owned(),
            format!("-I{}", runtime.join("libcxx").join("src").display()),
            format!("-I{}", runtime.join("libcxxabi").join("include").display()),
            format!("-I{}", runtime.join("libunwind").join("include").display()),
        ];
        let first = objects.len();
        let mut built: Vec<(usize, PathBuf)> = thread::scope(|scope| {
            let workers: Vec<_> = (0..jobs.min(sources.len()))
                .map(|worker| {
                    let (sources, flags, compile, out) = (&sources, &flags, &compile, &out);
                    scope.spawn(move || {
                        let mut done = Vec::new();
                        for (offset, source) in
                            sources.iter().enumerate().skip(worker).step_by(jobs)
                        {
                            let index = first + offset;
                            let object = out.join(format!("{index}.o"));
                            let unwind = [flags[0].clone(), flags[flags.len() - 1].clone()];
                            let flags = if is_cxx(source) {
                                &flags[..]
                            } else {
                                &unwind[..]
                            };
                            compile(source, &object, flags);
                            done.push((index, object));
                        }
                        done
                    })
                })
                .collect();
            workers
                .into_iter()
                .flat_map(|worker| worker.join().expect("a compile worker"))
                .collect()
        });
        built.sort_by_key(|(index, _)| *index);
        objects.extend(built.into_iter().map(|(_, object)| object));
    }
    fs::write(out.join("libnanocode_python_capi_ext.a"), archive(&objects)).expect("the archive");
    fs::write(
        out.join("extensions.rs"),
        format!("unsafe extern \"C\" {{\n{table}{arities}}}\n\npub static EXTENSIONS: &[Extension] = &[\n{entries}];\n\npub fn arity_tables() -> Vec<*const usize> {{\n    vec![\n{tables}    ]\n}}\n"),
    )
    .expect("the extension table");
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=nanocode_python_capi_ext");
    if cxx_runtime && !wasm {
        if env::var("CARGO_CFG_TARGET_VENDOR").is_ok_and(|vendor| vendor == "apple") {
            println!("cargo:rustc-link-lib=c++");
        } else if !windows {
            println!("cargo:rustc-link-lib=stdc++");
        }
    }
    if env::var("CARGO_CFG_TARGET_ENV").is_ok_and(|target_env| target_env == "msvc") {
        println!("cargo:rustc-link-lib=oldnames");
        println!("cargo:rustc-link-lib=gdi32");
        println!("cargo:rustc-link-lib=user32");
    }
}

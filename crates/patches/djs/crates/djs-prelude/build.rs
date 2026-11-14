//! Makes the base snapshot (ARCH "Images and dumps", D1321): a VM with the prelude installed,
//! collected, as a heap image in `$OUT_DIR/base.img`. The GC is deterministic (D1090), so
//! the same djs-core gives the same bytes; the image is verified here once, so loading it
//! can skip verification.

use djs_core::vm::{Vm, VmOptions};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let mut vm = Vm::new(VmOptions::default()).expect("out of memory making the base");
    // Without growth slack (0043), canonical (0101), like cells together (D2251).
    vm.prepare_base_image()
        .expect("out of memory making the base image");
    let image = vm.bytes().to_vec();
    // Full verification (cell structure, references, build hash) at build time.
    if let Err(e) = Vm::from_bytes(image.clone(), VmOptions::default()) {
        panic!("the base image does not load: {e}");
    }
    let dir = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    std::fs::write(dir.join("base.img"), &image).expect("cannot write the base image");
    let base_end = djs_core::heap::Heap::image_base_end(&image);
    std::fs::write(
        dir.join("base_end.rs"),
        format!("/// The base image's builtin prefix (`Roots::BASE_END`).\npub const BASE_END: u32 = {base_end};\n"),
    )
    .expect("cannot write the base's prefix length");
    // The build hash of the djs-core that made the image, which lib.rs compares with the
    // library's: the two are separate builds whose features cargo resolves apart (D2411).
    std::fs::write(
        dir.join("built_by.rs"),
        format!(
            "const BUILT_BY: u32 = {:#010x};\n",
            djs_core::vm::BUILD_HASH
        ),
    )
    .expect("cannot write the build hash");
}

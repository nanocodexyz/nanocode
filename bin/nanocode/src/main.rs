#![cfg(not(target_family = "wasm"))]
#![cfg_attr(all(target_os = "linux", target_env = "gnu"), no_main)]

#[global_allocator]
static ALLOCATOR: nanocode_sys::alloc::Heap = nanocode_sys::alloc::Heap;

nanocode_sys::main!(nanocode_cli::run);

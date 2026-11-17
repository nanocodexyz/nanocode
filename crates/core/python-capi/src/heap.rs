use std::{
    alloc::{Layout, alloc, alloc_zeroed, dealloc, realloc},
    cell::{Cell, RefCell},
    collections::HashMap,
    ffi::c_void,
    mem::size_of,
    ptr::null_mut,
};

use rustpython_vm::PyObject;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Plain,
    Proxy,
    Pending,
    Instance,
}

#[repr(C)]
pub struct Header {
    size: usize,
    pub kind: Cell<Kind>,
    pub queued: Cell<bool>,
    pub parked: Cell<bool>,
    pub link: Cell<*const PyObject>,
}

pub const ALIGN: usize = 16;
const HEADER: usize = size_of::<Header>().next_multiple_of(ALIGN);

fn layout(size: usize) -> Option<Layout> {
    Layout::from_size_align(HEADER.checked_add(size.max(1))?, ALIGN).ok()
}

pub fn allocate(size: usize, zeroed: bool) -> *mut c_void {
    let Some(layout) = layout(size) else {
        return null_mut();
    };
    let base = unsafe {
        if zeroed {
            alloc_zeroed(layout)
        } else {
            alloc(layout)
        }
    };
    if base.is_null() {
        return null_mut();
    }
    unsafe {
        base.cast::<Header>().write(Header {
            size,
            kind: Cell::new(Kind::Plain),
            queued: Cell::new(false),
            parked: Cell::new(false),
            link: Cell::new(std::ptr::null()),
        });
        base.add(HEADER).cast()
    }
}

#[cfg(target_family = "wasm")]
fn image() -> (usize, usize) {
    unsafe extern "C" {
        static __heap_base: u8;
    }
    (0, &raw const __heap_base as usize)
}

#[cfg(windows)]
fn image() -> (usize, usize) {
    unsafe extern "C" {
        static __ImageBase: u8;
    }
    let base = &raw const __ImageBase as usize;
    let size = unsafe {
        let pe = std::ptr::read_unaligned((base + 0x3c) as *const u32) as usize;
        std::ptr::read_unaligned((base + pe + 24 + 56) as *const u32) as usize
    };
    (base, base + size)
}

#[cfg(all(not(target_family = "wasm"), not(target_vendor = "apple"), not(windows)))]
fn image() -> (usize, usize) {
    unsafe extern "C" {
        static __executable_start: u8;
        static _end: u8;
    }
    (
        &raw const __executable_start as usize,
        &raw const _end as usize,
    )
}

#[cfg(target_vendor = "apple")]
fn image() -> (usize, usize) {
    use std::sync::OnceLock;

    const LC_SEGMENT_64: u32 = 0x19;

    #[repr(C)]
    struct Info {
        file: *const u8,
        base: *const MachHeader,
        symbol: *const u8,
        address: *const u8,
    }
    #[repr(C)]
    struct MachHeader {
        magic: u32,
        cpu: i32,
        subtype: i32,
        file: u32,
        commands: u32,
        size: u32,
        flags: u32,
        reserved: u32,
    }
    #[repr(C)]
    struct Segment {
        command: u32,
        size: u32,
        name: [u8; 16],
        address: u64,
        length: u64,
    }
    unsafe extern "C" {
        fn dladdr(address: *const c_void, info: *mut Info) -> i32;
    }
    static RANGE: OnceLock<(usize, usize)> = OnceLock::new();
    *RANGE.get_or_init(|| {
        let mut info = Info {
            file: std::ptr::null(),
            base: std::ptr::null(),
            symbol: std::ptr::null(),
            address: std::ptr::null(),
        };
        if unsafe { dladdr(image as *const c_void, &raw mut info) } == 0 || info.base.is_null() {
            return (0, 0);
        }
        let header = unsafe { info.base.read_unaligned() };
        let mut command = unsafe { info.base.add(1) }.cast::<u8>();
        let mut text = None;
        let (mut low, mut high) = (usize::MAX, 0);
        for _ in 0..header.commands {
            let segment = unsafe { command.cast::<Segment>().read_unaligned() };
            if segment.command == LC_SEGMENT_64 {
                let name = segment
                    .name
                    .split(|byte| *byte == 0)
                    .next()
                    .unwrap_or_default();
                let (address, length) = (segment.address as usize, segment.length as usize);
                if name == b"__TEXT" {
                    text = Some(address);
                }
                if name != b"__PAGEZERO" && name != b"__LINKEDIT" {
                    low = low.min(address);
                    high = high.max(address + length);
                }
            }
            command = unsafe { command.add(segment.size as usize) };
        }
        let Some(text) = text.filter(|_| low < high) else {
            return (0, 0);
        };
        let slide = (info.base as usize).wrapping_sub(text);
        (low.wrapping_add(slide), high.wrapping_add(slide))
    })
}

pub fn is_static<T>(pointer: *mut T) -> bool {
    let (start, end) = image();
    (start..end).contains(&(pointer as usize))
}

thread_local! {
    static STATIC_HEADERS: RefCell<HashMap<usize, &'static Header>> = RefCell::new(HashMap::new());
}

pub unsafe fn header<'a, T>(pointer: *mut T) -> &'a Header {
    if is_static(pointer) {
        return STATIC_HEADERS.with_borrow_mut(|headers| {
            *headers.entry(pointer as usize).or_insert_with(|| {
                Box::leak(Box::new(Header {
                    size: 0,
                    kind: Cell::new(Kind::Plain),
                    queued: Cell::new(false),
                    parked: Cell::new(false),
                    link: Cell::new(std::ptr::null()),
                }))
            })
        });
    }
    unsafe { &*pointer.cast::<u8>().sub(HEADER).cast::<Header>() }
}

pub unsafe fn release(pointer: *mut c_void) {
    if pointer.is_null() {
        return;
    }
    let base = unsafe { pointer.cast::<u8>().sub(HEADER) };
    let size = unsafe { (*base.cast::<Header>()).size };
    if let Some(layout) = layout(size) {
        unsafe { dealloc(base, layout) };
    }
}

pub unsafe fn resize(pointer: *mut c_void, size: usize) -> *mut c_void {
    if pointer.is_null() {
        return allocate(size, false);
    }
    let base = unsafe { pointer.cast::<u8>().sub(HEADER) };
    let old = unsafe { (*base.cast::<Header>()).size };
    let (Some(from), Some(to)) = (layout(old), layout(size)) else {
        return null_mut();
    };
    let moved = unsafe { realloc(base, from, to.size()) };
    if moved.is_null() {
        return null_mut();
    }
    unsafe {
        (*moved.cast::<Header>()).size = size;
        moved.add(HEADER).cast()
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn PyMem_Malloc(size: usize) -> *mut c_void {
    allocate(size, false)
}

#[unsafe(no_mangle)]
pub extern "C" fn PyMem_Calloc(count: usize, size: usize) -> *mut c_void {
    count
        .checked_mul(size)
        .map_or(null_mut(), |size| allocate(size, true))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn PyMem_Realloc(pointer: *mut c_void, size: usize) -> *mut c_void {
    unsafe { resize(pointer, size) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn PyMem_Free(pointer: *mut c_void) {
    unsafe { release(pointer) }
}

#[unsafe(no_mangle)]
pub extern "C" fn PyMem_RawMalloc(size: usize) -> *mut c_void {
    allocate(size, false)
}

#[unsafe(no_mangle)]
pub extern "C" fn PyMem_RawCalloc(count: usize, size: usize) -> *mut c_void {
    PyMem_Calloc(count, size)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn PyMem_RawRealloc(pointer: *mut c_void, size: usize) -> *mut c_void {
    unsafe { resize(pointer, size) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn PyMem_RawFree(pointer: *mut c_void) {
    unsafe { release(pointer) }
}

#[unsafe(no_mangle)]
pub extern "C" fn PyObject_Malloc(size: usize) -> *mut c_void {
    allocate(size, false)
}

#[unsafe(no_mangle)]
pub extern "C" fn PyObject_Calloc(count: usize, size: usize) -> *mut c_void {
    PyMem_Calloc(count, size)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn PyObject_Realloc(pointer: *mut c_void, size: usize) -> *mut c_void {
    unsafe { resize(pointer, size) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn PyObject_Free(pointer: *mut c_void) {
    unsafe { release(pointer) }
}

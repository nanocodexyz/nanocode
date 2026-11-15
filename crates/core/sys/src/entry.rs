use crate::libc;
use std::{
    ffi::CStr,
    process::ExitCode,
    sync::atomic::{AtomicPtr, AtomicUsize, Ordering},
};

static COUNT: AtomicUsize = AtomicUsize::new(0);
static ARGUMENTS: AtomicPtr<*const libc::c_char> = AtomicPtr::new(std::ptr::null_mut());

pub fn only_argument_in(candidates: &[&CStr]) -> bool {
    let arguments = ARGUMENTS.load(Ordering::Acquire);
    COUNT.load(Ordering::Acquire) == 2
        && candidates.contains(&unsafe { CStr::from_ptr(*arguments.add(1)) })
}

pub fn print(mut bytes: &[u8]) -> std::io::Result<()> {
    while !bytes.is_empty() {
        let written =
            unsafe { libc::write(libc::STDOUT_FILENO, bytes.as_ptr().cast(), bytes.len()) };
        if written < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(error);
            }
            continue;
        }
        bytes = &bytes[usize::try_from(written).expect("a successful write returns a count")..];
    }
    Ok(())
}

pub fn prepare(count: libc::c_int, arguments: *const *const libc::c_char) {
    ARGUMENTS.store(arguments.cast_mut(), Ordering::Release);
    COUNT.store(
        usize::try_from(count).expect("argc is not negative"),
        Ordering::Release,
    );
    for fd in 0..=2 {
        if unsafe { libc::fcntl(fd, libc::F_GETFD) } == -1
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::EBADF)
            && unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_RDWR) } == -1
        {
            unsafe { libc::abort() }
        }
    }
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN) };
    unsafe { libc::mallopt(libc::M_ARENA_MAX, 1) };
}

pub fn release_file_pages() {
    unsafe { libc::dl_iterate_phdr(Some(release_object), std::ptr::null_mut()) };
}

unsafe extern "C" fn release_object(
    info: *mut libc::dl_phdr_info,
    _: libc::size_t,
    _: *mut libc::c_void,
) -> libc::c_int {
    let info = unsafe { &*info };
    let vdso = unsafe { libc::getauxval(libc::AT_SYSINFO_EHDR) };
    if vdso != 0 && info.dlpi_addr == vdso {
        return 0;
    }
    let headers =
        unsafe { std::slice::from_raw_parts(info.dlpi_phdr, usize::from(info.dlpi_phnum)) };
    if headers.iter().any(|header| {
        header.p_type == libc::PT_DYNAMIC
            && unsafe {
                relocates_text((info.dlpi_addr + header.p_vaddr) as *const libc::Elf64_Dyn)
            }
    }) {
        return 0;
    }
    let page = usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) })
        .expect("the page size is positive");
    for header in headers
        .iter()
        .filter(|header| header.p_type == libc::PT_LOAD && header.p_flags & libc::PF_W == 0)
    {
        let start = (info.dlpi_addr + header.p_vaddr) as usize;
        let first = start.next_multiple_of(page);
        let end = (start + header.p_filesz as usize) / page * page;
        if end > first {
            unsafe { libc::madvise(first as *mut libc::c_void, end - first, libc::MADV_DONTNEED) };
        }
    }
    0
}

const unsafe fn relocates_text(mut entry: *const libc::Elf64_Dyn) -> bool {
    loop {
        let item = unsafe { *entry };
        match item.d_tag {
            libc::DT_NULL => return false,
            libc::DT_TEXTREL => return true,
            libc::DT_FLAGS if item.d_val & libc::DF_TEXTREL != 0 => return true,
            _ => entry = unsafe { entry.add(1) },
        }
    }
}

pub fn exit(code: ExitCode) -> ! {
    match (0..=u8::MAX).find(|&status| ExitCode::from(status) == code) {
        Some(status) => std::process::exit(i32::from(status)),
        None => unsafe { libc::abort() },
    }
}

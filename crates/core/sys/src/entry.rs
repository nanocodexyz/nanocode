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
}

pub fn exit(code: ExitCode) -> ! {
    match (0..=u8::MAX).find(|&status| ExitCode::from(status) == code) {
        Some(status) => std::process::exit(i32::from(status)),
        None => unsafe { libc::abort() },
    }
}

use crate::libc;
use std::process::ExitCode;

pub fn prepare() {
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

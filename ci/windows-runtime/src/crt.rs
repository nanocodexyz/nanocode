use std::{
    ffi::CStr,
    sync::atomic::{AtomicI32, Ordering},
    time::Duration,
};

use nanocode_sys::libc;

use crate::{Outcome, Report, ensure};

pub fn run(report: &mut Report) {
    let checks: &[(&str, fn() -> Outcome)] = &[
        ("crt.file_descriptors", file_descriptors),
        ("crt.stat", stat),
        ("crt.pipe_dup", pipe_dup),
        ("crt.handles", handles),
        ("crt.strings_locale", strings_locale),
        ("crt.time", time),
        ("crt.signal_raise", signal_raise),
        ("crt.sockets", sockets),
    ];
    for (name, check) in checks {
        report.guarded(name, Duration::from_secs(30), *check);
    }
}

fn path_c(name: &str) -> std::ffi::CString {
    let directory = std::env::temp_dir().join(format!("nanocode-windows-runtime-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&directory);
    std::ffi::CString::new(directory.join(name).to_string_lossy().into_owned()).expect("no NUL in a temp path")
}

fn file_descriptors() -> Outcome {
    let path = path_c("crt-file.bin");
    let fd = unsafe {
        libc::open(path.as_ptr(), libc::O_CREAT | libc::O_TRUNC | libc::O_RDWR | libc::O_BINARY, 0o600)
    };
    ensure(fd >= 0, || format!("open returned {fd}"))?;
    let data = b"crt bytes\n";
    let wrote = unsafe { libc::write(fd, data.as_ptr().cast(), data.len() as u32) };
    ensure(wrote == data.len() as i32, || format!("write returned {wrote}"))?;
    ensure(unsafe { libc::commit(fd) } == 0, || "_commit failed".into())?;
    let end = unsafe { libc::lseek(fd, 0, libc::SEEK_END) };
    ensure(end == data.len() as i32, || format!("lseek end {end}"))?;
    ensure(unsafe { libc::lseek(fd, 0, libc::SEEK_SET) } == 0, || "lseek set".into())?;
    let mut back = [0_u8; 10];
    let read = unsafe { libc::read(fd, back.as_mut_ptr().cast(), 10) };
    ensure(read == 10 && &back == data, || format!("read {read} {back:?}"))?;
    ensure(unsafe { libc::close(fd) } == 0, || "close".into())?;
    let wide: Vec<u16> = path.to_string_lossy().encode_utf16().chain([0]).collect();
    let wfd = unsafe { libc::wopen(wide.as_ptr(), libc::O_RDONLY | libc::O_BINARY) };
    ensure(wfd >= 0, || format!("_wopen returned {wfd}"))?;
    unsafe { libc::close(wfd) };
    ensure(unsafe { libc::access(path.as_ptr(), 0) } == 0, || "access".into())?;
    let renamed = path_c("crt-renamed.bin");
    ensure(unsafe { libc::rename(path.as_ptr(), renamed.as_ptr()) } == 0, || "rename".into())?;
    let missing = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY) };
    ensure(missing == -1, || "renamed file still opens".into())?;
    let fd = unsafe { libc::open(renamed.as_ptr(), libc::O_RDONLY | libc::O_BINARY) };
    let mode = std::ffi::CString::new("rb").expect("literal");
    let file = unsafe { libc::fdopen(fd, mode.as_ptr()) };
    ensure(!file.is_null(), || "_fdopen".into())?;
    ensure(unsafe { libc::fclose(file) } == 0, || "fclose".into())?;
    let _ = std::fs::remove_file(renamed.to_string_lossy().as_ref());
    Ok("_open/_write/_commit/_lseek/_read/_close/_wopen/_access/rename/_fdopen/fclose".into())
}

fn stat() -> Outcome {
    let path = path_c("crt-stat.bin");
    std::fs::write(path.to_string_lossy().as_ref(), vec![1_u8; 12345]).map_err(|error| error.to_string())?;
    let mut by_path: libc::stat = unsafe { std::mem::zeroed() };
    ensure(unsafe { libc::stat(path.as_ptr(), &mut by_path) } == 0, || "_stat64".into())?;
    ensure(by_path.st_size == 12345, || format!("st_size {}", by_path.st_size))?;
    ensure(i32::from(by_path.st_mode) & libc::S_IFMT == libc::S_IFREG, || format!("st_mode {:#o}", by_path.st_mode))?;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(|error| error.to_string())?.as_secs() as i64;
    ensure((now - by_path.st_mtime).abs() < 120, || format!("st_mtime {} vs now {now}", by_path.st_mtime))?;
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_BINARY) };
    let mut by_fd: libc::stat = unsafe { std::mem::zeroed() };
    ensure(unsafe { libc::fstat(fd, &mut by_fd) } == 0 && by_fd.st_size == 12345, || format!("_fstat64 size {}", by_fd.st_size))?;
    unsafe { libc::close(fd) };
    let directory = std::ffi::CString::new(std::env::temp_dir().to_string_lossy().trim_end_matches('\\').to_string()).expect("no NUL");
    let mut of_directory: libc::stat = unsafe { std::mem::zeroed() };
    ensure(unsafe { libc::stat(directory.as_ptr(), &mut of_directory) } == 0, || "_stat64 directory".into())?;
    ensure(i32::from(of_directory.st_mode) & libc::S_IFMT == libc::S_IFDIR, || format!("directory mode {:#o}", of_directory.st_mode))?;
    let _ = std::fs::remove_file(path.to_string_lossy().as_ref());
    Ok(format!("size 12345, regular file, mtime {}, directory mode {:#o}", by_path.st_mtime, of_directory.st_mode))
}

fn pipe_dup() -> Outcome {
    let mut fds = [0_i32; 2];
    ensure(unsafe { libc::pipe(fds.as_mut_ptr(), 4096, libc::O_BINARY) } == 0, || "_pipe".into())?;
    let copy = unsafe { libc::dup(fds[1]) };
    ensure(copy >= 0, || "_dup".into())?;
    let wrote = unsafe { libc::write(copy, b"dup".as_ptr().cast(), 3) };
    ensure(wrote == 3, || "write through dup".into())?;
    let target = unsafe { libc::dup(fds[1]) };
    ensure(unsafe { libc::dup2(fds[1], target) } == 0, || "_dup2".into())?;
    unsafe { libc::close(copy) };
    unsafe { libc::close(target) };
    unsafe { libc::close(fds[1]) };
    let mut back = [0_u8; 8];
    let read = unsafe { libc::read(fds[0], back.as_mut_ptr().cast(), 8) };
    ensure(read == 3 && &back[..3] == b"dup", || format!("read {read}"))?;
    let end = unsafe { libc::read(fds[0], back.as_mut_ptr().cast(), 8) };
    ensure(end == 0, || format!("read after close {end}"))?;
    unsafe { libc::close(fds[0]) };
    Ok("_pipe, _dup, _dup2, end of file after the writers closed".into())
}

fn handles() -> Outcome {
    let pid = unsafe { libc::getpid() };
    ensure(pid as u32 == std::process::id(), || format!("_getpid {pid}"))?;
    let handle = unsafe { libc::get_osfhandle(libc::STDOUT_FILENO) };
    ensure(handle != -1, || "_get_osfhandle stdout".into())?;
    let tty = unsafe { libc::isatty(libc::STDOUT_FILENO) };
    let path = path_c("crt-osf.bin");
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_CREAT | libc::O_RDWR | libc::O_BINARY, 0o600) };
    let raw = unsafe { libc::get_osfhandle(fd) };
    let mut duplicate: *mut core::ffi::c_void = std::ptr::null_mut();
    use nanocode_sys::windows_sys::Win32::Foundation::{DUPLICATE_SAME_ACCESS, DuplicateHandle, FALSE};
    use nanocode_sys::windows_sys::Win32::System::Threading::GetCurrentProcess;
    let duplicated = unsafe {
        DuplicateHandle(GetCurrentProcess(), raw as _, GetCurrentProcess(), &mut duplicate, 0, FALSE, DUPLICATE_SAME_ACCESS)
    };
    ensure(duplicated != 0, || "DuplicateHandle of an fd's handle".into())?;
    let reopened = unsafe { libc::open_osfhandle(duplicate as isize, libc::O_RDWR) };
    ensure(reopened >= 0 && reopened != fd, || format!("_open_osfhandle {reopened}"))?;
    unsafe { libc::close(reopened) };
    unsafe { libc::close(fd) };
    let _ = std::fs::remove_file(path.to_string_lossy().as_ref());
    Ok(format!("_getpid {pid}, stdout handle {handle:#x} isatty {tty}, _open_osfhandle {reopened}"))
}

fn strings_locale() -> Outcome {
    let message = unsafe { CStr::from_ptr(libc::strerror(libc::ENOENT)) }.to_string_lossy().into_owned();
    ensure(message.contains("No such file"), || format!("strerror(ENOENT) {message}"))?;
    let c = std::ffi::CString::new("C").expect("literal");
    let set = unsafe { libc::setlocale(libc::LC_ALL, c.as_ptr()) };
    ensure(!set.is_null(), || "setlocale".into())?;
    let a = std::ffi::CString::new("apple").expect("literal");
    let b = std::ffi::CString::new("banana").expect("literal");
    let order = unsafe { libc::strcoll(a.as_ptr(), b.as_ptr()) };
    ensure(order < 0, || format!("strcoll {order}"))?;
    let mut key = [0_i8; 32];
    let length = unsafe { libc::strxfrm(key.as_mut_ptr().cast(), a.as_ptr(), 32) };
    ensure(length == 5, || format!("strxfrm {length}"))?;
    let mut buffer = [0_u8; 16];
    unsafe { libc::memset(buffer.as_mut_ptr().cast(), 0x5a, 8) };
    unsafe { libc::memmove(buffer.as_mut_ptr().add(4).cast(), buffer.as_ptr().cast(), 8) };
    ensure(buffer[..12].iter().all(|byte| *byte == 0x5a) && buffer[12] == 0, || format!("{buffer:?}"))?;
    let errno_values = [
        (libc::EADDRINUSE, std::io::ErrorKind::AddrInUse),
        (libc::ECONNREFUSED, std::io::ErrorKind::ConnectionRefused),
        (libc::EWOULDBLOCK, std::io::ErrorKind::WouldBlock),
    ];
    let mut names = Vec::new();
    for (value, _) in errno_values {
        names.push(unsafe { CStr::from_ptr(libc::strerror(value)) }.to_string_lossy().into_owned());
    }
    Ok(format!("strerror(ENOENT) {message:?}, strcoll, strxfrm, memset/memmove, {names:?}"))
}

fn time() -> Outcome {
    let mut out: libc::time_t = 0;
    let now = unsafe { libc::time(&mut out) };
    let system = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(|error| error.to_string())?.as_secs() as i64;
    ensure(now == out && (now - system).abs() <= 2, || format!("_time64 {now} vs {system}"))?;
    Ok(format!("_time64 {now}"))
}

static SIGNALLED: AtomicI32 = AtomicI32::new(0);

extern "C" fn on_signal(signal: i32) {
    SIGNALLED.store(signal, Ordering::SeqCst);
}

fn signal_raise() -> Outcome {
    let previous = unsafe { libc::signal(libc::SIGTERM, on_signal as *const () as usize) };
    ensure(previous != libc::SIG_ERR as usize, || "signal".into())?;
    ensure(unsafe { libc::raise(libc::SIGTERM) } == 0, || "raise".into())?;
    let seen = SIGNALLED.load(Ordering::SeqCst);
    unsafe { libc::signal(libc::SIGTERM, libc::SIG_DFL) };
    ensure(seen == libc::SIGTERM, || format!("handler saw {seen}"))?;
    Ok("SIGTERM handler ran from raise".into())
}

fn sockets() -> Outcome {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").map_err(|error| error.to_string())?;
    let socket = unsafe { libc::socket(2, 1, 6) };
    ensure(socket != usize::MAX, || "socket".into())?;
    let mut kind = 0_i32;
    let mut length = 4_i32;
    let got = unsafe { libc::getsockopt(socket, 0xffff, 0x1008, (&raw mut kind).cast(), &mut length) };
    ensure(got == 0 && kind == 1, || format!("getsockopt {got} type {kind}"))?;
    let std::net::SocketAddr::V4(address) = listener.local_addr().map_err(|error| error.to_string())? else {
        return Err("not IPv4".into());
    };
    let mut raw = libc::sockaddr { sa_family: 2, sa_data: [0; 14] };
    let port = address.port().to_be_bytes();
    raw.sa_data[0] = port[0] as i8;
    raw.sa_data[1] = port[1] as i8;
    for (index, octet) in address.ip().octets().iter().enumerate() {
        raw.sa_data[2 + index] = *octet as i8;
    }
    let one = 1_i32;
    let set = unsafe { libc::setsockopt(socket, 0xffff, 4, (&raw const one).cast(), 4) };
    ensure(set == 0, || "setsockopt SO_REUSEADDR".into())?;
    let connected = unsafe { libc::connect(socket, &raw, 16) };
    ensure(connected == 0, || format!("connect {connected}"))?;
    let (_peer, from) = listener.accept().map_err(|error| error.to_string())?;
    Ok(format!("libc socket/getsockopt/setsockopt/connect reached the listener from {from}"))
}

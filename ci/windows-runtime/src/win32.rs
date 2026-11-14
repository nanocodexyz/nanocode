use std::{
    ffi::OsString,
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    ptr::{null, null_mut},
    sync::atomic::{AtomicU32, Ordering},
    time::Duration,
};

use nanocode_sys::windows_sys::{
    Win32::{
        Foundation::*,
        Globalization::*,
        Networking::WinSock::*,
        Security::{Authorization::*, Cryptography::*, *},
        Storage::FileSystem::*,
        System::{
            Com::*, Console::*, Diagnostics::Debug::*, Diagnostics::ToolHelp::*, Environment::*,
            IO::*, JobObjects::*, LibraryLoader::*, Memory::*, Performance::*, Pipes::*,
            Registry::*, Rpc::*, SystemInformation::*, Threading::*, Time::*,
            WindowsProgramming::*,
        },
        UI::Shell::*,
    },
    core::*,
};

use crate::{Outcome, Report, ensure, wine_gap};

const ERROR_FILE_NOT_FOUND_CODE: u32 = 2;
const ERROR_INSUFFICIENT_BUFFER_CODE: u32 = 122;
const ERROR_MORE_DATA_CODE: u32 = 234;
const ERROR_NO_MORE_FILES_CODE: u32 = 18;
const ERROR_INVALID_HANDLE_CODE: u32 = 6;
const ERROR_NOT_ALL_ASSIGNED_CODE: u32 = 1300;
const CREATE_ALWAYS_CODE: u32 = 2;
const FILE_BEGIN_CODE: u32 = 0;
const FILE_END_OF_FILE_INFO_CLASS: i32 = 6;
const FILE_STANDARD_INFO_CLASS: i32 = 1;
const TOKEN_QUERY_CODE: u32 = 8;
const SCS_64BIT_BINARY_CODE: u32 = 6;
const SO_TYPE_CODE: i32 = 0x1008;
const SOL_SOCKET_CODE: i32 = 0xffff;
const FIONBIO_CODE: u32 = 0x8004_667e;
const SOCK_DGRAM_CODE: i32 = 2;
const IPPROTO_UDP_CODE: i32 = 17;
const LOCALE_SNAME_CODE: u32 = 0x5c;
const LOCALE_USER_DEFAULT_CODE: u32 = 0x400;
const VER_PLATFORM_WIN32_NT_CODE: u32 = 2;
const S_FALSE_CODE: i32 = 1;
const CERT_FRIENDLY_NAME_PROP_ID_CODE: u32 = 11;
const CERT_STORE_ADD_ALWAYS_CODE: u32 = 4;
const ACCESS_RIGHTS_ALL: u32 = 0x001F_0003;

pub fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain([0]).collect()
}

pub fn wide_path(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain([0]).collect()
}

fn text_of(units: &[u16]) -> String {
    let end = units.iter().position(|unit| *unit == 0).unwrap_or(units.len());
    OsString::from_wide(&units[..end]).to_string_lossy().into_owned()
}

fn last(what: &str) -> String {
    format!("{what} failed with Win32 error {}", unsafe { GetLastError() })
}

fn scratch(name: &str) -> PathBuf {
    let directory = std::env::temp_dir().join(format!("nanocode-windows-runtime-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&directory);
    directory.join(name)
}

struct Owned(HANDLE);

impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            unsafe { CloseHandle(self.0) };
        }
    }
}

fn owned(handle: HANDLE, what: &str) -> Result<Owned, String> {
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Err(last(what));
    }
    Ok(Owned(handle))
}

pub fn under_wine() -> bool {
    let ntdll = unsafe { GetModuleHandleW(wide("ntdll.dll").as_ptr()) };
    !ntdll.is_null() && unsafe { GetProcAddress(ntdll, b"wine_get_version\0".as_ptr()) }.is_some()
}

pub fn run(report: &mut Report) {
    println!("wine {}", under_wine());
    let checks: &[(&str, fn() -> Outcome)] = &[
        ("sys.fill_random", fill_random),
        ("win32.BCryptGenRandom", bcrypt_random),
        ("win32.last_error", last_error),
        ("win32.handles", handles),
        ("win32.file_io", file_io),
        ("win32.file_attributes", file_attributes),
        ("win32.directories", directories),
        ("win32.paths_and_volumes", paths_and_volumes),
        ("win32.links", links),
        ("sys.junction", junction),
        ("win32.file_mapping", file_mapping),
        ("win32.anonymous_pipe", anonymous_pipe),
        ("win32.named_pipe", named_pipe),
        ("win32.events_mutexes", events_mutexes),
        ("win32.thread_pool_wait", thread_pool_wait),
        ("win32.threads", threads),
        ("win32.completion_port", completion_port),
        ("win32.overlapped_file", overlapped_file),
        ("win32.overlapped_cancel", overlapped_cancel),
        ("win32.process_exit_code", process_exit_code),
        ("win32.process_terminate", process_terminate),
        ("win32.process_attribute_list", process_attribute_list),
        ("win32.job_object", job_object),
        ("win32.toolhelp", toolhelp),
        ("win32.console", console),
        ("win32.console_ctrl", console_ctrl),
        ("win32.environment", environment),
        ("win32.strings", strings),
        ("win32.system_information", system_information),
        ("win32.time", time),
        ("win32.libraries", libraries),
        ("win32.file_version", file_version),
        ("win32.errors", errors),
        ("win32.registry", registry),
        ("win32.security", security),
        ("win32.certificate_store", certificate_store_raw),
        ("win32.certificate_chain", certificate_chain),
        ("sys.certificate_store", certificate_store),
        ("platform.native_certificates", native_certificates),
        ("win32.winsock_tcp", winsock_tcp),
        ("win32.winsock_udp", winsock_udp),
        ("win32.ole", ole),
        ("win32.shell_paths", shell_paths),
    ];
    for (name, check) in checks {
        report.guarded(name, Duration::from_secs(30), *check);
    }
}

fn fill_random() -> Outcome {
    let mut first = [0_u8; 64];
    let mut second = [0_u8; 64];
    nanocode_sys::fill_random(&mut first).map_err(|error| error.to_string())?;
    nanocode_sys::fill_random(&mut second).map_err(|error| error.to_string())?;
    let mut large = vec![0_u8; 1 << 20];
    nanocode_sys::fill_random(&mut large).map_err(|error| error.to_string())?;
    ensure(first != [0; 64] && first != second, || "two draws were zero or equal".into())?;
    let zeros = large.iter().filter(|byte| **byte == 0).count();
    ensure(zeros < 8192, || format!("{zeros} zero bytes in 1 MiB"))?;
    Ok(format!("64 B twice and 1 MiB, {zeros} zero bytes in the MiB"))
}

fn bcrypt_random() -> Outcome {
    let mut bytes = [0_u8; 32];
    let status = unsafe {
        BCryptGenRandom(null_mut(), bytes.as_mut_ptr(), 32, BCRYPT_USE_SYSTEM_PREFERRED_RNG)
    };
    ensure(status == 0, || format!("status {status:#x}"))?;
    ensure(bytes != [0; 32], || "all zero".into())?;
    Ok("32 bytes".into())
}

fn last_error() -> Outcome {
    unsafe { SetLastError(1234) };
    let read = unsafe { GetLastError() };
    ensure(read == 1234, || format!("read {read}"))?;
    let missing = wide(&scratch("missing-file").to_string_lossy());
    let attributes = unsafe { GetFileAttributesW(missing.as_ptr()) };
    let error = unsafe { GetLastError() };
    ensure(attributes == u32::MAX && error == ERROR_FILE_NOT_FOUND_CODE, || {
        format!("attributes {attributes:#x} error {error}")
    })?;
    Ok("SetLastError/GetLastError round trip and ERROR_FILE_NOT_FOUND".into())
}

fn handles() -> Outcome {
    let process = unsafe { GetCurrentProcess() };
    ensure(process as isize == -1, || format!("pseudo handle {process:?}"))?;
    let thread = unsafe { GetCurrentThread() };
    ensure(thread as isize == -2, || format!("thread pseudo handle {thread:?}"))?;
    let mut duplicate: HANDLE = null_mut();
    let duplicated = unsafe {
        DuplicateHandle(process, process, process, &mut duplicate, 0, FALSE, DUPLICATE_SAME_ACCESS)
    };
    ensure(duplicated != 0, || last("DuplicateHandle"))?;
    let duplicate = owned(duplicate, "DuplicateHandle")?;
    let set = unsafe { SetHandleInformation(duplicate.0, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) };
    ensure(set != 0, || last("SetHandleInformation"))?;
    let mut flags = 0_u32;
    let got = unsafe { GetHandleInformation(duplicate.0, &mut flags) };
    ensure(got != 0, || last("GetHandleInformation"))?;
    ensure(flags & HANDLE_FLAG_INHERIT != 0, || format!("flags {flags:#x}"))?;
    let mut code = 0_u32;
    let exit = unsafe { GetExitCodeProcess(duplicate.0, &mut code) };
    ensure(exit != 0 && code == STILL_ACTIVE as u32, || format!("exit code {code}"))?;
    Ok(format!("duplicated the process handle, inherit flag {flags:#x}, STILL_ACTIVE"))
}

fn create_file(path: &Path, access: u32, disposition: u32, flags: u32) -> Result<Owned, String> {
    let name = wide_path(path);
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            null(),
            disposition,
            flags,
            null_mut(),
        )
    };
    owned(handle, "CreateFileW")
}

fn file_io() -> Outcome {
    let path = scratch("file-io.bin");
    let file = create_file(&path, GENERIC_READ | GENERIC_WRITE, CREATE_ALWAYS_CODE, FILE_ATTRIBUTE_NORMAL)?;
    let payload: Vec<u8> = (0..100_000_u32).map(|index| (index % 251) as u8).collect();
    let mut written = 0_u32;
    let wrote = unsafe {
        WriteFile(file.0, payload.as_ptr(), payload.len() as u32, &mut written, null_mut())
    };
    ensure(wrote != 0 && written as usize == payload.len(), || last("WriteFile"))?;
    let mut position = 0_i64;
    let moved = unsafe { SetFilePointerEx(file.0, 0, &mut position, FILE_BEGIN_CODE) };
    ensure(moved != 0 && position == 0, || last("SetFilePointerEx"))?;
    let mut back = vec![0_u8; payload.len()];
    let mut read = 0_u32;
    let got = unsafe { ReadFile(file.0, back.as_mut_ptr(), back.len() as u32, &mut read, null_mut()) };
    ensure(got != 0 && back == payload, || last("ReadFile"))?;
    let file_type = unsafe { GetFileType(file.0) };
    ensure(file_type == FILE_TYPE_DISK, || format!("file type {file_type}"))?;
    let mut information: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    let informed = unsafe { GetFileInformationByHandle(file.0, &mut information) };
    ensure(informed != 0, || last("GetFileInformationByHandle"))?;
    let size = (u64::from(information.nFileSizeHigh) << 32) | u64::from(information.nFileSizeLow);
    ensure(size == payload.len() as u64 && information.nNumberOfLinks == 1, || {
        format!("size {size} links {}", information.nNumberOfLinks)
    })?;
    let mut standard = [0_u64; 3];
    let standard_ok = unsafe {
        GetFileInformationByHandleEx(file.0, FILE_STANDARD_INFO_CLASS, standard.as_mut_ptr().cast(), 24)
    };
    ensure(standard_ok != 0 && standard[1] == payload.len() as u64, || {
        format!("{} standard {:?}", last("GetFileInformationByHandleEx"), standard)
    })?;
    let mut basic: FILE_BASIC_INFO = unsafe { std::mem::zeroed() };
    let basic_ok = unsafe {
        GetFileInformationByHandleEx(
            file.0,
            FileBasicInfo,
            (&raw mut basic).cast(),
            std::mem::size_of::<FILE_BASIC_INFO>() as u32,
        )
    };
    ensure(basic_ok != 0 && basic.LastWriteTime > 0, || last("GetFileInformationByHandleEx(FileBasicInfo)"))?;
    let end_of_file = 4096_i64;
    let truncated = unsafe {
        SetFileInformationByHandle(file.0, FILE_END_OF_FILE_INFO_CLASS, (&raw const end_of_file).cast(), 8)
    };
    ensure(truncated != 0, || last("SetFileInformationByHandle(FileEndOfFileInfo)"))?;
    let stamp = FILETIME { dwLowDateTime: 0x1234_5678, dwHighDateTime: 0x01d0_0000 };
    let stamped = unsafe { SetFileTime(file.0, null(), null(), &stamp) };
    ensure(stamped != 0, || last("SetFileTime"))?;
    let mut final_path = vec![0_u16; 1024];
    let length = unsafe { GetFinalPathNameByHandleW(file.0, final_path.as_mut_ptr(), 1024, VOLUME_NAME_DOS) };
    ensure(length > 0 && length < 1024, || last("GetFinalPathNameByHandleW"))?;
    let final_text = text_of(&final_path);
    ensure(final_text.ends_with("file-io.bin"), || format!("final path {final_text}"))?;
    drop(file);
    let metadata = std::fs::metadata(&path).map_err(|error| error.to_string())?;
    ensure(metadata.len() == 4096, || format!("length after truncate {}", metadata.len()))?;
    let deleted = unsafe { DeleteFileW(wide_path(&path).as_ptr()) };
    ensure(deleted != 0, || last("DeleteFileW"))?;
    Ok(format!("100000 B written and read back, truncated to 4096, final path {final_text}"))
}

fn file_attributes() -> Outcome {
    let path = scratch("attributes.txt");
    std::fs::write(&path, b"attributes").map_err(|error| error.to_string())?;
    let name = wide_path(&path);
    let set = unsafe { SetFileAttributesW(name.as_ptr(), FILE_ATTRIBUTE_READONLY) };
    ensure(set != 0, || last("SetFileAttributesW"))?;
    let attributes = unsafe { GetFileAttributesW(name.as_ptr()) };
    ensure(attributes & FILE_ATTRIBUTE_READONLY != 0, || format!("attributes {attributes:#x}"))?;
    let mut data: WIN32_FILE_ATTRIBUTE_DATA = unsafe { std::mem::zeroed() };
    let got = unsafe { GetFileAttributesExW(name.as_ptr(), GetFileExInfoStandard, (&raw mut data).cast()) };
    ensure(got != 0 && data.nFileSizeLow == 10, || format!("{} size {}", last("GetFileAttributesExW"), data.nFileSizeLow))?;
    let cleared = unsafe { SetFileAttributesW(name.as_ptr(), FILE_ATTRIBUTE_NORMAL) };
    ensure(cleared != 0, || last("SetFileAttributesW normal"))?;
    let copy = scratch("attributes-copy.txt");
    let copy_name = wide_path(&copy);
    let copy_note = if under_wine() {
        std::fs::copy(&path, &copy).map_err(|error| error.to_string())?;
        "CopyFile2 skipped (Wine 9 does not implement it)"
    } else {
        let copied = unsafe { CopyFile2(name.as_ptr(), copy_name.as_ptr(), null()) };
        ensure(copied == 0, || format!("CopyFile2 HRESULT {copied:#x}"))?;
        "CopyFile2"
    };
    let moved_path = scratch("attributes-moved.txt");
    let moved_name = wide_path(&moved_path);
    let moved = unsafe { MoveFileExW(copy_name.as_ptr(), moved_name.as_ptr(), MOVEFILE_REPLACE_EXISTING) };
    ensure(moved != 0, || last("MoveFileExW"))?;
    let contents = std::fs::read(&moved_path).map_err(|error| error.to_string())?;
    ensure(contents == b"attributes", || "moved copy differs".into())?;
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&moved_path);
    Ok(format!("readonly set and cleared, size {}, {copy_note} then MoveFileExW", data.nFileSizeLow))
}

fn directories() -> Outcome {
    let directory = scratch("listing");
    let name = wide_path(&directory);
    let _ = std::fs::remove_dir_all(&directory);
    let created = unsafe { CreateDirectoryW(name.as_ptr(), null()) };
    ensure(created != 0, || last("CreateDirectoryW"))?;
    for index in 0..5 {
        std::fs::write(directory.join(format!("entry-{index}.txt")), b"x").map_err(|error| error.to_string())?;
    }
    let pattern = wide_path(&directory.join("*"));
    let mut data: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };
    let find = unsafe { FindFirstFileW(pattern.as_ptr(), &mut data) };
    ensure(find != INVALID_HANDLE_VALUE, || last("FindFirstFileW"))?;
    let mut names = vec![text_of(&data.cFileName)];
    loop {
        if unsafe { FindNextFileW(find, &mut data) } == 0 {
            let error = unsafe { GetLastError() };
            unsafe { FindClose(find) };
            ensure(error == ERROR_NO_MORE_FILES_CODE, || format!("FindNextFileW ended with {error}"))?;
            break;
        }
        names.push(text_of(&data.cFileName));
    }
    let entries = names.iter().filter(|name| name.starts_with("entry-")).count();
    ensure(entries == 5, || format!("names {names:?}"))?;
    for index in 0..5 {
        std::fs::remove_file(directory.join(format!("entry-{index}.txt"))).map_err(|error| error.to_string())?;
    }
    let removed = unsafe { RemoveDirectoryW(name.as_ptr()) };
    ensure(removed != 0, || last("RemoveDirectoryW"))?;
    Ok(format!("{} names found ({entries} entries)", names.len()))
}

fn paths_and_volumes() -> Outcome {
    let relative = wide("relative\\name.txt");
    let mut full = vec![0_u16; 1024];
    let mut file_part: PWSTR = null_mut();
    let length = unsafe { GetFullPathNameW(relative.as_ptr(), 1024, full.as_mut_ptr(), &mut file_part) };
    ensure(length > 0 && !file_part.is_null(), || last("GetFullPathNameW"))?;
    let full_text = text_of(&full);
    let expected = std::env::current_dir().map_err(|error| error.to_string())?.join("relative\\name.txt");
    ensure(Path::new(&full_text) == expected, || format!("{full_text} vs {}", expected.display()))?;
    let temp = std::env::temp_dir();
    let temp_name = wide_path(&temp);
    let mut long = vec![0_u16; 1024];
    let long_length = unsafe { GetLongPathNameW(temp_name.as_ptr(), long.as_mut_ptr(), 1024) };
    ensure(long_length > 0, || last("GetLongPathNameW"))?;
    let mut short = vec![0_u16; 1024];
    let short_length = unsafe { GetShortPathNameW(temp_name.as_ptr(), short.as_mut_ptr(), 1024) };
    ensure(short_length > 0, || last("GetShortPathNameW"))?;
    let mut volume = vec![0_u16; 1024];
    let volume_ok = unsafe { GetVolumePathNameW(temp_name.as_ptr(), volume.as_mut_ptr(), 1024) };
    ensure(volume_ok != 0, || last("GetVolumePathNameW"))?;
    let volume_text = text_of(&volume);
    let volume_name = wide(&volume_text);
    let drive = unsafe { GetDriveTypeW(volume_name.as_ptr()) };
    let mut free = 0_u64;
    let mut total = 0_u64;
    let mut total_free = 0_u64;
    let space = unsafe { GetDiskFreeSpaceExW(volume_name.as_ptr(), &mut free, &mut total, &mut total_free) };
    ensure(space != 0 && total > 0, || last("GetDiskFreeSpaceExW"))?;
    let mut drives = vec![0_u16; 512];
    let drives_length = unsafe { GetLogicalDriveStringsW(512, drives.as_mut_ptr()) };
    ensure(drives_length > 0, || last("GetLogicalDriveStringsW"))?;
    let mut first_volume = vec![0_u16; 260];
    let find = unsafe { FindFirstVolumeW(first_volume.as_mut_ptr(), 260) };
    let volumes = if find == INVALID_HANDLE_VALUE {
        format!("FindFirstVolumeW error {}", unsafe { GetLastError() })
    } else {
        let mut count = 1;
        let mut next = vec![0_u16; 260];
        while unsafe { FindNextVolumeW(find, next.as_mut_ptr(), 260) } != 0 {
            count += 1;
        }
        let mut paths = vec![0_u16; 1024];
        let mut returned = 0_u32;
        let _ = unsafe {
            GetVolumePathNamesForVolumeNameW(first_volume.as_ptr(), paths.as_mut_ptr(), 1024, &mut returned)
        };
        unsafe { FindVolumeClose(find) };
        format!("{count} volumes, first {}", text_of(&first_volume))
    };
    Ok(format!(
        "full {full_text}, volume {volume_text} drive type {drive}, {} GiB total, {volumes}",
        total >> 30
    ))
}

fn links() -> Outcome {
    let target = scratch("link-target.txt");
    std::fs::write(&target, b"linked").map_err(|error| error.to_string())?;
    let hard = scratch("link-hard.txt");
    let _ = std::fs::remove_file(&hard);
    let made = unsafe { CreateHardLinkW(wide_path(&hard).as_ptr(), wide_path(&target).as_ptr(), null()) };
    ensure(made != 0, || last("CreateHardLinkW"))?;
    let contents = std::fs::read(&hard).map_err(|error| format!("reading the hard link: {error}"))?;
    ensure(contents == b"linked", || "hard link differs".into())?;
    let symbolic = scratch("link-symbolic.txt");
    let _ = std::fs::remove_file(&symbolic);
    let created = unsafe {
        CreateSymbolicLinkW(
            wide_path(&symbolic).as_ptr(),
            wide_path(&target).as_ptr(),
            SYMBOLIC_LINK_FLAG_ALLOW_UNPRIVILEGED_CREATE,
        )
    };
    let symbolic_result = if created {
        wine_gap(std::fs::symlink_metadata(&symbolic).is_ok(), || "CreateSymbolicLinkW returned TRUE and created nothing".into())?;
        let through = std::fs::read(&symbolic).map_err(|error| format!("reading through the symbolic link: {error}"))?;
        ensure(through == b"linked", || "symbolic link content differs".into())?;
        let read = std::fs::read_link(&symbolic);
        if let Err(error) = &read {
            let _ = std::fs::remove_file(&symbolic);
            wine_gap(false, || format!("reading through the symbolic link works, but read_link (FSCTL_GET_REPARSE_POINT) fails: {error}"))?;
        }
        let read = read.map_err(|error| format!("read_link of the symbolic link: {error}"))?;
        format!("symbolic link reads {}", read.display())
    } else {
        let error = unsafe { GetLastError() };
        ensure(error == ERROR_PRIVILEGE_NOT_HELD, || format!("CreateSymbolicLinkW error {error}"))?;
        "symbolic link needs a privilege here (ERROR_PRIVILEGE_NOT_HELD)".into()
    };
    let _ = std::fs::remove_file(&symbolic);
    let _ = std::fs::remove_file(&hard);
    let _ = std::fs::remove_file(&target);
    Ok(format!("hard link read back, {symbolic_result}"))
}

fn junction() -> Outcome {
    let target = scratch("junction-target");
    let junction = scratch("junction-link");
    let _ = std::fs::remove_dir(&junction);
    let _ = std::fs::remove_dir_all(&target);
    std::fs::create_dir_all(&target).map_err(|error| error.to_string())?;
    std::fs::write(target.join("inside.txt"), b"through the junction").map_err(|error| error.to_string())?;
    if let Err(error) = nanocode_sys::junction::create(&target, &junction) {
        let _ = std::fs::remove_dir(&junction);
        wine_gap(error.raw_os_error() != Some(50), || format!("FSCTL_SET_REPARSE_POINT is not supported: {error}"))?;
        return Err(format!("create: {error}"));
    }
    let through = std::fs::read(junction.join("inside.txt")).map_err(|error| format!("read through: {error}"))?;
    ensure(through == b"through the junction", || "contents differ".into())?;
    let metadata = std::fs::symlink_metadata(&junction).map_err(|error| error.to_string())?;
    let link = std::fs::read_link(&junction).map_err(|error| format!("read_link: {error}"))?;
    std::fs::remove_dir(&junction).map_err(|error| error.to_string())?;
    let _ = std::fs::remove_dir_all(&target);
    Ok(format!("junction resolves, symlink metadata is_symlink={}, points at {}", metadata.is_symlink(), link.display()))
}

fn file_mapping() -> Outcome {
    let name = wide(&format!("Local\\nanocode-probe-{}", std::process::id()));
    let mapping = unsafe {
        CreateFileMappingW(INVALID_HANDLE_VALUE, null(), PAGE_READWRITE, 0, 65536, name.as_ptr())
    };
    let mapping = owned(mapping, "CreateFileMappingW")?;
    let view = unsafe { MapViewOfFile(mapping.0, FILE_MAP_ALL_ACCESS, 0, 0, 65536) };
    ensure(!view.Value.is_null(), || last("MapViewOfFile"))?;
    unsafe { std::ptr::copy_nonoverlapping(b"mapped".as_ptr(), view.Value.cast::<u8>(), 6) };
    let opened = unsafe { OpenFileMappingW(FILE_MAP_READ, FALSE, name.as_ptr()) };
    let opened = owned(opened, "OpenFileMappingW")?;
    let second = unsafe { MapViewOfFile(opened.0, FILE_MAP_READ, 0, 0, 0) };
    ensure(!second.Value.is_null(), || last("MapViewOfFile second"))?;
    let read = unsafe { std::slice::from_raw_parts(second.Value.cast::<u8>(), 6) };
    ensure(read == b"mapped", || format!("second view reads {read:?}"))?;
    let mut basic: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    let queried = unsafe {
        VirtualQuery(view.Value, &mut basic, std::mem::size_of::<MEMORY_BASIC_INFORMATION>())
    };
    ensure(queried == std::mem::size_of::<MEMORY_BASIC_INFORMATION>(), || last("VirtualQuery"))?;
    ensure(basic.State == MEM_COMMIT && basic.Type == MEM_MAPPED && basic.RegionSize >= 65536, || {
        format!("state {:#x} type {:#x} size {}", basic.State, basic.Type, basic.RegionSize)
    })?;
    let unmapped = unsafe { UnmapViewOfFile(second) } != 0 && unsafe { UnmapViewOfFile(view) } != 0;
    ensure(unmapped, || last("UnmapViewOfFile"))?;
    Ok(format!("two views share bytes, region {} B committed and mapped", basic.RegionSize))
}

fn anonymous_pipe() -> Outcome {
    let mut read: HANDLE = null_mut();
    let mut write: HANDLE = null_mut();
    let made = unsafe { CreatePipe(&mut read, &mut write, null(), 0) };
    ensure(made != 0, || last("CreatePipe"))?;
    let read = Owned(read);
    let write = Owned(write);
    let mut written = 0_u32;
    let wrote = unsafe { WriteFile(write.0, b"pipe!".as_ptr(), 5, &mut written, null_mut()) };
    ensure(wrote != 0 && written == 5, || last("WriteFile pipe"))?;
    let mut available = 0_u32;
    let peeked = unsafe { PeekNamedPipe(read.0, null_mut(), 0, null_mut(), &mut available, null_mut()) };
    ensure(peeked != 0 && available == 5, || format!("{} available {available}", last("PeekNamedPipe")))?;
    let file_type = unsafe { GetFileType(read.0) };
    ensure(file_type == FILE_TYPE_PIPE, || format!("file type {file_type}"))?;
    let mut buffer = [0_u8; 5];
    let mut got = 0_u32;
    let ok = unsafe { ReadFile(read.0, buffer.as_mut_ptr(), 5, &mut got, null_mut()) };
    ensure(ok != 0 && &buffer == b"pipe!", || last("ReadFile pipe"))?;
    Ok("5 bytes through CreatePipe, PeekNamedPipe saw 5".into())
}

fn named_pipe() -> Outcome {
    let name = wide(&format!("\\\\.\\pipe\\nanocode-probe-{}", std::process::id()));
    let server = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            4096,
            4096,
            0,
            null(),
        )
    };
    let server = owned(server, "CreateNamedPipeW")?;
    let waited = unsafe { WaitNamedPipeW(name.as_ptr(), 1000) };
    ensure(waited != 0, || last("WaitNamedPipeW"))?;
    let client = unsafe {
        CreateFileW(name.as_ptr(), GENERIC_READ | GENERIC_WRITE, 0, null(), OPEN_EXISTING, 0, null_mut())
    };
    let client = owned(client, "CreateFileW pipe client")?;
    let connected = unsafe { ConnectNamedPipe(server.0, null_mut()) };
    let error = unsafe { GetLastError() };
    ensure(connected != 0 || error == ERROR_PIPE_CONNECTED, || format!("ConnectNamedPipe error {error}"))?;
    let mut mode = PIPE_READMODE_BYTE;
    let set = unsafe { SetNamedPipeHandleState(client.0, &mut mode, null(), null()) };
    ensure(set != 0, || last("SetNamedPipeHandleState"))?;
    let mut state = 0_u32;
    let mut instances = 0_u32;
    let got = unsafe {
        GetNamedPipeHandleStateW(server.0, &mut state, &mut instances, null_mut(), null_mut(), null_mut(), 0)
    };
    ensure(got != 0 && instances == 1, || format!("{} instances {instances}", last("GetNamedPipeHandleStateW")))?;
    let mut written = 0_u32;
    let wrote = unsafe { WriteFile(client.0, b"named".as_ptr(), 5, &mut written, null_mut()) };
    ensure(wrote != 0, || last("WriteFile client"))?;
    let mut buffer = [0_u8; 5];
    let mut read = 0_u32;
    let ok = unsafe { ReadFile(server.0, buffer.as_mut_ptr(), 5, &mut read, null_mut()) };
    ensure(ok != 0 && &buffer == b"named", || last("ReadFile server"))?;
    Ok(format!("client connected, 5 bytes, server state {state:#x}"))
}

fn events_mutexes() -> Outcome {
    let name = wide(&format!("Local\\nanocode-probe-event-{}", std::process::id()));
    let event = owned(unsafe { CreateEventW(null(), TRUE, FALSE, name.as_ptr()) }, "CreateEventW")?;
    let timed_out = unsafe { WaitForSingleObject(event.0, 10) };
    ensure(timed_out == WAIT_TIMEOUT, || format!("unsignalled wait {timed_out}"))?;
    let opened = owned(unsafe { OpenEventW(SYNCHRONIZE | 2, FALSE, name.as_ptr()) }, "OpenEventW")?;
    ensure(unsafe { SetEvent(opened.0) } != 0, || last("SetEvent"))?;
    let signalled = unsafe { WaitForSingleObject(event.0, 1000) };
    ensure(signalled == WAIT_OBJECT_0, || format!("signalled wait {signalled}"))?;
    ensure(unsafe { ResetEvent(event.0) } != 0, || last("ResetEvent"))?;
    let mutex_name = wide(&format!("Local\\nanocode-probe-mutex-{}", std::process::id()));
    let mutex = owned(unsafe { CreateMutexW(null(), TRUE, mutex_name.as_ptr()) }, "CreateMutexW")?;
    let opened_mutex = owned(unsafe { OpenMutexW(SYNCHRONIZE, FALSE, mutex_name.as_ptr()) }, "OpenMutexW")?;
    ensure(unsafe { ReleaseMutex(mutex.0) } != 0, || last("ReleaseMutex"))?;
    let second = owned(unsafe { CreateEventW(null(), FALSE, TRUE, null()) }, "CreateEventW auto")?;
    let handles = [event.0, second.0];
    let any = unsafe { WaitForMultipleObjects(2, handles.as_ptr(), FALSE, 1000) };
    ensure(any == WAIT_OBJECT_0 + 1, || format!("WaitForMultipleObjects {any}"))?;
    let reacquired = unsafe { WaitForSingleObject(opened_mutex.0, 1000) };
    ensure(reacquired == WAIT_OBJECT_0, || format!("mutex wait {reacquired}"))?;
    ensure(unsafe { ReleaseMutex(opened_mutex.0) } != 0, || last("ReleaseMutex second"))?;
    Ok("named event and mutex, timeout, signal, WaitForMultipleObjects picked index 1".into())
}

static WAIT_CALLS: AtomicU32 = AtomicU32::new(0);

unsafe extern "system" fn on_wait(context: *mut core::ffi::c_void, timed_out: bool) {
    if !timed_out && context as usize == 77 {
        WAIT_CALLS.fetch_add(1, Ordering::SeqCst);
    }
}

fn thread_pool_wait() -> Outcome {
    let event = owned(unsafe { CreateEventW(null(), FALSE, FALSE, null()) }, "CreateEventW")?;
    let mut wait: HANDLE = null_mut();
    let registered = unsafe {
        RegisterWaitForSingleObject(&mut wait, event.0, Some(on_wait), 77 as *const _, INFINITE, WT_EXECUTEONLYONCE)
    };
    ensure(registered != 0, || last("RegisterWaitForSingleObject"))?;
    unsafe { SetEvent(event.0) };
    let start = std::time::Instant::now();
    while WAIT_CALLS.load(Ordering::SeqCst) == 0 && start.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(1));
    }
    let unregistered = unsafe { UnregisterWaitEx(wait, INVALID_HANDLE_VALUE) };
    ensure(unregistered != 0, || last("UnregisterWaitEx"))?;
    let calls = WAIT_CALLS.load(Ordering::SeqCst);
    ensure(calls == 1, || format!("callback ran {calls} times"))?;
    Ok(format!("callback ran once after {:?}", start.elapsed()))
}

unsafe extern "system" fn thread_main(parameter: *mut core::ffi::c_void) -> u32 {
    parameter as u32 + 1
}

fn threads() -> Outcome {
    let mut id = 0_u32;
    let thread = unsafe { CreateThread(null(), 0, Some(thread_main), 41 as *const _, 0, &mut id) };
    let thread = owned(thread, "CreateThread")?;
    let waited = unsafe { WaitForSingleObject(thread.0, 5000) };
    ensure(waited == WAIT_OBJECT_0, || format!("thread wait {waited}"))?;
    let mut code = 0_u32;
    ensure(unsafe { GetExitCodeThread(thread.0, &mut code) } != 0 && code == 42, || format!("exit code {code}"))?;
    let mut times = [FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 }; 4];
    let [a, b, c, d] = &mut times;
    ensure(unsafe { GetThreadTimes(thread.0, a, b, c, d) } != 0, || last("GetThreadTimes"))?;
    let mut low = 0_usize;
    let mut high = 0_usize;
    unsafe { GetCurrentThreadStackLimits(&mut low, &mut high) };
    let here = (&raw const low) as usize;
    ensure(low < here && here < high, || format!("stack {low:#x}..{high:#x} does not hold {here:#x}"))?;
    let mut guarantee = 0_u32;
    ensure(unsafe { SetThreadStackGuarantee(&mut guarantee) } != 0, || last("SetThreadStackGuarantee"))?;
    let remaining = nanocode_sys::current_thread_stack_remaining_bytes();
    ensure(remaining.is_some_and(|bytes| bytes > 0 && bytes < high - low), || format!("remaining {remaining:?}"))?;
    Ok(format!("thread {id} returned 42, stack {} KiB, remaining {remaining:?}", (high - low) >> 10))
}

fn completion_port() -> Outcome {
    let port = owned(unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, null_mut(), 0, 1) }, "CreateIoCompletionPort")?;
    let posted = unsafe { PostQueuedCompletionStatus(port.0, 7, 99, null()) };
    ensure(posted != 0, || last("PostQueuedCompletionStatus"))?;
    let mut bytes = 0_u32;
    let mut key = 0_usize;
    let mut overlapped: *mut OVERLAPPED = null_mut();
    let got = unsafe { GetQueuedCompletionStatus(port.0, &mut bytes, &mut key, &mut overlapped, 1000) };
    ensure(got != 0 && bytes == 7 && key == 99 && overlapped.is_null(), || {
        format!("{} bytes {bytes} key {key}", last("GetQueuedCompletionStatus"))
    })?;
    let empty = unsafe { GetQueuedCompletionStatus(port.0, &mut bytes, &mut key, &mut overlapped, 10) };
    let error = unsafe { GetLastError() };
    ensure(empty == 0 && error == WAIT_TIMEOUT, || format!("empty port returned {empty} error {error}"))?;
    Ok("posted packet received with its key and byte count, empty port times out".into())
}

fn overlapped_file() -> Outcome {
    let path = scratch("overlapped.bin");
    std::fs::write(&path, vec![9_u8; 8192]).map_err(|error| error.to_string())?;
    let file = create_file(&path, GENERIC_READ, OPEN_EXISTING, FILE_FLAG_OVERLAPPED)?;
    let port = owned(unsafe { CreateIoCompletionPort(file.0, null_mut(), 5, 1) }, "CreateIoCompletionPort file")?;
    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    overlapped.Anonymous.Anonymous.Offset = 4096;
    let mut buffer = vec![0_u8; 4096];
    let started = unsafe { ReadFile(file.0, buffer.as_mut_ptr(), 4096, null_mut(), &mut overlapped) };
    let error = unsafe { GetLastError() };
    ensure(started != 0 || error == ERROR_IO_PENDING, || format!("ReadFile overlapped error {error}"))?;
    let mut bytes = 0_u32;
    let mut key = 0_usize;
    let mut done: *mut OVERLAPPED = null_mut();
    let got = unsafe { GetQueuedCompletionStatus(port.0, &mut bytes, &mut key, &mut done, 5000) };
    ensure(got != 0 && bytes == 4096 && key == 5 && done == &raw mut overlapped, || {
        format!("{} bytes {bytes} key {key}", last("GetQueuedCompletionStatus file"))
    })?;
    let mut transferred = 0_u32;
    let result = unsafe { GetOverlappedResult(file.0, &overlapped, &mut transferred, FALSE) };
    ensure(result != 0 && transferred == 4096, || last("GetOverlappedResult"))?;
    ensure(buffer.iter().all(|byte| *byte == 9), || "wrong bytes".into())?;
    drop(file);
    let _ = std::fs::remove_file(&path);
    Ok("4096 B read at offset 4096 through a completion port".into())
}

fn overlapped_cancel() -> Outcome {
    let name = wide(&format!("\\\\.\\pipe\\nanocode-probe-cancel-{}", std::process::id()));
    let server = unsafe {
        CreateNamedPipeW(name.as_ptr(), PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED, PIPE_TYPE_BYTE, 1, 4096, 4096, 0, null())
    };
    let server = owned(server, "CreateNamedPipeW overlapped")?;
    let client = unsafe {
        CreateFileW(name.as_ptr(), GENERIC_READ | GENERIC_WRITE, 0, null(), OPEN_EXISTING, 0, null_mut())
    };
    let _client = owned(client, "CreateFileW client")?;
    let event = owned(unsafe { CreateEventW(null(), TRUE, FALSE, null()) }, "CreateEventW")?;
    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    overlapped.hEvent = event.0;
    let mut buffer = [0_u8; 16];
    let started = unsafe { ReadFile(server.0, buffer.as_mut_ptr(), 16, null_mut(), &mut overlapped) };
    let error = unsafe { GetLastError() };
    ensure(started == 0 && error == ERROR_IO_PENDING, || format!("read started {started} error {error}"))?;
    let cancelled = unsafe { CancelIoEx(server.0, &overlapped) };
    ensure(cancelled != 0, || last("CancelIoEx"))?;
    let mut transferred = 0_u32;
    let result = unsafe { GetOverlappedResult(server.0, &overlapped, &mut transferred, TRUE) };
    let error = unsafe { GetLastError() };
    ensure(result == 0 && error == 995, || format!("result {result} error {error}"))?;
    Ok("pending pipe read cancelled with ERROR_OPERATION_ABORTED".into())
}

pub fn quote(argument: &str) -> String {
    if !argument.is_empty() && !argument.contains([' ', '\t', '"']) {
        return argument.to_string();
    }
    format!("\"{}\"", argument.replace('"', "\\\""))
}

fn command_line(arguments: &[&str]) -> Vec<u16> {
    let exe = std::env::current_exe().expect("the current executable has a path");
    let mut line = quote(&exe.to_string_lossy());
    line.push_str(" --child");
    for argument in arguments {
        line.push(' ');
        line.push_str(&quote(argument));
    }
    wide(&line)
}

fn spawn_raw(arguments: &[&str], flags: u32, stdout: HANDLE) -> Result<(Owned, Owned, u32), String> {
    let mut line = command_line(arguments);
    let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
    startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    let inherit = if stdout.is_null() {
        FALSE
    } else {
        startup.dwFlags = STARTF_USESTDHANDLES;
        startup.hStdOutput = stdout;
        startup.hStdError = stdout;
        startup.hStdInput = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
        TRUE
    };
    let mut information: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    let created = unsafe {
        CreateProcessW(null(), line.as_mut_ptr(), null(), null(), inherit, flags, null(), null(), &startup, &mut information)
    };
    ensure(created != 0, || last("CreateProcessW"))?;
    Ok((Owned(information.hProcess), Owned(information.hThread), information.dwProcessId))
}

fn wait_exit(process: &Owned, limit: u32) -> Result<u32, String> {
    let waited = unsafe { WaitForSingleObject(process.0, limit) };
    ensure(waited == WAIT_OBJECT_0, || format!("process wait {waited}"))?;
    let mut code = 0_u32;
    ensure(unsafe { GetExitCodeProcess(process.0, &mut code) } != 0, || last("GetExitCodeProcess"))?;
    Ok(code)
}

fn process_exit_code() -> Outcome {
    let mut read: HANDLE = null_mut();
    let mut write: HANDLE = null_mut();
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: null_mut(),
        bInheritHandle: TRUE,
    };
    ensure(unsafe { CreatePipe(&mut read, &mut write, &attributes, 0) } != 0, || last("CreatePipe"))?;
    let read = Owned(read);
    let write = Owned(write);
    unsafe { SetHandleInformation(read.0, HANDLE_FLAG_INHERIT, 0) };
    let (process, _thread, id) = spawn_raw(&["print", "hello from a child"], CREATE_NO_WINDOW, write.0)?;
    drop(write);
    let mut output = Vec::new();
    let mut buffer = [0_u8; 256];
    loop {
        let mut got = 0_u32;
        let ok = unsafe { ReadFile(read.0, buffer.as_mut_ptr(), 256, &mut got, null_mut()) };
        if ok == 0 || got == 0 {
            break;
        }
        output.extend_from_slice(&buffer[..got as usize]);
    }
    let code = wait_exit(&process, 10_000)?;
    ensure(code == 0 && output == b"hello from a child", || format!("code {code} output {:?}", String::from_utf8_lossy(&output)))?;
    let opened = owned(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, id) }, "OpenProcess")?;
    let mut times = [FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 }; 4];
    let [a, b, c, d] = &mut times;
    ensure(unsafe { GetProcessTimes(opened.0, a, b, c, d) } != 0, || last("GetProcessTimes"))?;
    let (seven, _thread, _) = spawn_raw(&["exit", "7"], CREATE_NO_WINDOW, null_mut())?;
    let seven_code = wait_exit(&seven, 10_000)?;
    ensure(seven_code == 7, || format!("exit code {seven_code}"))?;
    Ok(format!("child {id} printed through an inherited pipe, second child exited 7"))
}

fn process_terminate() -> Outcome {
    let (process, _thread, _) = spawn_raw(&["sleep", "60000"], CREATE_NO_WINDOW, null_mut())?;
    let mut code = 0_u32;
    unsafe { GetExitCodeProcess(process.0, &mut code) };
    ensure(code == STILL_ACTIVE as u32, || format!("running child code {code}"))?;
    ensure(unsafe { TerminateProcess(process.0, 9) } != 0, || last("TerminateProcess"))?;
    let code = wait_exit(&process, 5000)?;
    ensure(code == 9, || format!("exit code {code}"))?;
    let (suspended, thread, _) = spawn_raw(&["exit", "3"], CREATE_NO_WINDOW | CREATE_SUSPENDED, null_mut())?;
    let still = unsafe { WaitForSingleObject(suspended.0, 50) };
    ensure(still == WAIT_TIMEOUT, || format!("suspended child wait {still}"))?;
    let previous = unsafe { ResumeThread(thread.0) };
    ensure(previous == 1, || format!("ResumeThread returned {previous}"))?;
    let resumed = wait_exit(&suspended, 10_000)?;
    ensure(resumed == 3, || format!("resumed code {resumed}"))?;
    Ok("sleeping child terminated with 9, suspended child resumed and exited 3".into())
}

fn process_attribute_list() -> Outcome {
    let mut bytes = 0_usize;
    unsafe { InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut bytes) };
    ensure(bytes > 0, || last("InitializeProcThreadAttributeList size"))?;
    let mut storage = vec![0_u64; bytes.div_ceil(8)];
    let list: LPPROC_THREAD_ATTRIBUTE_LIST = storage.as_mut_ptr().cast();
    ensure(unsafe { InitializeProcThreadAttributeList(list, 1, 0, &mut bytes) } != 0, || last("InitializeProcThreadAttributeList"))?;
    let mut read: HANDLE = null_mut();
    let mut write: HANDLE = null_mut();
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: null_mut(),
        bInheritHandle: TRUE,
    };
    ensure(unsafe { CreatePipe(&mut read, &mut write, &attributes, 0) } != 0, || last("CreatePipe"))?;
    let read = Owned(read);
    let write = Owned(write);
    let inherited = [write.0];
    let updated = unsafe {
        UpdateProcThreadAttribute(
            list,
            0,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
            inherited.as_ptr().cast(),
            std::mem::size_of::<HANDLE>(),
            null_mut(),
            null(),
        )
    };
    ensure(updated != 0, || last("UpdateProcThreadAttribute"))?;
    let mut startup: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdOutput = write.0;
    startup.StartupInfo.hStdError = write.0;
    startup.lpAttributeList = list;
    let mut line = command_line(&["print", "listed"]);
    let mut information: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    let created = unsafe {
        CreateProcessW(
            null(),
            line.as_mut_ptr(),
            null(),
            null(),
            TRUE,
            EXTENDED_STARTUPINFO_PRESENT | CREATE_NO_WINDOW,
            null(),
            null(),
            (&raw const startup).cast(),
            &mut information,
        )
    };
    let created_error = unsafe { GetLastError() };
    unsafe { DeleteProcThreadAttributeList(list) };
    ensure(created != 0, || format!("CreateProcessW with an attribute list failed with {created_error}"))?;
    let process = Owned(information.hProcess);
    let _thread = Owned(information.hThread);
    drop(write);
    let mut buffer = [0_u8; 64];
    let mut got = 0_u32;
    unsafe { ReadFile(read.0, buffer.as_mut_ptr(), 64, &mut got, null_mut()) };
    let code = wait_exit(&process, 10_000)?;
    ensure(code == 0 && &buffer[..got as usize] == b"listed", || {
        format!("code {code} read {:?}", String::from_utf8_lossy(&buffer[..got as usize]))
    })?;
    Ok(format!("attribute list of {bytes} B, child inherited only its pipe"))
}

fn job_object() -> Outcome {
    let job = owned(unsafe { CreateJobObjectW(null(), null()) }, "CreateJobObjectW")?;
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    let set = unsafe {
        SetInformationJobObject(
            job.0,
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    ensure(set != 0, || last("SetInformationJobObject"))?;
    let (process, thread, _) = spawn_raw(&["sleep", "60000"], CREATE_NO_WINDOW | CREATE_SUSPENDED, null_mut())?;
    ensure(unsafe { AssignProcessToJobObject(job.0, process.0) } != 0, || last("AssignProcessToJobObject"))?;
    unsafe { ResumeThread(thread.0) };
    let mut accounting: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { std::mem::zeroed() };
    let queried = unsafe {
        QueryInformationJobObject(
            job.0,
            JobObjectBasicAccountingInformation,
            (&raw mut accounting).cast(),
            std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
            null_mut(),
        )
    };
    ensure(queried != 0 && accounting.ActiveProcesses == 1, || {
        format!("{} active {}", last("QueryInformationJobObject"), accounting.ActiveProcesses)
    })?;
    let mut read_back: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    let queried_limits = unsafe {
        QueryInformationJobObject(
            job.0,
            JobObjectExtendedLimitInformation,
            (&raw mut read_back).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            null_mut(),
        )
    };
    ensure(queried_limits != 0, || last("QueryInformationJobObject limits"))?;
    ensure(unsafe { TerminateJobObject(job.0, 3) } != 0, || last("TerminateJobObject"))?;
    let code = wait_exit(&process, 5000)?;
    ensure(code == 3, || format!("job member exit code {code}"))?;
    wine_gap(read_back.BasicLimitInformation.LimitFlags & JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE != 0, || {
        format!("QueryInformationJobObject(JobObjectExtendedLimitInformation) succeeded but read back flags {:#x}", read_back.BasicLimitInformation.LimitFlags)
    })?;
    Ok("kill-on-close job, one active process, TerminateJobObject exit 3".into())
}

fn toolhelp() -> Outcome {
    let snapshot = owned(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }, "CreateToolhelp32Snapshot")?;
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    ensure(unsafe { Process32FirstW(snapshot.0, &mut entry) } != 0, || last("Process32FirstW"))?;
    let mut count = 1;
    let mut found = None;
    loop {
        if entry.th32ProcessID == std::process::id() {
            found = Some((text_of(&entry.szExeFile), entry.th32ParentProcessID));
        }
        if unsafe { Process32NextW(snapshot.0, &mut entry) } == 0 {
            break;
        }
        count += 1;
    }
    let (name, parent) = found.ok_or_else(|| format!("own process not among {count}"))?;
    let exe = std::env::current_exe().map_err(|error| error.to_string())?;
    let expected = exe.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    ensure(name.eq_ignore_ascii_case(&expected), || format!("named {name}, expected {expected}"))?;
    Ok(format!("{count} processes, found self as {name} with parent {parent}"))
}

fn console() -> Outcome {
    let mut notes = Vec::new();
    for (label, which) in [("stdin", STD_INPUT_HANDLE), ("stdout", STD_OUTPUT_HANDLE), ("stderr", STD_ERROR_HANDLE)] {
        let handle = unsafe { GetStdHandle(which) };
        ensure(handle != INVALID_HANDLE_VALUE, || last("GetStdHandle"))?;
        let file_type = unsafe { GetFileType(handle) };
        let mut mode = 0_u32;
        let console = unsafe { GetConsoleMode(handle, &mut mode) } != 0;
        notes.push(format!("{label} type {file_type} console {console} mode {mode:#x}"));
        if console && label == "stdout" {
            let mut info: CONSOLE_SCREEN_BUFFER_INFO = unsafe { std::mem::zeroed() };
            ensure(unsafe { GetConsoleScreenBufferInfo(handle, &mut info) } != 0, || last("GetConsoleScreenBufferInfo"))?;
            notes.push(format!("buffer {}x{}", info.dwSize.X, info.dwSize.Y));
            let text = wide("");
            let mut written = 0_u32;
            ensure(unsafe { WriteConsoleW(handle, text.as_ptr().cast(), 0, &mut written, null()) } != 0, || last("WriteConsoleW"))?;
        }
        if console && label == "stdin" {
            let mut events = 0_u32;
            ensure(unsafe { GetNumberOfConsoleInputEvents(handle, &mut events) } != 0, || last("GetNumberOfConsoleInputEvents"))?;
            notes.push(format!("{events} input events"));
        }
    }
    let input_page = unsafe { GetConsoleCP() };
    let output_page = unsafe { GetConsoleOutputCP() };
    let window = unsafe { GetConsoleWindow() };
    notes.push(format!("code pages {input_page}/{output_page} window {}", !window.is_null()));
    Ok(notes.join(", "))
}

static CTRL_SEEN: AtomicU32 = AtomicU32::new(u32::MAX);

unsafe extern "system" fn on_ctrl(kind: u32) -> BOOL {
    CTRL_SEEN.store(kind, Ordering::SeqCst);
    TRUE
}

pub fn ctrl_child() -> i32 {
    if unsafe { SetConsoleCtrlHandler(Some(on_ctrl), TRUE) } == 0 {
        return 110;
    }
    if unsafe { GenerateConsoleCtrlEvent(CTRL_C_EVENT, 0) } == 0 {
        return 112;
    }
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(10) {
        let seen = CTRL_SEEN.load(Ordering::SeqCst);
        if seen != u32::MAX {
            return 20 + seen as i32;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    111
}

fn console_ctrl() -> Outcome {
    ensure(unsafe { SetConsoleCtrlHandler(Some(on_ctrl), TRUE) } != 0, || last("SetConsoleCtrlHandler add"))?;
    ensure(unsafe { SetConsoleCtrlHandler(Some(on_ctrl), FALSE) } != 0, || last("SetConsoleCtrlHandler remove"))?;
    let mut child = std::process::Command::new(std::env::current_exe().map_err(|error| error.to_string())?);
    use std::os::windows::process::CommandExt;
    child.args(["--child", "ctrl"]).creation_flags(CREATE_NEW_CONSOLE);
    let status = child.status().map_err(|error| error.to_string())?;
    ensure(status.code() != Some(112), || "GenerateConsoleCtrlEvent failed in a child with its own console".into())?;
    ensure(status.code() == Some(20), || format!("child exit {status:?}, 20 means its handler saw CTRL_C_EVENT"))?;
    Ok("handler added and removed, a child on its own console saw the CTRL_C_EVENT it sent itself".into())
}

fn environment() -> Outcome {
    let name = wide("NANOCODE_PROBE_VARIABLE");
    let value = wide("probe value \u{2713}");
    ensure(unsafe { SetEnvironmentVariableW(name.as_ptr(), value.as_ptr()) } != 0, || last("SetEnvironmentVariableW"))?;
    let read = std::env::var("NANOCODE_PROBE_VARIABLE").map_err(|error| error.to_string())?;
    ensure(read == "probe value \u{2713}", || format!("std reads {read}"))?;
    let block = unsafe { GetEnvironmentStringsW() };
    ensure(!block.is_null(), || last("GetEnvironmentStringsW"))?;
    let mut count = 0;
    let mut found = false;
    let mut cursor = block;
    loop {
        let mut length = 0;
        while unsafe { *cursor.add(length) } != 0 {
            length += 1;
        }
        if length == 0 {
            break;
        }
        let entry = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(cursor, length) });
        found |= entry.starts_with("NANOCODE_PROBE_VARIABLE=");
        count += 1;
        cursor = unsafe { cursor.add(length + 1) };
    }
    ensure(unsafe { FreeEnvironmentStringsW(block) } != 0, || last("FreeEnvironmentStringsW"))?;
    ensure(found, || format!("variable not among {count}"))?;
    let source = wide("[%NANOCODE_PROBE_VARIABLE%]");
    let mut expanded = vec![0_u16; 256];
    let length = unsafe { ExpandEnvironmentStringsW(source.as_ptr(), expanded.as_mut_ptr(), 256) };
    ensure(length > 0 && text_of(&expanded) == "[probe value \u{2713}]", || format!("expanded {}", text_of(&expanded)))?;
    let need = unsafe { NeedCurrentDirectoryForExePathW(wide("cmd.exe").as_ptr()) };
    Ok(format!("{count} variables, expansion works, NeedCurrentDirectoryForExePathW {need}"))
}

fn strings() -> Outcome {
    let text = "h\u{e9}llo \u{2713} \u{1f600}";
    let mut units = vec![0_u16; 64];
    let made = unsafe {
        MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, text.as_ptr(), text.len() as i32, units.as_mut_ptr(), 64)
    };
    ensure(made > 0, || last("MultiByteToWideChar"))?;
    let expected: Vec<u16> = text.encode_utf16().collect();
    ensure(units[..made as usize] == expected[..], || "UTF-16 differs".into())?;
    let mut bytes = vec![0_u8; 64];
    let back = unsafe {
        WideCharToMultiByte(CP_UTF8, WC_ERR_INVALID_CHARS, units.as_ptr(), made, bytes.as_mut_ptr(), 64, null(), null_mut())
    };
    ensure(back as usize == text.len() && &bytes[..back as usize] == text.as_bytes(), || last("WideCharToMultiByte"))?;
    let upper = wide("ABC");
    let lower = wide("abc");
    let compared = unsafe { CompareStringOrdinal(upper.as_ptr(), -1, lower.as_ptr(), -1, TRUE) };
    ensure(compared == CSTR_EQUAL, || format!("CompareStringOrdinal {compared}"))?;
    let mut mapped = vec![0_u16; 16];
    let source = wide("mixed Case");
    let empty = wide("");
    let length = unsafe {
        LCMapStringEx(empty.as_ptr(), LCMAP_UPPERCASE, source.as_ptr(), -1, mapped.as_mut_ptr(), 16, null(), null(), 0)
    };
    ensure(length > 0 && text_of(&mapped) == "MIXED CASE", || format!("{} {}", last("LCMapStringEx"), text_of(&mapped)))?;
    let code_page = unsafe { GetACP() };
    let lcid = unsafe { GetUserDefaultLCID() };
    let mut locale = vec![0_u16; 85];
    let locale_length = unsafe { GetLocaleInfoW(LOCALE_USER_DEFAULT_CODE, LOCALE_SNAME_CODE, locale.as_mut_ptr(), 85) };
    ensure(locale_length > 0, || last("GetLocaleInfoW"))?;
    Ok(format!("UTF-8 round trip, ordinal compare, uppercase map, ACP {code_page}, LCID {lcid:#x}, locale {}", text_of(&locale)))
}

fn system_information() -> Outcome {
    let mut info: SYSTEM_INFO = unsafe { std::mem::zeroed() };
    unsafe { GetSystemInfo(&mut info) };
    ensure(info.dwPageSize == 4096 && info.dwNumberOfProcessors > 0 && info.dwAllocationGranularity == 65536, || {
        format!("page {} processors {} granularity {}", info.dwPageSize, info.dwNumberOfProcessors, info.dwAllocationGranularity)
    })?;
    let mut version: OSVERSIONINFOW = unsafe { std::mem::zeroed() };
    version.dwOSVersionInfoSize = std::mem::size_of::<OSVERSIONINFOW>() as u32;
    ensure(unsafe { GetVersionExW(&mut version) } != 0, || last("GetVersionExW"))?;
    ensure(version.dwPlatformId == VER_PLATFORM_WIN32_NT_CODE, || format!("platform {}", version.dwPlatformId))?;
    let packed = unsafe { GetVersion() };
    let ticks = unsafe { GetTickCount64() };
    let mut frequency = 0_i64;
    let mut first = 0_i64;
    let mut second = 0_i64;
    ensure(unsafe { QueryPerformanceFrequency(&mut frequency) } != 0 && frequency > 0, || last("QueryPerformanceFrequency"))?;
    unsafe { QueryPerformanceCounter(&mut first) };
    std::thread::sleep(Duration::from_millis(5));
    unsafe { QueryPerformanceCounter(&mut second) };
    ensure(second > first, || "counter did not advance".into())?;
    let mut name = vec![0_u16; 256];
    let mut length = 256_u32;
    ensure(unsafe { GetComputerNameExW(ComputerNamePhysicalDnsHostname, name.as_mut_ptr(), &mut length) } != 0, || last("GetComputerNameExW"))?;
    let mut system = vec![0_u16; 260];
    let system_length = unsafe { GetSystemDirectoryW(system.as_mut_ptr(), 260) };
    ensure(system_length > 0, || last("GetSystemDirectoryW"))?;
    let mut adjustment = 0_u32;
    let mut increment = 0_u32;
    let mut disabled = FALSE;
    ensure(unsafe { GetSystemTimeAdjustment(&mut adjustment, &mut increment, &mut disabled) } != 0, || last("GetSystemTimeAdjustment"))?;
    let mut user = vec![0_u16; 257];
    let mut user_length = 257_u32;
    ensure(unsafe { GetUserNameW(user.as_mut_ptr(), &mut user_length) } != 0, || last("GetUserNameW"))?;
    Ok(format!(
        "{} CPUs, Windows {}.{} build {} (GetVersion {packed:#x}), uptime {} s, QPC {frequency} Hz, host {}, system {}, user {}",
        info.dwNumberOfProcessors,
        version.dwMajorVersion,
        version.dwMinorVersion,
        version.dwBuildNumber,
        ticks / 1000,
        text_of(&name),
        text_of(&system),
        text_of(&user)
    ))
}

fn time() -> Outcome {
    let system = SYSTEMTIME {
        wYear: 2026,
        wMonth: 11,
        wDayOfWeek: 6,
        wDay: 14,
        wHour: 12,
        wMinute: 34,
        wSecond: 56,
        wMilliseconds: 789,
    };
    let mut file = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    ensure(unsafe { SystemTimeToFileTime(&system, &mut file) } != 0, || last("SystemTimeToFileTime"))?;
    let ticks = (u64::from(file.dwHighDateTime) << 32) | u64::from(file.dwLowDateTime);
    let unix = ticks / 10_000_000 - 11_644_473_600;
    ensure(unix == 1_794_659_696, || format!("unix seconds {unix}"))?;
    let mut back: SYSTEMTIME = unsafe { std::mem::zeroed() };
    ensure(unsafe { FileTimeToSystemTime(&file, &mut back) } != 0, || last("FileTimeToSystemTime"))?;
    ensure(back.wYear == 2026 && back.wMilliseconds == 789 && back.wDayOfWeek == 6, || "round trip differs".into())?;
    let mut zone: TIME_ZONE_INFORMATION = unsafe { std::mem::zeroed() };
    let zone_id = unsafe { GetTimeZoneInformation(&mut zone) };
    ensure(zone_id != u32::MAX, || last("GetTimeZoneInformation"))?;
    let mut local: SYSTEMTIME = unsafe { std::mem::zeroed() };
    ensure(unsafe { SystemTimeToTzSpecificLocalTime(&zone, &system, &mut local) } != 0, || last("SystemTimeToTzSpecificLocalTime"))?;
    Ok(format!("FILETIME round trip to unix {unix}, zone {} bias {} min", text_of(&zone.StandardName), zone.Bias))
}

fn libraries() -> Outcome {
    let kernel = unsafe { GetModuleHandleW(wide("kernel32.dll").as_ptr()) };
    ensure(!kernel.is_null(), || last("GetModuleHandleW"))?;
    let address = unsafe { GetProcAddress(kernel, b"GetTickCount64\0".as_ptr()) };
    let address = address.ok_or_else(|| last("GetProcAddress"))?;
    let tick: unsafe extern "system" fn() -> u64 = unsafe { std::mem::transmute(address) };
    let through = unsafe { tick() };
    ensure(through > 0, || "GetTickCount64 through GetProcAddress returned 0".into())?;
    let version = unsafe { LoadLibraryW(wide("version.dll").as_ptr()) };
    ensure(!version.is_null(), || last("LoadLibraryW"))?;
    ensure(unsafe { FreeLibrary(version) } != 0, || last("FreeLibrary"))?;
    let system = unsafe { LoadLibraryExW(wide("bcrypt.dll").as_ptr(), null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) };
    ensure(!system.is_null(), || last("LoadLibraryExW"))?;
    unsafe { FreeLibrary(system) };
    let mut path = vec![0_u16; 1024];
    let length = unsafe { GetModuleFileNameW(null_mut(), path.as_mut_ptr(), 1024) };
    ensure(length > 0, || last("GetModuleFileNameW"))?;
    let exe = std::env::current_exe().map_err(|error| error.to_string())?;
    ensure(Path::new(&text_of(&path)) == exe, || format!("{} vs {}", text_of(&path), exe.display()))?;
    let directory = std::env::temp_dir();
    let cookie = unsafe { AddDllDirectory(wide_path(&directory).as_ptr()) };
    ensure(!cookie.is_null(), || last("AddDllDirectory"))?;
    ensure(unsafe { RemoveDllDirectory(cookie) } != 0, || last("RemoveDllDirectory"))?;
    Ok("kernel32 GetTickCount64 through GetProcAddress, LoadLibraryW/ExW, module path, DLL directory".into())
}

fn file_version() -> Outcome {
    let mut system = vec![0_u16; 260];
    let length = unsafe { GetSystemDirectoryW(system.as_mut_ptr(), 260) };
    ensure(length > 0, || last("GetSystemDirectoryW"))?;
    let path = wide(&format!("{}\\kernel32.dll", text_of(&system)));
    let mut handle = 0_u32;
    let size = unsafe { GetFileVersionInfoSizeW(path.as_ptr(), &mut handle) };
    ensure(size > 0, || last("GetFileVersionInfoSizeW"))?;
    let mut data = vec![0_u8; size as usize];
    ensure(unsafe { GetFileVersionInfoW(path.as_ptr(), 0, size, data.as_mut_ptr().cast()) } != 0, || last("GetFileVersionInfoW"))?;
    let mut fixed: *mut core::ffi::c_void = null_mut();
    let mut fixed_length = 0_u32;
    let root = wide("\\");
    ensure(unsafe { VerQueryValueW(data.as_ptr().cast(), root.as_ptr(), &mut fixed, &mut fixed_length) } != 0, || last("VerQueryValueW"))?;
    let words = unsafe { std::slice::from_raw_parts(fixed.cast::<u32>(), 4) };
    ensure(words[0] == 0xFEEF_04BD, || format!("signature {:#x}", words[0]))?;
    let binary = unsafe {
        let mut kind = 0_u32;
        let exe = std::env::current_exe().map_err(|error| error.to_string())?;
        (GetBinaryTypeW(wide_path(&exe).as_ptr(), &mut kind) != 0).then_some(kind)
    };
    ensure(binary == Some(SCS_64BIT_BINARY_CODE), || format!("binary type {binary:?}"))?;
    Ok(format!("kernel32 version {}.{}, own binary is 64-bit", words[2] >> 16, words[2] & 0xffff))
}

fn errors() -> Outcome {
    let mut buffer: PWSTR = null_mut();
    let length = unsafe {
        FormatMessageW(
            FORMAT_MESSAGE_FROM_SYSTEM | FORMAT_MESSAGE_ALLOCATE_BUFFER | FORMAT_MESSAGE_IGNORE_INSERTS,
            null(),
            ERROR_FILE_NOT_FOUND_CODE,
            0,
            (&raw mut buffer).cast(),
            0,
            null(),
        )
    };
    ensure(length > 0 && !buffer.is_null(), || last("FormatMessageW"))?;
    let message = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(buffer, length as usize) });
    ensure(unsafe { LocalFree(buffer.cast()) }.is_null(), || last("LocalFree"))?;
    let previous = unsafe { GetErrorMode() };
    let set = unsafe { SetErrorMode(previous) };
    ensure(set == previous, || format!("SetErrorMode returned {set:#x} for {previous:#x}"))?;
    let io = std::io::Error::from_raw_os_error(ERROR_FILE_NOT_FOUND_CODE as i32);
    Ok(format!("message {:?}, std says {io}, error mode {previous:#x}", message.trim()))
}

fn registry() -> Outcome {
    let path = wide(&format!("Software\\NanocodeProbe{}", std::process::id()));
    let mut key: HKEY = null_mut();
    let mut disposition = 0_u32;
    let created = unsafe {
        RegCreateKeyExW(HKEY_CURRENT_USER, path.as_ptr(), 0, null(), REG_OPTION_NON_VOLATILE, KEY_ALL_ACCESS, null(), &mut key, &mut disposition)
    };
    ensure(created == 0 && disposition == REG_CREATED_NEW_KEY, || format!("RegCreateKeyExW {created} disposition {disposition}"))?;
    let text_name = wide("text");
    let text_value = wide("value \u{2713}");
    let set = unsafe {
        RegSetValueExW(key, text_name.as_ptr(), 0, REG_SZ, text_value.as_ptr().cast(), (text_value.len() * 2) as u32)
    };
    ensure(set == 0, || format!("RegSetValueExW {set}"))?;
    let number_name = wide("number");
    let number = 0xC0FFEE_u32;
    let set_number = unsafe { RegSetValueExW(key, number_name.as_ptr(), 0, REG_DWORD, (&raw const number).cast(), 4) };
    ensure(set_number == 0, || format!("RegSetValueExW dword {set_number}"))?;
    let mut kind = 0_u32;
    let mut data = vec![0_u16; 64];
    let mut length = 128_u32;
    let queried = unsafe { RegQueryValueExW(key, text_name.as_ptr(), null(), &mut kind, data.as_mut_ptr().cast(), &mut length) };
    ensure(queried == 0 && kind == REG_SZ && text_of(&data) == "value \u{2713}", || format!("RegQueryValueExW {queried} kind {kind}"))?;
    let mut read_number = 0_u32;
    let mut number_length = 4_u32;
    let queried_number = unsafe {
        RegQueryValueExW(key, number_name.as_ptr(), null(), &mut kind, (&raw mut read_number).cast(), &mut number_length)
    };
    ensure(queried_number == 0 && read_number == number, || format!("dword {queried_number} {read_number:#x}"))?;
    let mut sub: HKEY = null_mut();
    let sub_name = wide("child");
    ensure(unsafe { RegCreateKeyW(key, sub_name.as_ptr(), &mut sub) } == 0, || "RegCreateKeyW".into())?;
    unsafe { RegCloseKey(sub) };
    let mut subkeys = 0_u32;
    let mut values = 0_u32;
    let info = unsafe {
        RegQueryInfoKeyW(key, null_mut(), null_mut(), null(), &mut subkeys, null_mut(), null_mut(), &mut values, null_mut(), null_mut(), null_mut(), null_mut())
    };
    ensure(info == 0 && subkeys == 1 && values == 2, || format!("RegQueryInfoKeyW {info} subkeys {subkeys} values {values}"))?;
    let mut name = vec![0_u16; 64];
    let mut name_length = 64_u32;
    let enumerated = unsafe { RegEnumKeyExW(key, 0, name.as_mut_ptr(), &mut name_length, null(), null_mut(), null_mut(), null_mut()) };
    ensure(enumerated == 0 && text_of(&name) == "child", || format!("RegEnumKeyExW {enumerated}"))?;
    let mut value_names = Vec::new();
    for index in 0..2 {
        let mut value_name = vec![0_u16; 64];
        let mut value_length = 64_u32;
        let read = unsafe {
            RegEnumValueW(key, index, value_name.as_mut_ptr(), &mut value_length, null(), null_mut(), null_mut(), null_mut())
        };
        ensure(read == 0, || format!("RegEnumValueW {read}"))?;
        value_names.push(text_of(&value_name));
    }
    let mut opened: HKEY = null_mut();
    ensure(unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, path.as_ptr(), 0, KEY_READ, &mut opened) } == 0, || "RegOpenKeyExW".into())?;
    unsafe { RegCloseKey(opened) };
    ensure(unsafe { RegFlushKey(key) } == 0, || "RegFlushKey".into())?;
    ensure(unsafe { RegDeleteValueW(key, number_name.as_ptr()) } == 0, || "RegDeleteValueW".into())?;
    ensure(unsafe { RegDeleteKeyExW(key, sub_name.as_ptr(), KEY_WOW64_64KEY, 0) } == 0, || "RegDeleteKeyExW".into())?;
    unsafe { RegCloseKey(key) };
    ensure(unsafe { RegDeleteKeyW(HKEY_CURRENT_USER, path.as_ptr()) } == 0, || "RegDeleteKeyW".into())?;
    let mut remote: HKEY = null_mut();
    let connected = unsafe { RegConnectRegistryW(null(), HKEY_LOCAL_MACHINE, &mut remote) };
    if connected == 0 {
        unsafe { RegCloseKey(remote) };
    }
    Ok(format!("key with values {value_names:?}, subkey, deleted, RegConnectRegistryW {connected}"))
}

fn security() -> Outcome {
    let mut token: HANDLE = null_mut();
    let opened = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY_CODE, &mut token) };
    ensure(opened != 0, || last("OpenProcessToken"))?;
    let token = Owned(token);
    let mut luid = LUID { LowPart: 0, HighPart: 0 };
    if unsafe { LookupPrivilegeValueW(null(), SE_CREATE_SYMBOLIC_LINK_NAME, &mut luid) } == 0 {
        let error = unsafe { GetLastError() };
        let shutdown = wide("SeShutdownPrivilege");
        let known = unsafe { LookupPrivilegeValueW(null(), shutdown.as_ptr(), &mut luid) } != 0;
        wine_gap(!(error == 1313 && known), || "SeCreateSymbolicLinkPrivilege is unknown, SeShutdownPrivilege resolves".into())?;
        return Err(format!("LookupPrivilegeValueW failed with {error}"));
    }
    let privileges = TOKEN_PRIVILEGES {
        PrivilegeCount: 1,
        Privileges: [LUID_AND_ATTRIBUTES { Luid: luid, Attributes: SE_PRIVILEGE_ENABLED }],
    };
    let adjusted = unsafe {
        AdjustTokenPrivileges(token.0, FALSE, &privileges, std::mem::size_of::<TOKEN_PRIVILEGES>() as u32, null_mut(), null_mut())
    };
    ensure(adjusted != 0, || last("AdjustTokenPrivileges"))?;
    let status = unsafe { GetLastError() };
    let held = if status == ERROR_NOT_ALL_ASSIGNED_CODE { "not held" } else { "enabled" };
    let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
    let mut size = 0_u32;
    let sddl = wide("D:(A;;GA;;;WD)");
    let converted = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), SDDL_REVISION_1, &mut descriptor, &mut size)
    };
    ensure(converted != 0 && size > 0, || last("ConvertStringSecurityDescriptorToSecurityDescriptorW"))?;
    unsafe { LocalFree(descriptor.cast()) };
    Ok(format!("symbolic link privilege LUID {}:{} {held}, SDDL descriptor {size} B", luid.HighPart, luid.LowPart))
}

fn first_root() -> Result<Vec<u8>, String> {
    let roots = nanocode_platform::trust::mozilla_roots();
    roots.first().map(|root| root.as_ref().to_vec()).ok_or_else(|| "no bundled roots".into())
}

fn certificate_store_raw() -> Outcome {
    let root = wide("ROOT");
    let store = unsafe { CertOpenSystemStoreW(0, root.as_ptr()) };
    ensure(!store.is_null(), || last("CertOpenSystemStoreW"))?;
    let mut count = 0;
    let mut context: *mut CERT_CONTEXT = null_mut();
    loop {
        context = unsafe { CertEnumCertificatesInStore(store, context) };
        if context.is_null() {
            break;
        }
        count += 1;
    }
    unsafe { CertCloseStore(store, 0) };
    let memory = unsafe { CertOpenStore(CERT_STORE_PROV_MEMORY, 0, 0, 0, null()) };
    ensure(!memory.is_null(), || last("CertOpenStore memory"))?;
    let der = first_root()?;
    let mut added: *mut CERT_CONTEXT = null_mut();
    let ok = unsafe {
        CertAddEncodedCertificateToStore(memory, X509_ASN_ENCODING | PKCS_7_ASN_ENCODING, der.as_ptr(), der.len() as u32, CERT_STORE_ADD_ALWAYS_CODE, &mut added)
    };
    ensure(ok != 0 && !added.is_null(), || last("CertAddEncodedCertificateToStore"))?;
    let context = unsafe { &*added };
    ensure(context.cbCertEncoded as usize == der.len(), || format!("encoded {} vs {}", context.cbCertEncoded, der.len()))?;
    let friendly_name = wide("probe");
    let friendly = CRYPT_INTEGER_BLOB { cbData: 12, pbData: friendly_name.as_ptr() as *mut u8 };
    let set = unsafe { CertSetCertificateContextProperty(added, CERT_FRIENDLY_NAME_PROP_ID_CODE, 0, (&raw const friendly).cast()) };
    ensure(set != 0, || last("CertSetCertificateContextProperty"))?;
    let validity = unsafe { CertVerifyTimeValidity(null(), context.pCertInfo) };
    let crl = unsafe { CertEnumCRLsInStore(memory, null()) };
    ensure(crl.is_null(), || "memory store has a CRL".into())?;
    unsafe { CertFreeCertificateContext(added) };
    ensure(unsafe { CertCloseStore(memory, 0) } != 0, || last("CertCloseStore"))?;
    Ok(format!("ROOT store has {count} certificates, memory store took a {} B root, time validity {validity}", der.len()))
}

fn certificate_chain() -> Outcome {
    let der = first_root()?;
    let memory = unsafe { CertOpenStore(CERT_STORE_PROV_MEMORY, 0, 0, 0, null()) };
    ensure(!memory.is_null(), || last("CertOpenStore memory"))?;
    let mut added: *mut CERT_CONTEXT = null_mut();
    let ok = unsafe {
        CertAddEncodedCertificateToStore(memory, X509_ASN_ENCODING, der.as_ptr(), der.len() as u32, CERT_STORE_ADD_ALWAYS_CODE, &mut added)
    };
    ensure(ok != 0, || last("CertAddEncodedCertificateToStore"))?;
    let mut config: CERT_CHAIN_ENGINE_CONFIG = unsafe { std::mem::zeroed() };
    config.cbSize = std::mem::size_of::<CERT_CHAIN_ENGINE_CONFIG>() as u32;
    config.hExclusiveRoot = memory;
    let mut engine: HCERTCHAINENGINE = null_mut();
    let engine_ok = unsafe { CertCreateCertificateChainEngine(&config, &mut engine) };
    let engine_error = unsafe { GetLastError() };
    let mut parameters: CERT_CHAIN_PARA = unsafe { std::mem::zeroed() };
    parameters.cbSize = std::mem::size_of::<CERT_CHAIN_PARA>() as u32;
    let mut chain: *mut CERT_CHAIN_CONTEXT = null_mut();
    let built = unsafe {
        CertGetCertificateChain(if engine_ok != 0 { engine } else { null_mut() }, added, null(), memory, &parameters, 0, null(), &mut chain)
    };
    ensure(built != 0 && !chain.is_null(), || last("CertGetCertificateChain"))?;
    let status = unsafe { (*chain).TrustStatus };
    let mut policy: CERT_CHAIN_POLICY_PARA = unsafe { std::mem::zeroed() };
    policy.cbSize = std::mem::size_of::<CERT_CHAIN_POLICY_PARA>() as u32;
    let mut policy_status: CERT_CHAIN_POLICY_STATUS = unsafe { std::mem::zeroed() };
    policy_status.cbSize = std::mem::size_of::<CERT_CHAIN_POLICY_STATUS>() as u32;
    let verified = unsafe { CertVerifyCertificateChainPolicy(CERT_CHAIN_POLICY_SSL, chain, &policy, &mut policy_status) };
    ensure(verified != 0, || last("CertVerifyCertificateChainPolicy"))?;
    unsafe { CertFreeCertificateChain(chain) };
    if engine_ok != 0 {
        unsafe { CertFreeCertificateChainEngine(engine) };
    }
    unsafe { CertFreeCertificateContext(added) };
    unsafe { CertCloseStore(memory, 0) };
    Ok(format!(
        "chain built, trust error {:#x} info {:#x}, SSL policy error {:#x}, exclusive engine {}",
        status.dwErrorStatus,
        status.dwInfoStatus,
        policy_status.dwError,
        if engine_ok != 0 { "created".to_string() } else { format!("failed {engine_error}") }
    ))
}

fn certificate_store() -> Outcome {
    use nanocode_sys::certificate_store::{Location, Usages, current_user_roots, system_certificates};
    let user = current_user_roots().map_err(|error| format!("current user: {error}"))?;
    let machine = system_certificates(Location::LocalMachine, "ROOT").map_err(|error| format!("local machine: {error}"))?;
    let all = user.iter().filter(|certificate| matches!(certificate.usages, Ok(Usages::All))).count();
    let failed = user.iter().filter(|certificate| certificate.usages.is_err()).count();
    let valid = user.iter().filter(|certificate| certificate.time_valid).count();
    ensure(failed == 0, || format!("{failed} usages failed: {:?}", user.iter().find_map(|certificate| certificate.usages.as_ref().err())))?;
    ensure(user.iter().all(|certificate| certificate.der.first() == Some(&0x30)), || "a certificate is not DER".into())?;
    Ok(format!("current user {} ({all} for all usages, {valid} time valid), local machine {}", user.len(), machine.len()))
}

fn native_certificates() -> Outcome {
    let loaded = nanocode_platform::trust::load_native_certificates();
    let errors: Vec<String> = loaded.errors.iter().map(ToString::to_string).collect();
    ensure(errors.is_empty(), || format!("errors {errors:?}"))?;
    Ok(format!("{} usable roots", loaded.certificates.len()))
}

fn socket_address(address: std::net::SocketAddrV4) -> SOCKADDR_IN {
    let mut raw: SOCKADDR_IN = unsafe { std::mem::zeroed() };
    raw.sin_family = AF_INET;
    raw.sin_port = address.port().to_be();
    raw.sin_addr.S_un.S_addr = u32::from_ne_bytes(address.ip().octets());
    raw
}

fn start_winsock() -> Result<WSADATA, String> {
    let mut data: WSADATA = unsafe { std::mem::zeroed() };
    let started = unsafe { WSAStartup(0x0202, &mut data) };
    ensure(started == 0, || format!("WSAStartup {started}"))?;
    Ok(data)
}

fn winsock_tcp() -> Outcome {
    let data = start_winsock()?;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").map_err(|error| error.to_string())?;
    let std::net::SocketAddr::V4(address) = listener.local_addr().map_err(|error| error.to_string())? else {
        return Err("not IPv4".into());
    };
    let text = wide(&address.to_string());
    let mut parsed: SOCKADDR_IN = unsafe { std::mem::zeroed() };
    let mut parsed_length = std::mem::size_of::<SOCKADDR_IN>() as i32;
    let converted = unsafe { WSAStringToAddressW(text.as_ptr(), i32::from(AF_INET), null(), (&raw mut parsed).cast(), &mut parsed_length) };
    ensure(converted == 0 && u16::from_be(parsed.sin_port) == address.port(), || format!("WSAStringToAddressW {converted} error {}", unsafe { WSAGetLastError() }))?;
    let socket = unsafe { socket(i32::from(AF_INET), SOCK_STREAM, IPPROTO_TCP) };
    ensure(socket != INVALID_SOCKET, || format!("socket error {}", unsafe { WSAGetLastError() }))?;
    let mut kind = 0_i32;
    let mut kind_length = 4_i32;
    let got = unsafe { getsockopt(socket, SOL_SOCKET_CODE, SO_TYPE_CODE, (&raw mut kind).cast(), &mut kind_length) };
    ensure(got == 0 && kind == SOCK_STREAM, || format!("getsockopt {got} kind {kind}"))?;
    let raw = socket_address(address);
    let connected = unsafe { WSAConnect(socket, (&raw const raw).cast(), std::mem::size_of::<SOCKADDR_IN>() as i32, null(), null_mut(), null(), null()) };
    ensure(connected == 0, || format!("WSAConnect error {}", unsafe { WSAGetLastError() }))?;
    let (mut peer, _) = listener.accept().map_err(|error| error.to_string())?;
    let sent = unsafe { send(socket, b"plain".as_ptr(), 5, 0) };
    ensure(sent == 5, || format!("send {sent} error {}", unsafe { WSAGetLastError() }))?;
    let mut message = *b"vectored";
    let buffer = WSABUF { len: message.len() as u32, buf: message.as_mut_ptr() };
    let mut sent_bytes = 0_u32;
    let vectored = unsafe { WSASend(socket, &buffer, 1, &mut sent_bytes, 0, null_mut(), None) };
    ensure(vectored == 0 && sent_bytes == 8, || format!("WSASend error {}", unsafe { WSAGetLastError() }))?;
    use std::io::{Read, Write};
    let mut received = [0_u8; 13];
    peer.read_exact(&mut received).map_err(|error| error.to_string())?;
    ensure(&received == b"plainvectored", || format!("peer read {received:?}"))?;
    peer.write_all(b"answer").map_err(|error| error.to_string())?;
    let mut answer = [0_u8; 6];
    let receive = WSABUF { len: 6, buf: answer.as_mut_ptr() };
    let mut received_bytes = 0_u32;
    let mut flags = 0_u32;
    let got = unsafe { WSARecv(socket, &receive, 1, &mut received_bytes, &mut flags, null_mut(), None) };
    ensure(got == 0 && &answer[..received_bytes as usize] == &b"answer"[..received_bytes as usize] && received_bytes > 0, || {
        format!("WSARecv {got} bytes {received_bytes} error {}", unsafe { WSAGetLastError() })
    })?;
    let mut nonblocking = 1_u32;
    let mut returned = 0_u32;
    let ioctl = unsafe {
        WSAIoctl(socket, FIONBIO_CODE, (&raw const nonblocking).cast(), 4, null_mut(), 0, &mut returned, null_mut(), None)
    };
    nonblocking = 0;
    let _ = nonblocking;
    let mut error_value = 0_i32;
    let mut error_length = 4_i32;
    unsafe { getsockopt(socket, SOL_SOCKET_CODE, SO_ERROR, (&raw mut error_value).cast(), &mut error_length) };
    ensure(unsafe { closesocket(socket) } == 0, || "closesocket".into())?;
    Ok(format!(
        "Winsock {}.{}, parsed {address}, send+WSASend 13 B, WSARecv {received_bytes} B, FIONBIO ioctl {ioctl}, SO_ERROR {error_value}",
        data.wVersion & 0xff,
        data.wVersion >> 8
    ))
}

fn winsock_udp() -> Outcome {
    start_winsock()?;
    let make = || -> Result<(SOCKET, std::net::SocketAddrV4), String> {
        let socket = unsafe { socket(i32::from(AF_INET), SOCK_DGRAM_CODE, IPPROTO_UDP_CODE) };
        ensure(socket != INVALID_SOCKET, || format!("socket error {}", unsafe { WSAGetLastError() }))?;
        let local = socket_address("127.0.0.1:0".parse().expect("a literal address"));
        let bound = unsafe { bind(socket, (&raw const local).cast(), std::mem::size_of::<SOCKADDR_IN>() as i32) };
        ensure(bound == 0, || format!("bind error {}", unsafe { WSAGetLastError() }))?;
        let mut name: SOCKADDR_IN = unsafe { std::mem::zeroed() };
        let mut length = std::mem::size_of::<SOCKADDR_IN>() as i32;
        let got = unsafe { libc_getsockname(socket, (&raw mut name).cast(), &mut length) };
        ensure(got == 0, || format!("getsockname error {}", unsafe { WSAGetLastError() }))?;
        let ip = std::net::Ipv4Addr::from(unsafe { name.sin_addr.S_un.S_addr }.to_ne_bytes());
        Ok((socket, std::net::SocketAddrV4::new(ip, u16::from_be(name.sin_port))))
    };
    let (first, _first_address) = make()?;
    let (second, second_address) = make()?;
    let target = socket_address(second_address);
    let mut payload = *b"datagram";
    let buffer = WSABUF { len: 8, buf: payload.as_mut_ptr() };
    let mut sent = 0_u32;
    let sent_ok = unsafe {
        WSASendTo(first, &buffer, 1, &mut sent, 0, (&raw const target).cast(), std::mem::size_of::<SOCKADDR_IN>() as i32, null_mut(), None)
    };
    ensure(sent_ok == 0 && sent == 8, || format!("WSASendTo error {}", unsafe { WSAGetLastError() }))?;
    let mut received = [0_u8; 16];
    let receive = WSABUF { len: 16, buf: received.as_mut_ptr() };
    let mut got = 0_u32;
    let mut flags = 0_u32;
    let mut from: SOCKADDR_IN = unsafe { std::mem::zeroed() };
    let mut from_length = std::mem::size_of::<SOCKADDR_IN>() as i32;
    let received_ok = unsafe {
        WSARecvFrom(second, &receive, 1, &mut got, &mut flags, (&raw mut from).cast(), &mut from_length, null_mut(), None)
    };
    ensure(received_ok == 0 && &received[..got as usize] == b"datagram", || format!("WSARecvFrom error {}", unsafe { WSAGetLastError() }))?;
    unsafe { closesocket(first) };
    unsafe { closesocket(second) };
    Ok(format!("8 B datagram from port {} to {second_address}", u16::from_be(from.sin_port)))
}

#[link(name = "ws2_32.dll", kind = "raw-dylib", modifiers = "+verbatim")]
unsafe extern "system" {
    #[link_name = "getsockname"]
    fn libc_getsockname(socket: SOCKET, name: *mut SOCKADDR, length: *mut i32) -> i32;
}

fn ole() -> Outcome {
    let source = wide("binary string");
    let text = unsafe { SysAllocStringLen(source.as_ptr(), 13) };
    ensure(!text.is_null(), || "SysAllocStringLen returned null".into())?;
    let length = unsafe { SysStringLen(text) };
    ensure(length == 13, || format!("SysStringLen {length}"))?;
    unsafe { SysFreeString(text) };
    unsafe { CoTaskMemFree(null()) };
    let mut info = null_mut();
    let error_info = unsafe { GetErrorInfo(0, &mut info) };
    ensure(error_info == S_OK || error_info == S_FALSE_CODE, || format!("GetErrorInfo {error_info:#x}"))?;
    let mut uuid: GUID = unsafe { std::mem::zeroed() };
    let status = unsafe { UuidCreateSequential(&mut uuid) };
    ensure(status == 0 || status == 1824, || format!("UuidCreateSequential {status}"))?;
    let clsid_shell = GUID { data1: 0x1361_7e83, data2: 0x1d29, data3: 0x11d0, data4: [0x8c, 0x14, 0, 0xa0, 0xc9, 0x11, 0x3c, 0x2d] };
    let mut prog: PWSTR = null_mut();
    let prog_result = unsafe { ProgIDFromCLSID(&clsid_shell, &mut prog) };
    if prog_result == 0 && !prog.is_null() {
        unsafe { CoTaskMemFree(prog.cast()) };
    }
    Ok(format!(
        "BSTR of 13 units, GetErrorInfo {error_info:#x}, uuid {:08x}-{:04x}, ProgIDFromCLSID {prog_result:#x}",
        uuid.data1, uuid.data2
    ))
}

fn shell_paths() -> Outcome {
    let path = wide("C:\\Windows\\System32");
    let mut end: PCWSTR = null();
    let result = unsafe { PathCchSkipRoot(path.as_ptr(), &mut end) };
    ensure(result == 0 && !end.is_null(), || format!("PathCchSkipRoot {result:#x}"))?;
    let offset = (end as usize - path.as_ptr() as usize) / 2;
    ensure(offset == 3, || format!("root ends at {offset}"))?;
    let unused = ACCESS_RIGHTS_ALL;
    let _ = unused;
    let _ = (ERROR_INSUFFICIENT_BUFFER_CODE, ERROR_MORE_DATA_CODE, ERROR_INVALID_HANDLE_CODE);
    Ok("PathCchSkipRoot skips C:\\".into())
}

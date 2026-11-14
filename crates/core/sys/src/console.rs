use crate::{
    AtomicStatic,
    windows_sys::{
        Win32::{
            Foundation::{FALSE, TRUE, WAIT_FAILED, WAIT_OBJECT_0},
            System::{
                Console::{
                    CONSOLE_MODE, CONSOLE_SCREEN_BUFFER_INFO, CTRL_C_EVENT, GetConsoleMode,
                    GetConsoleScreenBufferInfo, GetNumberOfConsoleInputEvents, INPUT_RECORD,
                    KEY_EVENT, ReadConsoleInputW, SetConsoleCtrlHandler, SetConsoleMode,
                    WINDOW_BUFFER_SIZE_EVENT,
                },
                Threading::{CreateEventW, INFINITE, SetEvent, WaitForMultipleObjects},
            },
        },
        core::BOOL,
    },
};
use std::{
    io,
    os::windows::io::{AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle},
    ptr,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

static INTERRUPTS: AtomicU64 = AtomicU64::new(0);
static INSTALLED: AtomicBool = AtomicBool::new(false);
static NOTIFY: AtomicStatic<fn()> = AtomicStatic::empty();

unsafe extern "system" fn handle(kind: u32) -> BOOL {
    if kind != CTRL_C_EVENT {
        return FALSE;
    }
    INTERRUPTS.fetch_add(1, Ordering::SeqCst);
    if let Some(notify) = NOTIFY.load() {
        notify();
    }
    TRUE
}

pub fn listen_interrupts(notify: &'static fn()) -> io::Result<()> {
    NOTIFY.store(Some(notify));
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return Ok(());
    }
    if unsafe { SetConsoleCtrlHandler(Some(handle), TRUE) } == FALSE {
        INSTALLED.store(false, Ordering::SeqCst);
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[must_use]
pub fn interrupts() -> u64 {
    INTERRUPTS.load(Ordering::SeqCst)
}

pub struct Wakeup(OwnedHandle);

impl Wakeup {
    pub fn new() -> io::Result<Self> {
        let event = unsafe { CreateEventW(ptr::null(), FALSE, FALSE, ptr::null()) };
        if event.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(unsafe { OwnedHandle::from_raw_handle(event) }))
    }

    pub fn wake(&self) -> io::Result<()> {
        checked(unsafe { SetEvent(self.0.as_raw_handle()) })
    }
}

pub enum ConsoleInput {
    Unit(u16),
    Resize,
    Other,
}

impl From<&INPUT_RECORD> for ConsoleInput {
    fn from(record: &INPUT_RECORD) -> Self {
        match u32::from(record.EventType) {
            KEY_EVENT => {
                let key = unsafe { record.Event.KeyEvent };
                let unit = unsafe { key.uChar.UnicodeChar };
                if key.bKeyDown == FALSE || unit == 0 {
                    Self::Other
                } else {
                    Self::Unit(unit)
                }
            }
            WINDOW_BUFFER_SIZE_EVENT => Self::Resize,
            _ => Self::Other,
        }
    }
}

fn checked(result: BOOL) -> io::Result<()> {
    if result == FALSE {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn console_mode(console: BorrowedHandle<'_>) -> io::Result<CONSOLE_MODE> {
    let mut mode = 0;
    checked(unsafe { GetConsoleMode(console.as_raw_handle(), &raw mut mode) })?;
    Ok(mode)
}

pub fn set_console_mode(console: BorrowedHandle<'_>, mode: CONSOLE_MODE) -> io::Result<()> {
    checked(unsafe { SetConsoleMode(console.as_raw_handle(), mode) })
}

pub fn window_size(output: BorrowedHandle<'_>) -> io::Result<(u16, u16)> {
    let mut info = CONSOLE_SCREEN_BUFFER_INFO::default();
    checked(unsafe { GetConsoleScreenBufferInfo(output.as_raw_handle(), &raw mut info) })?;
    let window = info.srWindow;
    let span = |low: i16, high: i16| {
        u16::try_from(i32::from(high) - i32::from(low) + 1)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "console window has no size"))
    };
    Ok((
        span(window.Left, window.Right)?,
        span(window.Top, window.Bottom)?,
    ))
}

pub fn wait_input(input: BorrowedHandle<'_>, wakeup: &Wakeup) -> io::Result<bool> {
    let handles = [input.as_raw_handle(), wakeup.0.as_raw_handle()];
    let count = u32::try_from(handles.len()).expect("two handles fit in u32");
    match unsafe { WaitForMultipleObjects(count, handles.as_ptr(), FALSE, INFINITE) } {
        WAIT_OBJECT_0 => Ok(true),
        signaled if signaled == WAIT_OBJECT_0 + 1 => Ok(false),
        WAIT_FAILED => Err(io::Error::last_os_error()),
        other => Err(io::Error::other(format!("console wait returned {other}"))),
    }
}

pub fn read_input<'a>(
    input: BorrowedHandle<'_>,
    records: &'a mut [INPUT_RECORD],
) -> io::Result<&'a [INPUT_RECORD]> {
    let capacity = u32::try_from(records.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "too many input records"))?;
    let mut read = 0;
    checked(unsafe {
        ReadConsoleInputW(
            input.as_raw_handle(),
            records.as_mut_ptr(),
            capacity,
            &raw mut read,
        )
    })?;
    let read = usize::try_from(read).expect("a u32 count fits in usize");
    Ok(&records[..read])
}

pub fn pending_input(input: BorrowedHandle<'_>) -> io::Result<u32> {
    let mut count = 0;
    checked(unsafe { GetNumberOfConsoleInputEvents(input.as_raw_handle(), &raw mut count) })?;
    Ok(count)
}

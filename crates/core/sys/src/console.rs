use crate::{
    AtomicStatic,
    windows_sys::{
        Win32::{
            Foundation::{FALSE, TRUE},
            System::Console::{CTRL_C_EVENT, SetConsoleCtrlHandler},
        },
        core::BOOL,
    },
};
use std::{
    io,
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

use std::{
    fs::File,
    io,
    os::windows::io::{AsHandle, BorrowedHandle},
    sync::LazyLock,
};

use nanocode_sync::Slot;
use nanocode_sys::{
    console::{console_mode, set_console_mode, window_size},
    windows_sys::Win32::System::Console::{
        CONSOLE_MODE, DISABLE_NEWLINE_AUTO_RETURN, ENABLE_ECHO_INPUT, ENABLE_EXTENDED_FLAGS,
        ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT, ENABLE_PROCESSED_OUTPUT, ENABLE_QUICK_EDIT_MODE,
        ENABLE_VIRTUAL_TERMINAL_INPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING, ENABLE_WINDOW_INPUT,
    },
};

const INPUT_COOKED: CONSOLE_MODE =
    ENABLE_ECHO_INPUT | ENABLE_LINE_INPUT | ENABLE_PROCESSED_INPUT | ENABLE_QUICK_EDIT_MODE;
const INPUT_RAW: CONSOLE_MODE =
    ENABLE_VIRTUAL_TERMINAL_INPUT | ENABLE_WINDOW_INPUT | ENABLE_EXTENDED_FLAGS;
const OUTPUT_RAW: CONSOLE_MODE =
    ENABLE_VIRTUAL_TERMINAL_PROCESSING | ENABLE_PROCESSED_OUTPUT | DISABLE_NEWLINE_AUTO_RETURN;

#[derive(Clone, Copy)]
struct Modes {
    input: CONSOLE_MODE,
    output: CONSOLE_MODE,
}

static ORIGINAL_MODE: LazyLock<Slot<Modes>> = LazyLock::new(Slot::default);

pub struct Tty(File);

impl AsHandle for Tty {
    fn as_handle(&self) -> BorrowedHandle<'_> {
        self.0.as_handle()
    }
}

fn console(name: &str) -> io::Result<File> {
    File::options().read(true).write(true).open(name)
}

pub fn tty() -> io::Result<Tty> {
    console("CONIN$").map(Tty)
}

#[must_use]
pub fn raw_mode_enabled() -> bool {
    !ORIGINAL_MODE.is_empty()
}

pub fn enable_raw_mode() -> io::Result<()> {
    if raw_mode_enabled() {
        return Ok(());
    }
    let input = tty()?;
    let output = console("CONOUT$")?;
    let original = Modes {
        input: console_mode(input.as_handle())?,
        output: console_mode(output.as_handle())?,
    };
    set_console_mode(
        input.as_handle(),
        (original.input & !INPUT_COOKED) | INPUT_RAW,
    )?;
    if let Err(error) = set_console_mode(output.as_handle(), original.output | OUTPUT_RAW) {
        drop(set_console_mode(input.as_handle(), original.input));
        return Err(error);
    }
    assert!(
        ORIGINAL_MODE.put(original).is_none(),
        "raw mode is entered once before it is left"
    );
    Ok(())
}

pub fn disable_raw_mode() -> io::Result<()> {
    let Some(original) = ORIGINAL_MODE.take() else {
        return Ok(());
    };
    let restored = tty()
        .and_then(|input| set_console_mode(input.as_handle(), original.input))
        .and_then(|()| console("CONOUT$"))
        .and_then(|output| set_console_mode(output.as_handle(), original.output));
    if restored.is_err() {
        ORIGINAL_MODE.put(original);
    }
    restored
}

pub fn size() -> io::Result<(u16, u16)> {
    match console("CONOUT$") {
        Ok(output) => window_size(output.as_handle()),
        Err(_) => window_size(io::stdout().as_handle()),
    }
}

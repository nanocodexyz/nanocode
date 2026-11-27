#![deny(unsafe_op_in_unsafe_fn)]

pub mod adler32;
pub mod alloc;
#[cfg(all(target_vendor = "apple", not(target_os = "macos")))]
pub mod apple;
mod atomic;
pub mod base64;
mod bilock;
pub mod blake3;
#[cfg(windows)]
pub mod certificate_store;
#[cfg(windows)]
pub mod console;
#[cfg(target_os = "macos")]
pub mod core_foundation;
pub mod crc32;
pub mod environment;
mod decimal;
#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub mod entry;
mod exclusive;
mod grow;
mod inline;
pub mod jpeg;
#[cfg(windows)]
pub mod junction;
pub mod libc;
pub mod lz4;
#[cfg(all(target_os = "linux", target_env = "gnu"))]
mod math;
pub mod memchr;
mod pin;
pub mod queue;
#[cfg(any(unix, windows))]
mod random;
#[cfg(all(target_vendor = "apple", not(target_os = "macos")))]
pub mod security;
pub mod sha1;
pub mod sha256;
pub mod sha512;
mod stack;
mod swap;
pub mod teddy;
#[cfg(target_os = "macos")]
pub mod trust_settings;
#[cfg(unix)]
pub mod unix;
pub mod utf8;
mod waker;
pub mod webp;
#[cfg(windows)]
#[allow(
    non_snake_case,
    non_upper_case_globals,
    non_camel_case_types,
    dead_code,
    clippy::all,
    clippy::missing_const_for_fn,
    clippy::use_self
)]
pub mod windows_sys;
mod zero;

pub use atomic::{AtomicArc, AtomicBox, AtomicPinBox, AtomicStatic};
pub use bilock::{BiLock, BiLockGuard};
pub use decimal::DecimalText;
pub use exclusive::Exclusive;
pub use grow::Grow;
pub use inline::{InlineVec, OwnedItems, new_arc_with};
pub use pin::{Scope, Scoped};
#[cfg(any(unix, windows))]
pub use random::fill_random;
pub use stack::current_thread_stack_remaining_bytes;
pub use swap::{AsRaw, Enclosed, Guard, Pointer, Swap, SwapOption, Swapped};
pub use waker::AtomicWaker;
pub use zero::{zero, zero_vec};

#[macro_export]
macro_rules! main {
    ($run:path) => {
        $crate::main!($run, take = []);
    };
    ($run:path, take = [$($name:expr),* $(,)?]) => {
        #[cfg(all(target_os = "linux", target_env = "gnu"))]
        #[unsafe(no_mangle)]
        extern "C" fn main(
            count: $crate::libc::c_int,
            arguments: *const *const $crate::libc::c_char,
        ) -> $crate::libc::c_int {
            $crate::entry::prepare(count, arguments);
            $crate::environment::take_before_threads(&[$($name),*]);
            $crate::entry::exit($run())
        }

        #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
        fn main() -> ::std::process::ExitCode {
            $crate::environment::take_before_threads(&[$($name),*]);
            $run()
        }
    };
}

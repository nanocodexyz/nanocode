#[cfg(not(target_family = "wasm"))]
mod analyze;
#[cfg(not(target_family = "wasm"))]
mod check;
#[cfg(not(target_family = "wasm"))]
mod cli;
#[cfg(not(target_family = "wasm"))]
mod downstream;

#[cfg(not(target_family = "wasm"))]
fn main() -> std::process::ExitCode {
    cli::main()
}

#[cfg(target_family = "wasm")]
fn main() {}

//! Binary entry point. Parses argv, then dispatches through the real runner +
//! the interactive Ratatui wizard. The library does all the work; `main` is a
//! thin shell so the whole surface is testable without spawning a process.

use ma_demo::cli::{self, TuiWizard};
use ma_demo::runner::RealRunner;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let runner = RealRunner;
    let wizard = TuiWizard;
    let code = cli::dispatch(&args, &runner, &wizard);
    std::process::exit(code);
}

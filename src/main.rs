//! MinWin's executable entry point.
//!
//! All this does is hand control to the CLI layer and translate a MinWin error
//! into a clean message plus an exit code. Nothing else lives here.

fn main() -> std::process::ExitCode {
    minwin::cli::main()
}

//! mett-gui: mett in a window of its own, with the same screens as in the
//! terminal.

// No console window on Windows.
#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() -> std::process::ExitCode {
    mett_tui::gui_main()
}

#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
mod process;
#[cfg(windows)]
mod profiles;
#[cfg(windows)]
mod runtime;
#[cfg(windows)]
mod startup;
#[cfg(windows)]
mod telegram;
#[cfg(windows)]
mod ui;

#[cfg(windows)]
fn wide(text: impl AsRef<std::ffi::OsStr>) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    text.as_ref().encode_wide().chain([0]).collect()
}

#[cfg(windows)]
fn main() {
    ui::run();
}

#[cfg(not(windows))]
fn main() {
    eprintln!("Zapret UI requires Windows.");
}

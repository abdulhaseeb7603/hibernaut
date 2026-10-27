//! The hibernaut tray icon: every session at a glance, with actions on them. A GUI program, so
//! Windows opens no console window for it.
#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() {
    hibernaut::tray::main()
}

// Release builds are GUI apps with no console window; debug builds keep one so
// panics and warnings stay visible while developing.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod diag;
mod sensors;
mod ui;

fn main() {
    // The overlay re-launches itself as the NVML helper; see sensors::gpu.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some(sensors::gpu::HELPER_ARG) {
        sensors::gpu::helper_main(&args[1..]);
    }

    if let Err(e) = ui::window::run() {
        // With no console attached in release, a message box is the only way
        // this ever reaches the user.
        let msg = format!("Vitals could not start:\n\n{e}");
        let text: Vec<u16> = msg.encode_utf16().chain(std::iter::once(0)).collect();
        let title: Vec<u16> = "Vitals".encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            windows::Win32::UI::WindowsAndMessaging::MessageBoxW(
                None,
                windows::core::PCWSTR(text.as_ptr()),
                windows::core::PCWSTR(title.as_ptr()),
                windows::Win32::UI::WindowsAndMessaging::MB_ICONERROR,
            );
        }
        std::process::exit(1);
    }
}

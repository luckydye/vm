mod agent;
mod audio;
mod cli;
mod clipboard;
mod config;
mod control;
mod devices;
mod display;
mod fuse;
mod help;
mod hypervisor;
mod instance;
mod keymap;
mod machine;
mod memory;
mod net;
mod node;
mod protocol;
mod registry;
mod screen;
mod seed;
mod serve;
mod ssh_config;
mod stdio;
mod store;
mod suspend;
mod telemetry;
mod ui;
mod uart;
mod virtio;
mod vmm;

fn main() {
    #[cfg(target_os = "linux")]
    if std::env::args().nth(1).as_deref() == Some(clipboard::SERVE_TEXT_COMMAND) {
        return clipboard::serve_text();
    }
    cli::main();
}

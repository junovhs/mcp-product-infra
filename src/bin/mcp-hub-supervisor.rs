#![cfg_attr(windows, windows_subsystem = "windows")]

use std::path::{Path, PathBuf};

fn main() {
    let mut arguments = std::env::args_os().skip(1);
    let Some(config_path) = arguments.next().map(PathBuf::from) else {
        std::process::exit(2);
    };
    if arguments.next().is_some() {
        std::process::exit(2);
    }

    let _ = std::fs::remove_file(config_path.with_extension("error.log"));
    match mcp_product_infra::service_host::run_windows_service_host(&config_path) {
        Ok(code) => std::process::exit(code as i32),
        Err(error) => {
            let _ = write_error(&config_path, &error);
            std::process::exit(1);
        }
    }
}

fn write_error(config_path: &Path, error: &str) -> std::io::Result<()> {
    std::fs::write(config_path.with_extension("error.log"), error)
}

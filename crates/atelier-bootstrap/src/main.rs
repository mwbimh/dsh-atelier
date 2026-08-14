#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use std::process::ExitCode;

fn main() -> ExitCode {
    match atelier_bootstrap::run_from_environment() {
        Ok(0) => ExitCode::SUCCESS,
        Ok(_) => ExitCode::FAILURE,
        Err(error) => {
            eprintln!("dsh-atelier bootstrap: {error}");
            ExitCode::FAILURE
        }
    }
}

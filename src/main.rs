use std::process::ExitCode;

fn main() -> ExitCode {
    aisession_backup::tui::install_panic_hook();
    if let Err(err) = aisession_backup::cli::run() {
        let code = aisession_backup::cli::exit_code(&err);
        let msg = format!("{err:#}");
        if !msg.is_empty() {
            eprintln!("error: {msg}");
        }
        return ExitCode::from(code);
    }
    ExitCode::from(aisession_backup::cli::EXIT_OK)
}

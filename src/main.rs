use std::process::ExitCode;

fn main() -> ExitCode {
    claude_migrate::tui::install_panic_hook();
    if let Err(err) = claude_migrate::cli::run() {
        let code = claude_migrate::cli::exit_code(&err);
        let msg = format!("{err:#}");
        if !msg.is_empty() {
            eprintln!("error: {msg}");
        }
        return ExitCode::from(code);
    }
    ExitCode::from(claude_migrate::cli::EXIT_OK)
}

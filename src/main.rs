use std::process::ExitCode;

fn main() -> ExitCode {
    if let Err(err) = claude_migrate::cli::run() {
        eprintln!("error: {err:#}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

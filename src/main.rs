use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    match smtpmaster::cli::run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("smtpmaster: {error}");
            ExitCode::FAILURE
        }
    }
}

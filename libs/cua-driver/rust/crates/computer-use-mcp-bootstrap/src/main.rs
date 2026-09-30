use std::process::ExitCode;

use computer_use_mcp_bootstrap::{run, CommandLine};

fn main() -> ExitCode {
    match CommandLine::parse(std::env::args_os().skip(1)) {
        Ok(CommandLine::Help) => {
            println!("{}", computer_use_mcp_bootstrap::USAGE);
            ExitCode::SUCCESS
        }
        Ok(CommandLine::Version) => {
            println!("computer-use-mcp-bootstrap {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Ok(CommandLine::BuildAttestation) => {
            println!("{}", computer_use_mcp_bootstrap::build_attestation());
            ExitCode::SUCCESS
        }
        Ok(CommandLine::Run(config)) => match run(config) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("computer-use-mcp-bootstrap: {error}");
                ExitCode::FAILURE
            }
        },
        Err(error) => {
            eprintln!(
                "computer-use-mcp-bootstrap: {error}\n{}",
                computer_use_mcp_bootstrap::USAGE
            );
            ExitCode::from(64)
        }
    }
}

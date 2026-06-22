use clap::Parser;
use simulacra_cli::{CliArgs, CliOutput, run};

fn main() {
    let args = CliArgs::parse();
    let output = match run(args) {
        Ok(output) => output,
        Err(e) => CliOutput {
            stdout_content: String::new(),
            stderr_content: format!("{e:#}"),
            exit_code: 1,
            telemetry_flushed: false,
        },
    };

    if !output.stderr_content.is_empty() {
        eprintln!("{}", output.stderr_content);
    }
    if !output.stdout_content.is_empty() {
        print!("{}", output.stdout_content);
    }

    std::process::exit(output.exit_code);
}

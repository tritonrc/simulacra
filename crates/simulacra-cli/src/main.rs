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
            streamed_to_stdout: false,
        },
    };

    if !output.stderr_content.is_empty() {
        eprintln!("{}", output.stderr_content);
    }
    // S055: in JSONL headless mode, stdout was already streamed line-by-line
    // during `run`; reprinting `stdout_content` would duplicate the stream.
    if !output.streamed_to_stdout && !output.stdout_content.is_empty() {
        print!("{}", output.stdout_content);
    }

    std::process::exit(output.exit_code);
}

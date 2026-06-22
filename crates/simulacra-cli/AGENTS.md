# simulacra-cli

Binary crate providing the `simulacra` CLI entry point.

## Responsibility

Parse command-line arguments via clap, load the project configuration from
`simulacra.toml` (or a user-specified path), and print a summary of the loaded
project.

## Key Flags

- `-c / --config` -- path to config file (default: `simulacra.toml`)
- `-m / --mode` -- run mode (interactive or headless)
- `-t / --task` -- task description for headless execution

## Constraints

- Depends on `simulacra-config` for configuration types.
- Gracefully handles missing config files (prints a message, does not crash).
- Initializes tracing via `tracing-subscriber` before any other work.
- No async runtime required at this stage.

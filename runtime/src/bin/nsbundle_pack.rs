//! `nsbundle_pack` — seals a built webpack output directory (`runtime.js`/`vendor.js`/entry/
//! `package.json`/...) into an encrypted `app.nsbundle` container (see `runtime::source_protect`).
//!
//! Usage:
//!   nsbundle_pack --input <dir> --output <file> [--key-hex <64 hex chars>]
//!
//! Without `--key-hex`, the container is sealed with the compiled-in default pepper
//! (`key_mode = 0`) — no further runtime setup needed. With `--key-hex`, the container is sealed
//! `key_mode = 1` (custom key); the app must call `runtime_set_bundle_key(sameHex)` before
//! `runtime_init`, or the sealed bundle cannot be opened at startup.
//!
//! This is the packer half of the CLI contract documented for the NativeScript CLI's
//! `ns build windows`/`ns deploy windows`: invoke this after webpack finishes, pointing `--input`
//! at its output directory, and stop staging the plaintext directory once `app.nsbundle` is
//! produced (the app project's `.csproj` picks whichever one is present).

use std::path::PathBuf;
use std::process::ExitCode;

use runtime::source_protect;

fn print_usage() {
    eprintln!(
        "Usage: nsbundle_pack --input <dir> --output <file> [--key-hex <64 hex chars>]"
    );
}

fn main() -> ExitCode {
    let mut input: Option<PathBuf> = None;
    let mut output: Option<PathBuf> = None;
    let mut key_hex: Option<String> = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--input" => input = args.next().map(PathBuf::from),
            "--output" => output = args.next().map(PathBuf::from),
            "--key-hex" => key_hex = args.next(),
            "-h" | "--help" => {
                print_usage();
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("Unknown argument: {other}");
                print_usage();
                return ExitCode::FAILURE;
            }
        }
    }

    let (Some(input), Some(output)) = (input, output) else {
        print_usage();
        return ExitCode::FAILURE;
    };

    if !input.is_dir() {
        eprintln!("--input {} is not a directory", input.display());
        return ExitCode::FAILURE;
    }

    let (key_mode, key) = match key_hex {
        Some(hex) => match source_protect::parse_key_hex(&hex) {
            Some(key) => (source_protect::KEY_MODE_CUSTOM, key),
            None => {
                eprintln!("--key-hex must be exactly 64 hex characters (32 bytes)");
                return ExitCode::FAILURE;
            }
        },
        None => (source_protect::KEY_MODE_DEFAULT, source_protect::default_key()),
    };

    match source_protect::pack_directory(&input, &output, key_mode, key) {
        Ok(()) => {
            println!(
                "Sealed {} -> {} ({})",
                input.display(),
                output.display(),
                if key_mode == source_protect::KEY_MODE_CUSTOM {
                    "custom key — call runtime_set_bundle_key() with the same --key-hex before runtime_init"
                } else {
                    "default key"
                }
            );
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("Failed to pack {}: {err}", input.display());
            ExitCode::FAILURE
        }
    }
}

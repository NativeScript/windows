//! `nsbundle_pack` — seals a built webpack output directory (`runtime.js`/`vendor.js`/entry/
//! `package.json`/...) into an encrypted `app.nsbundle` container (see `runtime::source_protect`).
//!
//! Usage:
//!   nsbundle_pack --input <dir> --output <file> [--key-hex <64 hex chars> | --write-key <file>]
//!
//! Without a key option, the container is sealed with the compiled-in default pepper
//! (`key_mode = 0`), which every app shares. With `--key-hex`, the container is sealed
//! `key_mode = 1` (custom key); the app must call `runtime_set_bundle_key(sameHex)` before
//! `runtime_init`, or the sealed bundle cannot be opened at startup. `--write-key` seals with a
//! fresh random key and writes its hex to `<file>`, which the app template compiles into the
//! exe and passes to `runtime_set_bundle_key`.
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
        "Usage: nsbundle_pack --input <dir> --output <file> [--key-hex <64 hex chars> | --write-key <file>]"
    );
}

fn main() -> ExitCode {
    let mut input: Option<PathBuf> = None;
    let mut output: Option<PathBuf> = None;
    let mut key_hex: Option<String> = None;
    let mut write_key: Option<PathBuf> = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--input" => input = args.next().map(PathBuf::from),
            "--output" => output = args.next().map(PathBuf::from),
            "--key-hex" => key_hex = args.next(),
            "--write-key" => write_key = args.next().map(PathBuf::from),
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

    if let Some(path) = write_key {
        if key_hex.is_some() {
            eprintln!("--key-hex and --write-key are mutually exclusive");
            return ExitCode::FAILURE;
        }
        let mut key = [0u8; 32];
        if getrandom::getrandom(&mut key).is_err() {
            eprintln!("The OS random number generator is unavailable");
            return ExitCode::FAILURE;
        }
        let hex: String = key.iter().map(|b| format!("{b:02x}")).collect();
        if let Err(err) = std::fs::write(&path, &hex) {
            eprintln!("Failed to write {}: {err}", path.display());
            return ExitCode::FAILURE;
        }
        key_hex = Some(hex);
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

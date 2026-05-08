use std::{
    env, fs,
    io::{self, Read},
    path::PathBuf,
    process,
};

use serde_json::{json, Value};

use stellaclaw_apply_patch::{apply_patch, ApplyPatchOptions, PatchFormat};

fn main() {
    let exit_code = match run() {
        Ok(result) => {
            println!("{result}");
            if result
                .get("applied")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                0
            } else {
                1
            }
        }
        Err(error) => {
            println!(
                "{}",
                json!({
                    "applied": false,
                    "error_kind": "invalid_arguments",
                    "error": error,
                })
            );
            2
        }
    };
    process::exit(exit_code);
}

fn run() -> Result<Value, String> {
    let mut workspace =
        env::current_dir().map_err(|error| format!("failed to read cwd: {error}"))?;
    let mut format = PatchFormat::Auto;
    let mut check = false;
    let mut strip = 0usize;
    let mut reverse = false;
    let mut max_output_chars = 1000usize;
    let mut patch_file: Option<PathBuf> = None;

    let mut args = env::args().skip(1).peekable();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print_help();
                process::exit(0);
            }
            "--version" => {
                println!("{}", env!("CARGO_PKG_VERSION"));
                process::exit(0);
            }
            "--workspace" => {
                workspace = PathBuf::from(next_value(&mut args, "--workspace")?);
            }
            "--format" => {
                format = parse_format(&next_value(&mut args, "--format")?)?;
            }
            "--check" => {
                check = true;
            }
            "--strip" | "-p" => {
                strip = next_value(&mut args, &arg)?
                    .parse()
                    .map_err(|_| format!("{arg} must be a non-negative integer"))?;
            }
            "--reverse" | "-R" => {
                reverse = true;
            }
            "--max-output-chars" => {
                max_output_chars = next_value(&mut args, "--max-output-chars")?
                    .parse()
                    .map_err(|_| "--max-output-chars must be a non-negative integer".to_string())?;
            }
            "--patch-file" => {
                patch_file = Some(PathBuf::from(next_value(&mut args, "--patch-file")?));
            }
            _ if arg.starts_with("-p") && arg.len() > 2 => {
                strip = arg[2..]
                    .parse()
                    .map_err(|_| "-pN must use a non-negative integer")?;
            }
            _ => return Err(format!("unknown argument: {arg}")),
        }
    }

    let patch = match patch_file {
        Some(path) => fs::read_to_string(&path)
            .map_err(|error| format!("failed to read patch file {}: {error}", path.display()))?,
        None => {
            let mut patch = String::new();
            io::stdin()
                .read_to_string(&mut patch)
                .map_err(|error| format!("failed to read patch from stdin: {error}"))?;
            patch
        }
    };

    let options = ApplyPatchOptions {
        workspace,
        format,
        check,
        strip,
        reverse,
        max_output_chars,
    };
    Ok(apply_patch(&patch, &options))
}

fn next_value<I>(args: &mut std::iter::Peekable<I>, flag: &str) -> Result<String, String>
where
    I: Iterator<Item = String>,
{
    args.next()
        .ok_or_else(|| format!("{flag} requires a value"))
}

fn parse_format(value: &str) -> Result<PatchFormat, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "auto" => Ok(PatchFormat::Auto),
        "codex" => Ok(PatchFormat::Codex),
        "unified" => Ok(PatchFormat::Unified),
        other => Err(format!(
            "unsupported format {other}; expected auto, codex, or unified"
        )),
    }
}

fn print_help() {
    println!(
        "stellaclaw-apply-patch {}\n\nUSAGE:\n  stellaclaw-apply-patch [OPTIONS] < patch.txt\n\nOPTIONS:\n      --workspace <DIR>          Workspace root. Defaults to current directory.\n      --format <auto|codex|unified>\n                                  Patch format. Defaults to auto.\n      --check                    Validate without writing files.\n      --patch-file <FILE>        Read patch from a file instead of stdin.\n  -p, --strip <N>                Strip N leading path components for unified diff.\n  -R, --reverse                  Apply unified diff in reverse.\n      --max-output-chars <N>     Max stdout/stderr chars returned for unified diff.\n      --version                  Print version.\n  -h, --help                     Print help.",
        env!("CARGO_PKG_VERSION")
    );
}

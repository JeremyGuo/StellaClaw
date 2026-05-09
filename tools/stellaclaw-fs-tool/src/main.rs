use std::{
    env, fs,
    io::{self, Read},
    path::PathBuf,
    process,
};

use serde_json::{json, Value};

use stellaclaw_fs_tool::{
    apply_patch, file_read, file_write, ApplyPatchOptions, FileReadOptions, FileWriteOptions,
    PatchFormat,
};

fn main() {
    let exit_code = match run() {
        Ok(result) => {
            println!("{result}");
            tool_exit_code(&result)
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
    let mut raw_args = env::args().skip(1).collect::<Vec<_>>();
    match raw_args.first().map(String::as_str) {
        Some("apply-patch") => {
            raw_args.remove(0);
            run_apply_patch(raw_args)
        }
        Some("file-read") => {
            raw_args.remove(0);
            run_file_read(raw_args)
        }
        Some("file-write") => {
            raw_args.remove(0);
            run_file_write(raw_args)
        }
        Some("-h" | "--help" | "--version") | None => run_apply_patch(raw_args),
        Some(command) if command.starts_with('-') => run_apply_patch(raw_args),
        Some(command) => Err(format!(
            "unknown command: {command}; expected apply-patch, file-read, or file-write"
        )),
    }
}

fn run_apply_patch(raw_args: Vec<String>) -> Result<Value, String> {
    let mut workspace =
        env::current_dir().map_err(|error| format!("failed to read cwd: {error}"))?;
    let mut format = PatchFormat::Auto;
    let mut check = false;
    let mut strip = 0usize;
    let mut reverse = false;
    let mut max_output_chars = 1000usize;
    let mut patch_file: Option<PathBuf> = None;

    let mut args = raw_args.into_iter().peekable();
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

fn run_file_read(raw_args: Vec<String>) -> Result<Value, String> {
    let mut workspace =
        env::current_dir().map_err(|error| format!("failed to read cwd: {error}"))?;
    let mut file_path: Option<String> = None;
    let mut start_line = 1usize;
    let mut end_line: Option<usize> = None;
    let mut limit: Option<usize> = None;

    let mut args = raw_args.into_iter().peekable();
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
            "--file-path" | "--path" => {
                file_path = Some(next_value(&mut args, &arg)?);
            }
            "--start-line" | "--offset" => {
                start_line = parse_usize(&next_value(&mut args, &arg)?, &arg)?;
            }
            "--end-line" => {
                end_line = Some(parse_usize(
                    &next_value(&mut args, "--end-line")?,
                    "--end-line",
                )?);
            }
            "--limit" => {
                limit = Some(parse_usize(&next_value(&mut args, "--limit")?, "--limit")?);
            }
            _ => return Err(format!("unknown file-read argument: {arg}")),
        }
    }
    Ok(file_read(&FileReadOptions {
        workspace,
        file_path: file_path.ok_or_else(|| "file-read requires --file-path".to_string())?,
        start_line,
        end_line,
        limit,
    }))
}

fn run_file_write(raw_args: Vec<String>) -> Result<Value, String> {
    let mut workspace =
        env::current_dir().map_err(|error| format!("failed to read cwd: {error}"))?;
    let mut file_path: Option<String> = None;
    let mut mode = "overwrite".to_string();
    let mut content: Option<String> = None;
    let mut content_file: Option<PathBuf> = None;

    let mut args = raw_args.into_iter().peekable();
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
            "--file-path" | "--path" => {
                file_path = Some(next_value(&mut args, &arg)?);
            }
            "--mode" => {
                mode = next_value(&mut args, "--mode")?;
            }
            "--content" => {
                content = Some(next_value(&mut args, "--content")?);
            }
            "--content-file" => {
                content_file = Some(PathBuf::from(next_value(&mut args, "--content-file")?));
            }
            _ => return Err(format!("unknown file-write argument: {arg}")),
        }
    }
    let content = match (content, content_file) {
        (Some(content), None) => content,
        (None, Some(path)) => fs::read_to_string(&path)
            .map_err(|error| format!("failed to read content file {}: {error}", path.display()))?,
        (None, None) => {
            let mut content = String::new();
            io::stdin()
                .read_to_string(&mut content)
                .map_err(|error| format!("failed to read content from stdin: {error}"))?;
            content
        }
        (Some(_), Some(_)) => {
            return Err("--content and --content-file cannot be used together".to_string());
        }
    };
    Ok(file_write(&FileWriteOptions {
        workspace,
        file_path: file_path.ok_or_else(|| "file-write requires --file-path".to_string())?,
        content,
        mode,
    }))
}

fn tool_exit_code(result: &Value) -> i32 {
    if let Some(applied) = result.get("applied").and_then(Value::as_bool) {
        return if applied { 0 } else { 1 };
    }
    if result.get("ok").and_then(Value::as_bool) == Some(false) {
        return 1;
    }
    0
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

fn parse_usize(value: &str, flag: &str) -> Result<usize, String> {
    value
        .parse()
        .map_err(|_| format!("{flag} must be a non-negative integer"))
}

fn print_help() {
    println!(
        "stellaclaw-fs-tool {}\n\nUSAGE:\n  stellaclaw-fs-tool apply-patch [OPTIONS] < patch.txt\n  stellaclaw-fs-tool file-read --file-path <PATH> [OPTIONS]\n  stellaclaw-fs-tool file-write --file-path <PATH> [OPTIONS] < content.txt\n  stellaclaw-fs-tool [OPTIONS] < patch.txt\n\nCOMMANDS:\n  apply-patch                  Apply a Codex or unified patch.\n  file-read                    Read UTF-8 text from a file with optional line range.\n  file-write                   Write UTF-8 text to a file.\n\nCOMMON OPTIONS:\n      --workspace <DIR>          Workspace root. Defaults to current directory.\n      --version                  Print version.\n  -h, --help                     Print help.",
        env!("CARGO_PKG_VERSION")
    );
}

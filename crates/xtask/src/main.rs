//! Workspace task runner (the `cargo xtask` convention, see
//! https://github.com/matklad/cargo-xtask).
//!
//! Usage (via the alias in .cargo/config.toml):
//!   cargo xtask vsix    — build the engine + extension and package a
//!                         current-platform VSIX for local install/testing
//!   cargo xtask help    — list tasks
//!
//! Multi-platform release packaging stays in scripts/package-all.sh;
//! this task exists for the local dev loop (F5 alternative / manual smoke).

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let task = env::args().nth(1).unwrap_or_else(|| "help".to_string());
    let result = match task.as_str() {
        "vsix" => package_vsix(),
        "help" | "--help" | "-h" => {
            print_help();
            Ok(())
        }
        other => {
            eprintln!("error: unknown task `{other}`\n");
            print_help();
            std::process::exit(1);
        }
    };
    if let Err(err) = result {
        eprintln!("\nxtask failed: {err}");
        std::process::exit(1);
    }
}

fn print_help() {
    println!(
        "cargo xtask <TASK>

Tasks:
  vsix    Build the engine (release) and the extension, then package a
          VSIX for the current platform into editors/vscode/ (for local
          install & testing). Multi-platform release builds remain
          scripts/package-all.sh.
  help    Show this message."
    );
}

/// `cargo xtask vsix`: engine release build + vite build + vsce package.
fn package_vsix() -> Result<(), String> {
    let root = workspace_root()?;
    let ext_dir = root.join("editors/vscode");
    let exe_suffix = if env::consts::OS == "windows" {
        ".exe"
    } else {
        ""
    };
    let engine_name = format!("bamboo-engine{exe_suffix}");
    let engine_bin = root.join("target/release").join(&engine_name);
    let server_bin = ext_dir.join("server").join(&engine_name);

    run(Command::new("cargo")
        .arg("build")
        .arg("--release")
        .arg("--bin")
        .arg("bamboo-engine")
        .current_dir(&root))?;

    run(Command::new("npm")
        .arg("run")
        .arg("compile")
        .current_dir(&ext_dir))?;

    fs::create_dir_all(server_bin.parent().unwrap())
        .map_err(|e| format!("create server/ dir: {e}"))?;
    fs::copy(&engine_bin, &server_bin).map_err(|e| {
        format!(
            "copy {} -> {}: {e}",
            engine_bin.display(),
            server_bin.display()
        )
    })?;

    // vsce package (via npm run package so the vsce invocation stays
    // defined in one place, package.json). Any stale VSIX in the tree is
    // already excluded from the new package via .vscodeignore.
    let status = run_capture(
        Command::new("npm")
            .arg("run")
            .arg("package")
            .current_dir(&ext_dir),
    )?;
    let _ = status; // vsce output already printed by run_capture

    // Remove the binary so a later package can never bundle a stale
    // platform build (same hygiene as scripts/package-all.sh).
    fs::remove_file(&server_bin).map_err(|e| format!("clean up {e}"))?;

    let version = read_version(&ext_dir)?;
    let vsix = ext_dir.join(format!("git-bamboo-{version}.vsix"));
    println!("\n==> VSIX ready: {}", vsix.display());
    Ok(())
}

fn run(command: &mut Command) -> Result<(), String> {
    println!("==> {}", command_line(command));
    let status = command
        .status()
        .map_err(|e| format!("spawn {}: {e}", command_line(command)))?;
    if !status.success() {
        return Err(format!("command failed: {}", command_line(command)));
    }
    Ok(())
}

fn run_capture(command: &mut Command) -> Result<String, String> {
    println!("==> {}", command_line(command));
    let output = command
        .output()
        .map_err(|e| format!("spawn {}: {e}", command_line(command)))?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    print!("{stdout}");
    eprint!("{}", String::from_utf8_lossy(&output.stderr));
    if !output.status.success() {
        return Err(format!("command failed: {}", command_line(command)));
    }
    Ok(stdout)
}

fn command_line(command: &Command) -> String {
    let mut parts = vec![command.get_program().to_string_lossy().into_owned()];
    parts.extend(command.get_args().map(|a| a.to_string_lossy().into_owned()));
    parts.join(" ")
}

/// Workspace root: xtask is a workspace member, so CARGO_MANIFEST_DIR's
/// parent is the root regardless of invocation directory.
fn workspace_root() -> Result<PathBuf, String> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(Path::parent)
        .map(|p| p.to_path_buf())
        .ok_or_else(|| "cannot locate workspace root".to_string())
}

fn read_version(ext_dir: &Path) -> Result<String, String> {
    let pkg = fs::read_to_string(ext_dir.join("package.json"))
        .map_err(|e| format!("read package.json: {e}"))?;
    // Minimal JSON scrape — xtask stays dependency-free, and the version
    // is a flat "version": "x.y.z" field we control.
    let marker = "\"version\":";
    let idx = pkg
        .find(marker)
        .ok_or("version field missing in package.json")?;
    let rest = &pkg[idx + marker.len()..];
    let start = rest.find('"').ok_or("version value malformed")? + 1;
    let end = start + rest[start..].find('"').ok_or("version value malformed")?;
    Ok(rest[start..end].to_string())
}

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use tempfile::NamedTempFile;
use tracing::debug;

use crate::config::{ChainStep, IoMode, ProcessorRule};

/// Result of applying a processor rule to a file.
pub enum ProcessResult {
    /// File was modified. Contains the new content.
    Modified(Vec<u8>),
    /// File was not modified (processor ran but produced identical output, or
    /// a step signalled no change via exit code).
    Unchanged,
}

/// Apply a processor rule to `input_data` (bytes of the file).
/// `filename` is used only for logging.
pub fn apply_rule(rule: &ProcessorRule, input_data: &[u8], filename: &str) -> Result<ProcessResult> {
    let result = if let Some(shell_expr) = &rule.shell {
        apply_shell(shell_expr, rule.io, input_data, filename)?
    } else if !rule.chain.is_empty() {
        apply_chain(&rule.chain, input_data, filename)?
    } else {
        bail!("processor rule for '{}' has neither 'chain' nor 'shell'", rule.r#match);
    };

    if result == input_data {
        Ok(ProcessResult::Unchanged)
    } else {
        Ok(ProcessResult::Modified(result))
    }
}

/// Apply an explicit chain of steps, threading output of each into the next.
fn apply_chain(steps: &[ChainStep], input_data: &[u8], filename: &str) -> Result<Vec<u8>> {
    let mut data = input_data.to_vec();
    for (i, step) in steps.iter().enumerate() {
        debug!("  chain step {}: {} {:?}", i, step.command, step.args);
        data = run_step(&step.command, &step.args, step.io, &data, filename)
            .with_context(|| format!("chain step {} ({}) failed on '{}'", i, step.command, filename))?;
    }
    Ok(data)
}

/// Apply a shell expression.
fn apply_shell(expr: &str, io: IoMode, input_data: &[u8], filename: &str) -> Result<Vec<u8>> {
    debug!("  shell: {}", expr);
    // For shell, we still need a temp file for {input} substitution.
    // We write input_data to a temp file, substitute {input}, run via sh -c.
    let input_tmp = write_temp(input_data, extension_from(filename))?;
    let input_path = input_tmp.path().to_owned();

    let expanded = expr.replace("{input}", &input_path.to_string_lossy());

    match io {
        IoMode::StdinStdout => {
            // Pipe input_data through stdin, capture stdout.
            let mut child = Command::new("sh")
                .arg("-c")
                .arg(&expanded)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .context("failed to spawn sh")?;
            child.stdin.take().unwrap().write_all(input_data)?;
            let output = child.wait_with_output()?;
            check_status(&output.status, "sh", &expanded)?;
            Ok(output.stdout)
        }
        IoMode::FileToStdout | IoMode::InPlace | IoMode::FileToFile => {
            // For all other modes on shell, just capture stdout.
            let output = Command::new("sh")
                .arg("-c")
                .arg(&expanded)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .output()
                .context("failed to spawn sh")?;
            check_status(&output.status, "sh", &expanded)?;
            if output.stdout.is_empty() {
                // Shell modified file in place; read it back.
                let data = std::fs::read(&input_path)?;
                Ok(data)
            } else {
                Ok(output.stdout)
            }
        }
    }
}

/// Run a single command step and return the resulting bytes.
fn run_step(command: &str, args: &[String], io: IoMode, input_data: &[u8], filename: &str) -> Result<Vec<u8>> {
    match io {
        IoMode::InPlace => {
            let tmp = write_temp(input_data, extension_from(filename))?;
            let path = tmp.path().to_owned();
            let expanded_args = expand_args(args, &path, &path);
            let status = Command::new(command)
                .args(&expanded_args)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .status()
                .with_context(|| format!("failed to spawn '{}'", command))?;
            check_status(&status, command, &format!("{:?}", expanded_args))?;
            let result = std::fs::read(&path)?;
            Ok(result)
        }
        IoMode::FileToFile => {
            let input_tmp = write_temp(input_data, extension_from(filename))?;
            let input_path = input_tmp.path().to_owned();
            let output_tmp = NamedTempFile::new()?;
            let output_path = output_tmp.path().to_owned();
            let expanded_args = expand_args(args, &input_path, &output_path);
            let status = Command::new(command)
                .args(&expanded_args)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .status()
                .with_context(|| format!("failed to spawn '{}'", command))?;
            check_status(&status, command, &format!("{:?}", expanded_args))?;
            let result = std::fs::read(&output_path)?;
            Ok(result)
        }
        IoMode::FileToStdout => {
            let tmp = write_temp(input_data, extension_from(filename))?;
            let path = tmp.path().to_owned();
            let expanded_args = expand_args(args, &path, &path);
            let output = Command::new(command)
                .args(&expanded_args)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .output()
                .with_context(|| format!("failed to spawn '{}'", command))?;
            check_status(&output.status, command, &format!("{:?}", expanded_args))?;
            Ok(output.stdout)
        }
        IoMode::StdinStdout => {
            let mut child = Command::new(command)
                .args(args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .with_context(|| format!("failed to spawn '{}'", command))?;
            child.stdin.take().unwrap().write_all(input_data)?;
            let output = child.wait_with_output()?;
            check_status(&output.status, command, &format!("{:?}", args))?;
            Ok(output.stdout)
        }
    }
}

fn write_temp(data: &[u8], ext: &str) -> Result<NamedTempFile> {
    let suffix = if ext.is_empty() {
        String::new()
    } else {
        format!(".{}", ext)
    };
    let mut tmp = tempfile::Builder::new().suffix(&suffix).tempfile()?;
    tmp.write_all(data)?;
    tmp.flush()?;
    Ok(tmp)
}

fn expand_args(args: &[String], input: &Path, output: &Path) -> Vec<String> {
    args.iter()
        .map(|a| {
            a.replace("{input}", &input.to_string_lossy())
             .replace("{output}", &output.to_string_lossy())
        })
        .collect()
}

fn extension_from(filename: &str) -> &str {
    Path::new(filename)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
}

fn check_status(status: &std::process::ExitStatus, cmd: &str, detail: &str) -> Result<()> {
    if !status.success() {
        bail!("'{}' exited with {}: {}", cmd, status, detail);
    }
    Ok(())
}

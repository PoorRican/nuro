use std::io;
use std::process::Command;

use super::composer::normalize_line_endings;

pub(super) trait ClipboardProvider {
    fn read_text(&mut self) -> Result<Option<String>, String>;
}

pub(super) struct CommandClipboard;

impl ClipboardProvider for CommandClipboard {
    fn read_text(&mut self) -> Result<Option<String>, String> {
        #[cfg(target_os = "macos")]
        {
            return run_clipboard_command("pbpaste", &[]);
        }

        #[cfg(target_os = "linux")]
        {
            for (cmd, args) in [
                ("wl-paste", vec!["-n"]),
                ("xclip", vec!["-selection", "clipboard", "-o"]),
                ("xsel", vec!["--clipboard", "--output"]),
            ] {
                if let Some(value) = run_clipboard_command(cmd, &args)? {
                    return Ok(Some(value));
                }
            }
            return Ok(None);
        }

        #[cfg(target_os = "windows")]
        {
            return run_clipboard_command(
                "powershell",
                &["-NoProfile", "-Command", "Get-Clipboard"],
            );
        }

        #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
        {
            Ok(None)
        }
    }
}

fn run_clipboard_command(command: &str, args: &[&str]) -> Result<Option<String>, String> {
    let output = match Command::new(command).args(args).output() {
        Ok(output) => output,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.to_string()),
    };

    if !output.status.success() {
        return Ok(None);
    }

    let content = String::from_utf8_lossy(&output.stdout).to_string();
    if content.trim().is_empty() {
        return Ok(None);
    }

    Ok(Some(normalize_line_endings(content.trim_end_matches('\n'))))
}

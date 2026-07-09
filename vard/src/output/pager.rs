//! Shared pager subprocess spawn. Used by `--help` long-form rendering and,
//! later, by record output on a TTY. Honors `$PAGER`; defaults to `less -FRX`
//! (`-F` quit if it fits, `-R` raw ANSI, `-X` no init/deinit).

use std::env;
use std::io::Write;
use std::process::{Command, Stdio};

/// Whether output should be paged: only on a TTY, only when not suppressed, and
/// only when the buffer is taller than the terminal (minus a two-line margin).
///
/// `term_height` is resolved once by the caller (alongside the TTY check) and
/// threaded in, so a single terminal query serves palette resolution, paging,
/// and rendering.
pub fn should_page(
    buffer_line_count: usize,
    no_pager: bool,
    stdout_is_tty: bool,
    term_height: usize,
) -> bool {
    if no_pager || !stdout_is_tty {
        return false;
    }
    buffer_line_count > term_height.saturating_sub(2)
}

/// Resolve the pager command and args from `$PAGER`, defaulting to `less -FRX`.
pub fn resolve_pager() -> (String, Vec<String>) {
    match env::var("PAGER") {
        Ok(p) if !p.is_empty() => {
            let mut parts = p.split_whitespace().map(String::from);
            let cmd = parts.next().unwrap_or_else(|| "less".to_string());
            let args: Vec<String> = parts.collect();
            (cmd, args)
        }
        _ => ("less".to_string(), vec!["-FRX".to_string()]),
    }
}

/// Pipe `buffer` through the resolved pager; if the pager cannot be spawned,
/// warn to `stderr` and write `buffer` straight to `stdout`. A broken pipe
/// (pager exited early) is swallowed.
pub fn spawn_pager_or_passthrough(
    buffer: &[u8],
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
    context: &str,
) -> std::io::Result<()> {
    let (cmd, args) = resolve_pager();
    let mut child = match Command::new(&cmd).args(&args).stdin(Stdio::piped()).spawn() {
        Ok(c) => c,
        Err(e) => {
            writeln!(
                stderr,
                "{context}: pager '{cmd}' failed: {e}; writing directly to terminal"
            )?;
            stdout.write_all(buffer)?;
            return Ok(());
        }
    };
    if let Some(stdin) = child.stdin.as_mut()
        && let Err(e) = stdin.write_all(buffer)
        && e.kind() != std::io::ErrorKind::BrokenPipe
    {
        return Err(e);
    }
    let _ = child.wait();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_pager_flag_disables() {
        assert!(!should_page(1000, true, true, 24));
    }

    #[test]
    fn non_tty_disables() {
        assert!(!should_page(1000, false, false, 24));
    }

    #[test]
    fn short_output_skips_pager() {
        assert!(!should_page(5, false, true, 24));
    }

    #[test]
    fn tall_output_pages() {
        assert!(should_page(1000, false, true, 24));
    }
}

//! Personal Access Token input for `scenery auth token`.
//!
//! A `vh_pat_...` token is a long-lived registry credential, so it must
//! never travel on the command line: argv is readable by every local user
//! (`ps`, `/proc/<pid>/cmdline`) for as long as the process runs, and it
//! lands in shell history. `auth token` therefore takes no token argument
//! and reads the token from:
//!
//! - **stdin, when stdin is not a terminal** — the headless / CI path, e.g.
//!   `scenery auth token < pat.txt`.
//! - **a hidden prompt, when stdin is a terminal** — terminal echo is turned
//!   off while the token is typed or pasted (Unix).
//!
//! [`resolve_token`] is the pure decision function (injectable readers, so
//! it is testable without a terminal); [`read_token_live`] wires it to the
//! real stdin and is what `cmd_auth_token` calls.

use std::io::{self, BufRead, IsTerminal, Write};

fn no_token_error() -> String {
    "no Personal Access Token was provided — run `scenery auth token` and \
     paste it at the prompt, or pipe it in on stdin \
     (`scenery auth token < pat.txt`)"
        .to_string()
}

/// Pick the token source and return the trimmed token.
///
/// `read_piped` is used when stdin is not a terminal, `read_hidden` when it
/// is. Each reader returns `Ok(Some(line))` for a raw line, `Ok(None)` on
/// EOF, or `Err(_)` if the read failed. An empty / whitespace-only line or
/// EOF is an error, so a failed scripted run exits non-zero.
pub fn resolve_token<P, H>(
    stdin_is_terminal: bool,
    read_piped: P,
    read_hidden: H,
) -> Result<String, String>
where
    P: FnOnce() -> io::Result<Option<String>>,
    H: FnOnce() -> io::Result<Option<String>>,
{
    let line = if stdin_is_terminal {
        read_hidden()
    } else {
        read_piped()
    }
    .map_err(|e| format!("failed to read the Personal Access Token: {e}"))?;

    match line {
        Some(raw) => {
            let token = raw.trim();
            if token.is_empty() {
                Err(no_token_error())
            } else {
                Ok(token.to_string())
            }
        }
        None => Err(no_token_error()),
    }
}

fn read_one_line<R: BufRead>(reader: &mut R) -> io::Result<Option<String>> {
    let mut line = String::new();
    let n = reader.read_line(&mut line)?;
    if n == 0 {
        Ok(None)
    } else {
        Ok(Some(line))
    }
}

/// Read one line from the terminal on stdin with echo turned off, restoring
/// the original terminal settings afterwards.
#[cfg(unix)]
fn read_line_hidden(stdin: &io::Stdin) -> io::Result<Option<String>> {
    use std::os::unix::io::AsRawFd;

    let fd = stdin.as_raw_fd();
    // SAFETY: `termios` is a plain C struct; `tcgetattr` fully initialises
    // it on success, and we bail out before using it on failure.
    let mut original: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(fd, &mut original) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut hidden = original;
    hidden.c_lflag &= !libc::ECHO;
    // SAFETY: `fd` is stdin and `hidden` is a valid termios copied from it.
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &hidden) } != 0 {
        return Err(io::Error::last_os_error());
    }

    let result = read_one_line(&mut stdin.lock());

    // SAFETY: restores the settings read above on the same fd.
    unsafe { libc::tcsetattr(fd, libc::TCSANOW, &original) };
    result
}

/// No termios outside Unix: the token is still read from the console rather
/// than argv, but is echoed as typed.
#[cfg(not(unix))]
fn read_line_hidden(stdin: &io::Stdin) -> io::Result<Option<String>> {
    read_one_line(&mut stdin.lock())
}

/// Real entry point used by `cmd_auth_token`: reads the token from stdin
/// (piped) or from a hidden prompt (terminal).
pub fn read_token_live() -> Result<String, String> {
    let stdin = io::stdin();
    let is_terminal = stdin.is_terminal();
    resolve_token(
        is_terminal,
        || read_one_line(&mut stdin.lock()),
        || {
            eprint!("Personal Access Token: ");
            io::stderr().flush().ok();
            let line = read_line_hidden(&stdin);
            // Echo was off, so the user's Enter didn't move the cursor.
            eprintln!();
            line
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(s: &'static str) -> impl FnOnce() -> io::Result<Option<String>> {
        move || Ok(Some(s.to_string()))
    }

    fn unused() -> impl FnOnce() -> io::Result<Option<String>> {
        || panic!("this reader must not be used")
    }

    #[test]
    fn piped_stdin_is_read_when_not_a_terminal() {
        let result = resolve_token(false, line("vh_pat_piped\n"), unused());
        assert_eq!(result, Ok("vh_pat_piped".to_string()));
    }

    #[test]
    fn hidden_prompt_is_used_when_stdin_is_a_terminal() {
        let result = resolve_token(true, unused(), line("vh_pat_typed\n"));
        assert_eq!(result, Ok("vh_pat_typed".to_string()));
    }

    #[test]
    fn surrounding_whitespace_and_cr_are_trimmed() {
        let result = resolve_token(false, line("  vh_pat_padded \r\n"), unused());
        assert_eq!(result, Ok("vh_pat_padded".to_string()));
    }

    #[test]
    fn empty_input_is_an_error() {
        let err = resolve_token(true, unused(), line("   \n")).expect_err("empty must fail");
        assert!(err.contains("scenery auth token < pat.txt"), "{err}");
    }

    #[test]
    fn eof_is_an_error() {
        let err = resolve_token(false, || Ok(None), unused()).expect_err("EOF must fail");
        assert!(err.contains("no Personal Access Token"), "{err}");
    }

    #[test]
    fn read_failure_is_an_error() {
        let err = resolve_token(false, || Err(io::Error::other("boom")), unused())
            .expect_err("read error must fail");
        assert!(err.contains("boom"), "{err}");
    }
}

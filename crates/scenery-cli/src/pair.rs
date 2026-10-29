//! Pairing-code resolution for `vectorhawk auth pair`.
//!
//! The positional `code` argument is optional. When it's absent, the code
//! is resolved in a fixed order:
//!
//! 1. **Explicit CLI argument** — if given, wins unconditionally and
//!    behaves exactly as before this module existed.
//! 2. **`VH_PAIR_CODE` env var**, if set and non-empty. This is the
//!    unattended path for MDM / Intune / Jamf rollouts — it must work with
//!    no terminal at all, so it's used without prompting.
//! 3. **Interactive prompt**, if a terminal is available.
//! 4. **Otherwise: a clear error, non-zero exit.** A scripted install that
//!    failed to pair must look failed in logs — never exit 0 here.
//!
//! [`resolve_pair_code`] is the pure decision function: it takes the
//! explicit arg, the env value, an "is a terminal available" flag, and an
//! injectable line-reader, so the ordering, retry, and error-message logic
//! is fully testable without a real terminal. [`resolve_pair_code_live`]
//! wires it to the real OS-level terminal detection and is what
//! `cmd_auth_pair` calls.

use std::io::{self, BufRead, Write};

/// Env var for the unattended pairing path (MDM / Intune / Jamf rollouts).
/// Read without prompting when set and non-empty.
pub const PAIR_CODE_ENV: &str = "VH_PAIR_CODE";

/// Bounded re-prompt count on empty input: 2 retries, 3 attempts total.
/// Never loop forever — this runs inside install scripts.
const MAX_ATTEMPTS: u32 = 3;

/// A once-per-call line reader sourced from a real terminal device.
type LineReader = Box<dyn FnMut() -> io::Result<Option<String>>>;

/// Trim surrounding whitespace (this includes a trailing `\r` left by
/// Windows terminals and some pasted input — `\r` is Unicode whitespace) so
/// `read_line`'s raw line becomes a clean code.
fn clean(raw: &str) -> String {
    raw.trim().to_string()
}

fn no_code_error(reason: &str) -> String {
    format!(
        "no pairing code given and {reason} — pass it as an argument \
         (`vectorhawk auth pair <code>`) or set {PAIR_CODE_ENV}"
    )
}

/// Resolve the pairing code to use for `vectorhawk auth pair`.
///
/// `read_line` is called once per prompt attempt (only when `explicit` is
/// `None`, `env_value` is absent/empty, and `terminal_available` is `true`)
/// and must return:
/// - `Ok(Some(line))` — a raw line was read (not yet trimmed here)
/// - `Ok(None)` — EOF, no more input available
/// - `Err(_)` — the underlying read failed
///
/// Both `Ok(None)` and `Err(_)` stop retrying immediately (there's no point
/// re-prompting a closed input stream); only an empty *line* triggers a
/// re-prompt, up to `MAX_ATTEMPTS` total attempts.
pub fn resolve_pair_code<F>(
    explicit: Option<&str>,
    env_value: Option<&str>,
    terminal_available: bool,
    mut read_line: F,
) -> Result<String, String>
where
    F: FnMut() -> io::Result<Option<String>>,
{
    // 1. Explicit argument wins unconditionally — behavior unchanged from
    // before this module existed.
    if let Some(code) = explicit {
        return Ok(code.to_string());
    }

    // 2. VH_PAIR_CODE — the unattended path. Used without prompting.
    if let Some(env) = env_value {
        let trimmed = clean(env);
        if !trimmed.is_empty() {
            return Ok(trimmed);
        }
    }

    // 3. Interactive prompt, if a terminal is available.
    if terminal_available {
        println!(
            "No pairing code given. Find it in the portal's device setup \
             screen (open the catalog page — if this device isn't \
             registered yet, you'll see it there)."
        );
        for attempt in 1..=MAX_ATTEMPTS {
            print!("Pairing code: ");
            io::stdout().flush().ok();
            match read_line() {
                Ok(Some(line)) => {
                    let trimmed = clean(&line);
                    if !trimmed.is_empty() {
                        return Ok(trimmed);
                    }
                    if attempt < MAX_ATTEMPTS {
                        println!("Pairing code cannot be empty — try again.");
                    }
                }
                Ok(None) | Err(_) => break,
            }
        }
        return Err(no_code_error("no pairing code was entered"));
    }

    // 4. No argument, no env, no terminal — clear, non-zero-exit error.
    Err(no_code_error("no terminal available to prompt on"))
}

/// Real (non-pure) terminal glue for [`resolve_pair_code`]: detects whether
/// an interactive terminal is available and returns a line-reader sourced
/// from it.
///
/// - **Unix**: opens `/dev/tty` directly — the same device Homebrew and
///   rustup read pairing/confirmation prompts from — so an inherited
///   non-tty stdin (e.g. `curl -fsSL … | sh`, where the shell's stdin is
///   the install script itself) isn't mistaken for "no terminal" when a
///   real one is sitting right there. Falls back to stdin if `/dev/tty`
///   can't be opened at all (no controlling terminal — cron, a
///   LaunchDaemon, Homebrew's non-interactive `post_install`); in that
///   fallback case, "is a terminal available" is whatever
///   `stdin.is_terminal()` reports.
/// - **Windows**: no `/dev/tty` equivalent — `std::io::IsTerminal` on
///   stdin.
#[cfg(unix)]
fn detect_terminal() -> (bool, LineReader) {
    use std::fs::OpenOptions;

    if let Ok(tty) = OpenOptions::new().read(true).write(true).open("/dev/tty") {
        let mut reader = io::BufReader::new(tty);
        return (true, Box::new(move || read_one_line(&mut reader)));
    }

    use std::io::IsTerminal;
    let is_tty = io::stdin().is_terminal();
    let mut reader = io::BufReader::new(io::stdin());
    (is_tty, Box::new(move || read_one_line(&mut reader)))
}

#[cfg(windows)]
fn detect_terminal() -> (bool, LineReader) {
    use std::io::IsTerminal;
    let is_tty = io::stdin().is_terminal();
    let mut reader = io::BufReader::new(io::stdin());
    (is_tty, Box::new(move || read_one_line(&mut reader)))
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

/// Real entry point used by `cmd_auth_pair`: wires OS terminal detection and
/// the `VH_PAIR_CODE` env var to [`resolve_pair_code`].
pub fn resolve_pair_code_live(explicit: Option<&str>) -> Result<String, String> {
    let env_value = std::env::var(PAIR_CODE_ENV).ok();
    let (terminal_available, read_line) = detect_terminal();
    resolve_pair_code(
        explicit,
        env_value.as_deref(),
        terminal_available,
        read_line,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Builds a `read_line` closure that yields each line in order, then
    /// `Ok(None)` (EOF) forever after.
    fn scripted_reader(lines: Vec<&'static str>) -> impl FnMut() -> io::Result<Option<String>> {
        let queue = RefCell::new(lines.into_iter());
        move || Ok(queue.borrow_mut().next().map(|s| s.to_string()))
    }

    #[test]
    fn explicit_argument_wins_over_env() {
        let result = resolve_pair_code(
            Some("VH-ARG1"),
            Some("VH-ENV1"),
            true,
            scripted_reader(vec![]),
        );
        assert_eq!(result, Ok("VH-ARG1".to_string()));
    }

    #[test]
    fn env_used_when_no_argument_and_no_terminal() {
        let result = resolve_pair_code(None, Some("VH-ENV1"), false, scripted_reader(vec![]));
        assert_eq!(result, Ok("VH-ENV1".to_string()));
    }

    #[test]
    fn env_used_without_prompting_even_when_terminal_available() {
        // VH_PAIR_CODE must work with no terminal at all, and shouldn't
        // require one either — it's used without prompting regardless.
        let result = resolve_pair_code(None, Some("VH-ENV1"), true, scripted_reader(vec![]));
        assert_eq!(result, Ok("VH-ENV1".to_string()));
    }

    #[test]
    fn prompt_used_when_no_argument_no_env_terminal_available() {
        let result = resolve_pair_code(None, None, true, scripted_reader(vec!["VH-PROMPTED\n"]));
        assert_eq!(result, Ok("VH-PROMPTED".to_string()));
    }

    #[test]
    fn no_argument_no_env_no_terminal_errors_naming_both_escapes() {
        let result = resolve_pair_code(None, None, false, scripted_reader(vec![]));
        let err = result.expect_err("expected an error with no terminal available");
        assert!(
            err.contains("vectorhawk auth pair <code>"),
            "error should name the argument escape: {err}"
        );
        assert!(
            err.contains("VH_PAIR_CODE"),
            "error should name the env var escape: {err}"
        );
        assert!(
            err.contains("no terminal available"),
            "error should explain why: {err}"
        );
    }

    #[test]
    fn empty_input_reprompts_then_gives_up_after_bounded_attempts() {
        // 3 empty lines = MAX_ATTEMPTS all consumed with no code — should
        // give up rather than loop forever, and still name both escapes.
        let result =
            resolve_pair_code(None, None, true, scripted_reader(vec!["\n", "   \n", "\n"]));
        let err = result.expect_err("expected an error after exhausting empty-input attempts");
        assert!(err.contains("vectorhawk auth pair <code>"), "{err}");
        assert!(err.contains("VH_PAIR_CODE"), "{err}");
    }

    #[test]
    fn empty_input_reprompts_then_succeeds_on_a_later_attempt() {
        let result = resolve_pair_code(
            None,
            None,
            true,
            scripted_reader(vec!["\n", "VH-SECOND-TRY\n"]),
        );
        assert_eq!(result, Ok("VH-SECOND-TRY".to_string()));
    }

    #[test]
    fn eof_on_prompt_gives_up_immediately_without_exhausting_attempts() {
        // read_line() returning Ok(None) (EOF) mid-loop should stop
        // retrying rather than call read_line() MAX_ATTEMPTS times against
        // a closed stream.
        let calls = RefCell::new(0u32);
        let reader = || {
            *calls.borrow_mut() += 1;
            Ok(None)
        };
        let result = resolve_pair_code(None, None, true, reader);
        assert!(result.is_err());
        assert_eq!(*calls.borrow(), 1, "should stop after the first EOF");
    }

    #[test]
    fn whitespace_and_trailing_cr_are_trimmed_from_prompt_input() {
        let result = resolve_pair_code(
            None,
            None,
            true,
            scripted_reader(vec!["  VH-PADDED-CODE  \r\n"]),
        );
        assert_eq!(result, Ok("VH-PADDED-CODE".to_string()));
    }

    #[test]
    fn whitespace_and_trailing_cr_are_trimmed_from_env_value() {
        let result = resolve_pair_code(
            None,
            Some("  VH-ENV-PADDED\r\n"),
            false,
            scripted_reader(vec![]),
        );
        assert_eq!(result, Ok("VH-ENV-PADDED".to_string()));
    }

    #[test]
    fn empty_env_value_falls_through_to_prompt() {
        let result = resolve_pair_code(
            None,
            Some("   "),
            true,
            scripted_reader(vec!["VH-FALLBACK\n"]),
        );
        assert_eq!(result, Ok("VH-FALLBACK".to_string()));
    }
}

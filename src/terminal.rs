//! zorg's terminal output for a person: confirmation cards and send progress.

use std::io::IsTerminal;
use std::time::Duration;

use console::{style, Key, Term};
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};

/// Error for a confirmation answered with Esc or `n`; nothing was done.
pub const CANCELLED: &str = "\0cancelled";
/// Error for Ctrl-C at a confirmation; zorg exits with status 130.
pub const INTERRUPTED: &str = "\0interrupted";
/// Error for a failure already shown on screen; zorg exits with status 1.
pub const REPORTED: &str = "\0reported";

/// Addresses longer than this are shortened on a card.
const SHORT_ADDRESS_LEN: usize = 43;

/// True when a person is at a terminal: both stdin and stdout are terminals.
pub fn interactive() -> bool {
    std::io::stdin().is_terminal() && Term::stdout().is_term()
}

/// `zatoshis` in ZEC with trailing zeros trimmed, keeping at least `min_decimals`.
pub fn zec(zatoshis: u64, min_decimals: usize) -> String {
    let fraction = format!("{:08}", zatoshis % 100_000_000);
    let mut fraction = fraction.trim_end_matches('0').to_string();
    while fraction.len() < min_decimals {
        fraction.push('0');
    }
    let whole = zatoshis / 100_000_000;
    if fraction.is_empty() {
        whole.to_string()
    } else {
        format!("{whole}.{fraction}")
    }
}

/// `address` with its first 7 and last 8 characters highlighted. A long address
/// is shortened around an ellipsis unless `full` is set.
pub fn address(address: &str, full: bool) -> String {
    let head = &address[..7.min(address.len())];
    let tail = &address[address.len().saturating_sub(8).max(head.len())..];
    let middle = if full || address.len() <= SHORT_ADDRESS_LEN {
        address[head.len()..address.len() - tail.len()].to_string()
    } else {
        format!(
            "{}…{}",
            &address[7..20],
            &address[address.len() - 23..address.len() - 8]
        )
    };
    format!(
        "{}{middle}{}",
        style(head).cyan().bold(),
        style(tail).cyan().bold()
    )
}

/// `address` as its first 8 and last 6 characters, for one-line summaries.
pub fn short_address(address: &str) -> String {
    if address.len() <= 14 {
        return address.to_string();
    }
    format!("{}…{}", &address[..8], &address[address.len() - 6..])
}

/// The privacy tag for a recipient of the given address kind.
pub fn privacy_tag(kind: &str) -> String {
    if matches!(kind, "unified" | "sapling") {
        style(" Shielded ").black().on_green().to_string()
    } else {
        style(" Transparent · amount and recipient are public ")
            .black()
            .on_yellow()
            .to_string()
    }
}

/// A row of a card: a dimmed label padded to `width`, then `value`.
pub fn row(label: &str, width: usize, value: impl std::fmt::Display) -> String {
    format!("  {}  {value}", style(format!("{label:<width$}")).dim())
}

/// Shows the card `render(full)` returns and waits for one key. Enter or `y`
/// confirms. Esc or `n` fails with [`CANCELLED`], and Ctrl-C with
/// [`INTERRUPTED`]. When `address_len` is long, `x` redraws the card with
/// `full` toggled. Keys typed before the card appears are discarded.
pub fn confirm(
    action: &str,
    address_len: usize,
    render: impl Fn(bool) -> Vec<String>,
) -> Result<(), String> {
    let term = Term::stdout();
    let expandable = address_len > SHORT_ADDRESS_LEN;
    let mut full = false;
    let draw = |full: bool| -> Result<usize, String> {
        let mut lines = render(full);
        lines.push(String::new());
        let mut keys = format!(
            "{} {action}   {} cancel",
            style("[Enter/y]").green().bold(),
            style("[Esc/n]").red().bold()
        );
        if expandable {
            let toggle = if full {
                "short address"
            } else {
                "full address"
            };
            keys.push_str(&format!("   {} {toggle}", style("[x]").bold()));
        }
        lines.push(keys);
        let width = usize::from(term.size().1).max(1);
        let mut rows = 0;
        for line in &lines {
            term.write_line(line).map_err(|e| e.to_string())?;
            rows += console::measure_text_width(line).max(1).div_ceil(width);
        }
        Ok(rows)
    };
    let _keys = KeysOnly::new()?;
    let mut rows = draw(full)?;
    let answer = loop {
        match term.read_key_raw().map_err(|e| e.to_string())? {
            Key::Enter | Key::Char('y' | 'Y') => break Ok(()),
            Key::Escape | Key::Char('n' | 'N') => break Err(CANCELLED.to_string()),
            Key::CtrlC => break Err(INTERRUPTED.to_string()),
            Key::Char('x' | 'X') if expandable => {
                full = !full;
                term.clear_last_lines(rows).map_err(|e| e.to_string())?;
                rows = draw(full)?;
            }
            _ => {}
        }
    };
    let _ = term.clear_last_lines(1);
    let _ = term.show_cursor();
    answer
}

/// Holds stdin in a mode where keys are not echoed, not line-buffered, and
/// Ctrl-C arrives as a key, from creation until drop. Creation discards keys
/// typed before it.
struct KeysOnly(libc::termios);

impl KeysOnly {
    fn new() -> Result<Self, String> {
        let mut termios = std::mem::MaybeUninit::uninit();
        // SAFETY: stdin is a terminal here, and tcgetattr fills `termios` on success.
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, termios.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        // SAFETY: tcgetattr succeeded, so `termios` is initialized.
        let original = unsafe { termios.assume_init() };
        let mut keys_only = original;
        keys_only.c_lflag &= !(libc::ECHO | libc::ICANON | libc::ISIG);
        // SAFETY: applies a copy of the current settings with three flags cleared.
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, &keys_only);
        }
        Ok(Self(original))
    }
}

impl Drop for KeysOnly {
    fn drop(&mut self) {
        // SAFETY: restores the settings read in `new`.
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.0);
        }
    }
}

/// Prints a dimmed line, such as "Cancelled. Nothing was sent."
pub fn note(text: &str) {
    println!("{}", style(text).dim());
}

/// Prints a failure line: "✗ <text>".
pub fn failure(text: &str) {
    println!("{} {text}", style("✗").red().bold());
}

/// Prints a success line: "✓ <text>".
pub fn success(text: &str) {
    println!("{} {text}", style("✓").green().bold());
}

/// A one-line spinner whose message changes as a send moves through its stages.
pub struct Progress(ProgressBar);

impl Progress {
    /// A spinner that draws nothing until [`Progress::show`].
    pub fn hidden() -> Self {
        let bar = ProgressBar::with_draw_target(None, ProgressDrawTarget::hidden());
        bar.set_style(ProgressStyle::with_template("{spinner:.cyan} {msg}").expect("template"));
        Self(bar)
    }

    /// Shows the spinner with `message`, or changes its message.
    pub fn show(&self, message: &str) {
        if self.0.is_hidden() {
            self.0.set_draw_target(ProgressDrawTarget::stdout());
            self.0.enable_steady_tick(Duration::from_millis(80));
        }
        self.0.set_message(message.to_string());
    }

    /// Removes the spinner line.
    pub fn clear(&self) {
        self.0.finish_and_clear();
    }
}

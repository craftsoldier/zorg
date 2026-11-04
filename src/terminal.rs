//! zorg's terminal output for a person: confirmation cards and send progress.

use std::io::IsTerminal;
use std::time::{Duration, Instant};

use console::{style, Key, Term};
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};

/// Error for a confirmation answered with Esc or `n`; nothing was done.
pub const CANCELLED: &str = "\0cancelled";
/// Error for Ctrl-C at a confirmation; zorg exits with status 130.
pub const INTERRUPTED: &str = "\0interrupted";
/// Error for a failure already shown on screen; zorg exits with status 1.
pub const REPORTED: &str = "\0reported";

/// Keys pressed this soon after a card is drawn are ignored, so keys typed
/// ahead cannot answer a card that was not yet shown.
const TYPE_AHEAD_WINDOW: Duration = Duration::from_millis(250);

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

/// A card's Tab key: its label on the card, and what it changes before the card
/// is redrawn.
pub type TabKey<'a> = (&'a str, &'a mut dyn FnMut() -> Result<(), String>);

/// Shows the card `render(full)` returns and waits for one key. Enter or `y`
/// confirms. Esc or `n` fails with [`CANCELLED`], and Ctrl-C with
/// [`INTERRUPTED`]. When `address_len` is long, `x` redraws the card with
/// `full` toggled. With `tab` given as `(label, change)`, Tab runs `change` and
/// redraws the card. Keys pressed within 250 ms of the card being drawn are
/// ignored, so keys typed ahead cannot answer it.
pub fn confirm(
    action: &str,
    address_len: usize,
    mut tab: Option<TabKey<'_>>,
    render: impl Fn(bool) -> Vec<String>,
) -> Result<(), String> {
    let term = Term::stdout();
    let expandable = address_len > SHORT_ADDRESS_LEN;
    let tab_label = tab.as_ref().map(|(label, _)| label.to_string());
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
        if let Some(label) = &tab_label {
            keys.push_str(&format!("   {} {label}", style("[Tab]").bold()));
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
    let mut rows = draw(full)?;
    let mut drawn = Instant::now();
    let answer = loop {
        let key = term.read_key_raw().map_err(|e| e.to_string())?;
        if drawn.elapsed() < TYPE_AHEAD_WINDOW {
            continue;
        }
        match key {
            Key::Enter | Key::Char('y' | 'Y') => break Ok(()),
            Key::Escape | Key::Char('n' | 'N') => break Err(CANCELLED.to_string()),
            Key::CtrlC => break Err(INTERRUPTED.to_string()),
            Key::Char('x' | 'X') if expandable => {
                full = !full;
                term.clear_last_lines(rows).map_err(|e| e.to_string())?;
                rows = draw(full)?;
                drawn = Instant::now();
            }
            Key::Tab => {
                if let Some((_, change)) = tab.as_mut() {
                    term.clear_last_lines(rows).map_err(|e| e.to_string())?;
                    change()?;
                    rows = draw(full)?;
                    drawn = Instant::now();
                }
            }
            _ => {}
        }
    };
    let _ = term.clear_last_lines(1);
    let _ = term.show_cursor();
    answer
}

/// Asks `question` and reads a typed answer, echoing it. Enter accepts the
/// answer once `check` approves it; an empty answer is accepted only when
/// `optional`. A rejected answer shows the reason and asks again. Esc fails with
/// [`CANCELLED`], and Ctrl-C with [`INTERRUPTED`].
pub fn ask(
    question: &str,
    optional: bool,
    check: impl Fn(&str) -> Result<(), String>,
) -> Result<String, String> {
    let term = Term::stdout();
    let io = |e: std::io::Error| e.to_string();
    let prompt = format!(
        "{} {} ",
        style("?").yellow().bold(),
        style(format!("{question}:")).bold()
    );
    let prompt_width = console::measure_text_width(&prompt);
    let width = usize::from(term.size().1).max(1);
    let rows = |text: &str| {
        (prompt_width + console::measure_text_width(text))
            .div_ceil(width)
            .max(1)
    };
    let erase = |shown: usize| -> Result<(), String> {
        term.clear_line().map_err(io)?;
        for _ in 1..shown {
            term.move_cursor_up(1).map_err(io)?;
            term.clear_line().map_err(io)?;
        }
        Ok(())
    };
    let mut answer = String::new();
    term.write_str(&prompt).map_err(io)?;
    loop {
        let before = rows(&answer);
        match term.read_key_raw().map_err(io)? {
            Key::Char(c) if !c.is_control() => answer.push(c),
            Key::Backspace => {
                answer.pop();
            }
            Key::Enter => {
                let text = answer.trim();
                let verdict = match (text.is_empty(), optional) {
                    (true, true) => Ok(()),
                    (true, false) => Err("An answer is needed".to_string()),
                    (false, _) => check(text),
                };
                erase(before)?;
                match verdict {
                    Ok(()) => {
                        let shown = if text.is_empty() { "–" } else { text };
                        term.write_line(&format!(
                            "{} {} {}",
                            style("✔").green(),
                            style(question).bold(),
                            style(format!("· {shown}")).dim()
                        ))
                        .map_err(io)?;
                        return Ok(text.to_string());
                    }
                    Err(reason) => {
                        term.write_line(&format!("{} {reason}", style("✗").red().bold()))
                            .map_err(io)?;
                        answer.clear();
                        term.write_str(&prompt).map_err(io)?;
                    }
                }
                continue;
            }
            Key::Escape => {
                term.write_line("").map_err(io)?;
                return Err(CANCELLED.to_string());
            }
            Key::CtrlC => {
                term.write_line("").map_err(io)?;
                return Err(INTERRUPTED.to_string());
            }
            _ => continue,
        }
        erase(before)?;
        term.write_str(&format!("{prompt}{answer}")).map_err(io)?;
    }
}

/// Shows `items` as a list to choose from with the arrow keys, starting at
/// `default`, and returns the chosen index. Esc fails with [`CANCELLED`].
pub fn pick(question: &str, items: &[String], default: usize) -> Result<usize, String> {
    let term = Term::stdout();
    let choice = dialoguer::Select::with_theme(&dialoguer::theme::ColorfulTheme::default())
        .with_prompt(format!(
            "{question}  {}",
            style("↑↓ move · Enter pick · Esc cancel").dim()
        ))
        .items(items)
        .default(default)
        .report(false)
        .interact_on_opt(&term)
        .map_err(|e| e.to_string())?;
    let Some(index) = choice else {
        return Err(CANCELLED.to_string());
    };
    term.write_line(&format!(
        "{} {} {}",
        style("✔").green(),
        style(question).bold(),
        style(format!(
            "· {}",
            console::strip_ansi_codes(&items[index]).trim()
        ))
        .dim()
    ))
    .map_err(|e| e.to_string())?;
    Ok(index)
}

/// Shows `items` as a menu to choose from with the arrow keys, starting at
/// `default`, with `leave` ("q quit" or "Esc back") in the hint. Returns the
/// chosen index, or none for q or Esc. The menu is cleared after a choice.
pub fn menu(leave: &str, items: &[String], default: usize) -> Result<Option<usize>, String> {
    dialoguer::Select::with_theme(&dialoguer::theme::ColorfulTheme::default())
        .with_prompt(
            style(format!("↑↓ move · Enter pick · {leave}"))
                .dim()
                .to_string(),
        )
        .items(items)
        .default(default)
        .max_length(
            usize::from(Term::stdout().size().0)
                .saturating_sub(6)
                .max(1),
        )
        .report(false)
        .interact_on_opt(&Term::stdout())
        .map_err(|e| e.to_string())
}

/// Shows `items` as a menu like [`menu`], with `leave` in the hint, where
/// pressing the `i`th of `keys` answers `items.len() + i`, as if it were one
/// more item, named in the hint.
/// While it waits for a key it calls `idle` about every 100 ms.
/// q or Esc answers none; Ctrl-C is [`INTERRUPTED`]. Lines are cut to the
/// terminal's width, and the menu is cleared after.
pub fn menu_with_keys(
    leave: &str,
    items: &[String],
    default: usize,
    keys: &[(char, &str)],
    idle: &mut dyn FnMut(),
) -> Result<Option<usize>, String> {
    let term = Term::stdout();
    let width = usize::from(term.size().1).saturating_sub(2);
    let fit = |text: &str| console::truncate_str(text, width, "…").into_owned();
    let named: String = keys
        .iter()
        .map(|(key, name)| format!("{key} {name} · "))
        .collect();
    let hint = fit(&format!("{named}{leave} · ↑↓ · Enter"));
    let items: Vec<String> = items.iter().map(|item| fit(item)).collect();
    let draw = |current: usize| -> std::io::Result<()> {
        term.write_line(&style(&hint).dim().to_string())?;
        for (i, item) in items.iter().enumerate() {
            if i == current {
                term.write_line(&format!("{} {}", style("❯").cyan(), style(item).cyan()))?;
            } else {
                term.write_line(&format!("  {item}"))?;
            }
        }
        Ok(())
    };
    let mut current = default;
    term.hide_cursor().map_err(|e| e.to_string())?;
    draw(current).map_err(|e| e.to_string())?;
    let answer = loop {
        let answer = match read_key_while(idle)? {
            Key::ArrowUp => {
                current = (current + items.len() - 1) % items.len();
                None
            }
            Key::ArrowDown => {
                current = (current + 1) % items.len();
                None
            }
            Key::Enter => Some(Some(current)),
            Key::Escape | Key::Char('q') => Some(None),
            Key::Char(c) => keys
                .iter()
                .position(|(key, _)| *key == c)
                .map(|i| Some(items.len() + i)),
            Key::CtrlC => {
                let _ = term.show_cursor();
                return Err(INTERRUPTED.to_string());
            }
            _ => None,
        };
        term.clear_last_lines(items.len() + 1)
            .map_err(|e| e.to_string())?;
        match answer {
            Some(answer) => break answer,
            None => draw(current).map_err(|e| e.to_string())?,
        }
    };
    term.show_cursor().map_err(|e| e.to_string())?;
    Ok(answer)
}

/// Waits for a key, calling `idle` about every 100 ms until one arrives. The
/// key is read on a short-lived thread that ends once it has the key, so no
/// read is left waiting to take a later screen's keys.
fn read_key_while(idle: &mut dyn FnMut()) -> Result<Key, String> {
    let (send_key, key) = std::sync::mpsc::channel();
    std::thread::spawn(move || send_key.send(Term::stdout().read_key_raw()));
    loop {
        match key.recv_timeout(Duration::from_millis(100)) {
            Ok(key) => return key.map_err(|e| e.to_string()),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => idle(),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err("Could not read a key".into())
            }
        }
    }
}

/// Clears the terminal.
pub fn clear() -> Result<(), String> {
    Term::stdout().clear_screen().map_err(|e| e.to_string())
}

/// Shows `label` and waits for Esc, Enter, or q. Ctrl-C is [`INTERRUPTED`].
pub fn wait_for(label: &str) -> Result<(), String> {
    let term = Term::stdout();
    term.write_line(&style(label).dim().to_string())
        .map_err(|e| e.to_string())?;
    loop {
        match term.read_key_raw().map_err(|e| e.to_string())? {
            Key::Escape | Key::Enter | Key::Char('q') => return Ok(()),
            Key::CtrlC => return Err(INTERRUPTED.to_string()),
            _ => {}
        }
    }
}

/// Puts `text` on the macOS clipboard.
pub fn copy(text: &str) -> Result<(), String> {
    use std::io::Write;
    let mut child = std::process::Command::new("pbcopy")
        .stdin(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("Could not copy: {e}"))?;
    let written = child
        .stdin
        .take()
        .map(|mut stdin| stdin.write_all(text.as_bytes()));
    let status = child.wait();
    match (written, status) {
        (Some(Ok(())), Ok(status)) if status.success() => Ok(()),
        _ => Err("Could not copy".into()),
    }
}

/// A bar `width` characters wide, filled to `fraction` (from 0 to 1).
pub fn bar(fraction: f64, width: usize) -> String {
    let filled = ((fraction.clamp(0.0, 1.0) * width as f64) as usize).min(width);
    format!(
        "{}{}",
        style("━".repeat(filled)).cyan(),
        style("─".repeat(width - filled)).dim()
    )
}

/// Rewrites line `row` of the screen, counting from 1 at the top, and puts
/// the cursor back where it was.
pub fn paint_line(row: usize, text: &str) {
    use std::io::Write;
    let mut out = std::io::stdout();
    let _ = out.write_all(format!("\x1b7\x1b[{row};1H\x1b[2K{text}\x1b8").as_bytes());
    let _ = out.flush();
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

    /// A progress bar out of 100 that draws nothing until [`Progress::show`].
    pub fn hidden_bar() -> Self {
        let bar = ProgressBar::with_draw_target(Some(100), ProgressDrawTarget::hidden());
        bar.set_style(
            ProgressStyle::with_template("{spinner:.cyan} {bar:30.cyan/dim} {pos:>3}%  {msg}")
                .expect("template")
                .progress_chars("━━─"),
        );
        Self(bar)
    }

    /// Moves a bar to `percent`, from 0 to 100.
    pub fn set_percent(&self, percent: u64) {
        self.0.set_position(percent);
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

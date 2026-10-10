use std::io::{self, IsTerminal, Write};

#[derive(Clone, Copy)]
enum Level {
    Info,
    Warn,
    Error,
}

impl Level {
    fn label(self) -> &'static str {
        match self {
            Self::Info => "::",
            Self::Warn => "warning:",
            Self::Error => "error:",
        }
    }

    fn color(self) -> &'static str {
        match self {
            Self::Info => "38;5;208",
            Self::Warn => "38;5;214",
            Self::Error => "38;5;196",
        }
    }
}

fn color_enabled(terminal: bool) -> bool {
    terminal
        && std::env::var_os("NO_COLOR").is_none()
        && std::env::var_os("TERM").is_none_or(|term| term != "dumb")
}

fn line(level: Level, message: &str, color: bool) -> String {
    let label = level.label();
    if color {
        let color = level.color();
        format!("\x1b[1;{color}m{label}\x1b[0m {message}")
    } else {
        format!("{label} {message}")
    }
}

pub fn step(message: impl AsRef<str>) {
    let color = color_enabled(io::stdout().is_terminal());
    for message in message.as_ref().split('\n') {
        println!("{}", line(Level::Info, message, color));
    }
}

fn diagnostic(level: Level, message: &str) {
    let color = color_enabled(io::stderr().is_terminal());
    for message in message.split('\n') {
        eprintln!("{}", line(level, message, color));
    }
}

fn emphasis(message: &str) -> String {
    if color_enabled(io::stdout().is_terminal()) {
        format!("\x1b[1;38;5;214m{message}\x1b[0m")
    } else {
        message.to_owned()
    }
}

pub fn item(message: impl AsRef<str>) {
    println!("  {}", emphasis(message.as_ref()));
}

pub fn plan(packages: &[String]) {
    let heading = format!("Packages ({})", packages.len());
    let indent = " ".repeat(heading.len() + 2);
    let mut width = heading.len();
    let columns = terminal_columns();
    print!("\n{}", emphasis(&heading));
    for package in packages {
        let length = package.chars().count();
        if width + 2 + length >= columns && width > heading.len() {
            print!("\n{indent}{}", emphasis(package));
            width = indent.len() + length;
        } else {
            print!("  {}", emphasis(package));
            width += 2 + length;
        }
    }
    println!("\n");
}

pub fn package(name: &str, version: &impl std::fmt::Display, description: &str, installed: bool) {
    let status = if installed { " [installed]" } else { "" };
    item(format!("{name}-{version}{status}"));
    if !description.is_empty() {
        println!("    {description}");
    }
}

pub fn progress(current: usize, total: usize, action: &str, name: &str) {
    println!(
        "({current}/{total}) {} {}",
        action.to_lowercase(),
        emphasis(name)
    );
}

pub fn confirm(noconfirm: bool) -> anyhow::Result<bool> {
    if noconfirm {
        return Ok(true);
    }
    print!(
        "{} ",
        line(
            Level::Info,
            &emphasis("Proceed? [Y/n]"),
            color_enabled(io::stdout().is_terminal())
        )
    );
    io::stdout().flush()?;
    let mut answer = String::new();
    let read = io::stdin().read_line(&mut answer)?;
    let confirmed = read > 0
        && matches!(
            answer.trim().to_ascii_lowercase().as_str(),
            "" | "y" | "yes"
        );
    if !io::stdin().is_terminal() || read == 0 {
        println!();
    }
    if !confirmed {
        if read == 0 {
            step("Cancelled: no confirmation received. Use --noconfirm to accept automatically.");
        } else {
            step("Cancelled; no package changes made.");
        }
    }
    Ok(confirmed)
}

pub fn warn(message: impl AsRef<str>) {
    diagnostic(Level::Warn, message.as_ref());
}

pub fn error(message: impl AsRef<str>) {
    diagnostic(Level::Error, message.as_ref());
}

fn size(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    for unit in ["KiB", "MiB", "GiB", "TiB"] {
        value /= 1024.0;
        if value < 1024.0 || unit == "TiB" {
            return format!("{value:.1} {unit}");
        }
    }
    unreachable!()
}

/// A download phase can discover additional dependencies as it runs.
#[derive(Default)]
pub struct DownloadBatch {
    completed: usize,
    bytes: u64,
    started: Option<std::time::Instant>,
}

impl DownloadBatch {
    pub fn next_position(&mut self) -> usize {
        if self.started.is_none() {
            self.started = Some(std::time::Instant::now());
            step("Downloading packages...");
        }
        self.completed + 1
    }

    pub fn completed(&mut self, bytes: u64) {
        self.completed += 1;
        self.bytes += bytes;
    }

    pub fn finish(&self) {
        if let Some(started) = self.started {
            step(format!(
                "Downloaded {} package{} ({}) in {:.1}s.",
                self.completed,
                if self.completed == 1 { "" } else { "s" },
                size(self.bytes),
                started.elapsed().as_secs_f64()
            ));
        }
    }
}

fn terminal_columns() -> usize {
    use std::os::fd::AsRawFd;
    let mut dimensions: libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: dimensions points to a writable winsize and stderr is a valid descriptor.
    if unsafe { libc::ioctl(io::stderr().as_raw_fd(), libc::TIOCGWINSZ, &mut dimensions) } == 0
        && dimensions.ws_col > 0
    {
        usize::from(dimensions.ws_col)
    } else {
        80
    }
}

fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }
    if width <= 3 {
        return ".".repeat(width);
    }
    format!("{}...", text.chars().take(width - 3).collect::<String>())
}

fn download_row(
    name: &str,
    received: u64,
    total: Option<u64>,
    speed: &str,
    columns: usize,
) -> String {
    // Reserve one column to avoid automatic line wrapping at the right edge.
    let available = columns.saturating_sub(":: ".len() + 1);
    let status = match total {
        Some(total) if total > 0 => {
            format!("{:3.0}%", (received as f64 / total as f64).min(1.0) * 100.0)
        }
        Some(_) => "100%".to_owned(),
        None => "...".to_owned(),
    };
    let ratio = total
        .filter(|total| *total > 0)
        .map(|total| (received as f64 / total as f64).min(1.0));
    let bar = ratio
        .map(|ratio| {
            let filled = (ratio * 12.0) as usize;
            format!("[{}{}] ", "=".repeat(filled), "-".repeat(12 - filled))
        })
        .unwrap_or_default();
    let detail = format!("{bar}{status}  {}  {speed}/s", size(received));
    let detail = if available >= detail.len() + 18 {
        detail
    } else {
        format!("{status}  {}", size(received))
    };
    let name_width = available.saturating_sub(detail.len() + 2);
    let message = format!("{}  {detail}", truncate(name, name_width));
    truncate(&message, available)
}

/// Counts bytes actually written, refreshing terminals at most ten times a second.
pub struct DownloadProgress {
    name: String,
    total: Option<u64>,
    received: u64,
    terminal: bool,
    last: std::time::Instant,
    started: std::time::Instant,
    width: std::cell::Cell<usize>,
    finished: bool,
}

impl DownloadProgress {
    pub fn new(name: String, total: Option<u64>) -> Self {
        let progress = Self {
            name,
            total,
            received: 0,
            terminal: io::stderr().is_terminal()
                && std::env::var_os("TERM").is_none_or(|term| term != "dumb"),
            last: std::time::Instant::now(),
            started: std::time::Instant::now(),
            width: std::cell::Cell::new(0),
            finished: false,
        };
        if progress.terminal {
            progress.render();
        }
        progress
    }

    pub fn advance(&mut self, bytes: usize) {
        self.received += bytes as u64;
        if self.terminal && self.last.elapsed() >= std::time::Duration::from_millis(100) {
            self.render();
            self.last = std::time::Instant::now();
        }
    }

    fn render(&self) {
        let elapsed = self.started.elapsed().as_secs_f64();
        let speed = size((self.received as f64 / elapsed.max(0.001)) as u64);
        let columns = terminal_columns();
        let message = download_row(&self.name, self.received, self.total, &speed, columns);
        let plain_width = line(Level::Info, &message, false).chars().count();
        let padding = self
            .width
            .get()
            .min(columns.saturating_sub(1))
            .saturating_sub(plain_width);
        self.width.set(plain_width);
        eprint!(
            "\r{}{}",
            line(Level::Info, &message, color_enabled(self.terminal)),
            " ".repeat(padding)
        );
        let _ = io::stderr().flush();
    }

    fn clear(&self) {
        eprint!(
            "\r{}\r",
            " ".repeat(self.width.get().min(terminal_columns().saturating_sub(1)))
        );
        let _ = io::stderr().flush();
    }

    pub fn finish(&mut self) {
        if self.terminal {
            self.clear();
        } else {
            step(format!(
                "Downloaded {} ({})",
                self.name,
                size(self.received)
            ));
        }
        self.finished = true;
    }
}

impl Drop for DownloadProgress {
    fn drop(&mut self) {
        if self.terminal && !self.finished {
            self.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn download_rows_fit_narrow_terminals_and_keep_full_names_in_wide_ones() {
        let name = "[12] a-package-with-a-very-long-name-1.2.3";
        for columns in [20, 40, 60, 80, 120] {
            for total in [None, Some(0), Some(10 * 1024 * 1024)] {
                let row = download_row(name, 1024 * 1024, total, "2.0 MiB", columns);
                assert!(line(Level::Info, &row, false).chars().count() < columns);
                assert!(!row.contains('\n'));
            }
        }
        let row = download_row(name, 1024, Some(2048), "1.0 KiB", 120);
        assert!(row.contains(name));
        assert!(row.contains("50%"));
        assert!(row.contains("1.0 KiB/s"));
        assert!(!download_row(name, 1024, None, "1.0 KiB", 120).contains('%'));
    }
}

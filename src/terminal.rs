//! Terminal adapter for the reusable pipeline's progress events.

use std::cell::RefCell;

use indicatif::{ProgressBar, ProgressStyle};

use crate::progress::{ProgressEvent, ProgressObserver, ProgressUnit, SourceKind};
use crate::textsafe::sanitize;

#[derive(Default)]
pub struct TerminalProgress {
    bar: RefCell<Option<ProgressBar>>,
}

impl ProgressObserver for TerminalProgress {
    fn on_event(&self, event: ProgressEvent<'_>) {
        match event {
            ProgressEvent::IndexStarted {
                source,
                destination,
                kind,
            } => {
                match kind {
                    SourceKind::Archive(fmt) => println!("Archive : {} ({fmt})", source.display()),
                    SourceKind::Directory => println!("Source  : {} (directory)", source.display()),
                }
                println!("Index   : {}", destination.display());
                println!();
            }
            ProgressEvent::Total { amount, unit } => {
                let pb = ProgressBar::new(amount);
                let template = match unit {
                    ProgressUnit::Bytes => "{spinner:.cyan} [{elapsed_precise}] [{bar:45.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}, eta {eta}) — {msg}",
                    ProgressUnit::Files => "{spinner:.cyan} [{elapsed_precise}] [{bar:45.cyan/blue}] {pos}/{len} files ({per_sec}, eta {eta}) — {msg}",
                };
                pb.set_style(
                    ProgressStyle::with_template(template)
                        .unwrap_or_else(|_| ProgressStyle::default_bar())
                        .progress_chars("=>-"),
                );
                *self.bar.borrow_mut() = Some(pb);
            }
            ProgressEvent::Advanced { amount } => {
                if let Some(pb) = self.bar.borrow().as_ref() {
                    pb.inc(amount);
                }
            }
            ProgressEvent::Entry { path, number } => {
                if number % 64 == 1 {
                    if let Some(pb) = self.bar.borrow().as_ref() {
                        pb.set_message(sanitize(&truncate_path(path, 50)).into_owned());
                    }
                }
            }
            ProgressEvent::Warning { message } => {
                let render = || eprintln!("{}", sanitize(message));
                match self.bar.borrow().as_ref() {
                    Some(pb) => pb.suspend(render),
                    None => render(),
                }
            }
            ProgressEvent::IndexDiscovered { path } => eprintln!(
                "hint: using index '{}' — pass --index to be explicit",
                sanitize(&path.display().to_string())
            ),
            ProgressEvent::ReadyToPromote => {
                if let Some(pb) = self.bar.borrow().as_ref() {
                    pb.finish_with_message("done");
                }
            }
            ProgressEvent::Finished => {}
        }
    }
}

/// Truncate a path to `max_chars` characters, keeping the tail visible.
fn truncate_path(s: &str, max_chars: usize) -> String {
    let count = s.chars().count();
    if count <= max_chars {
        return s.to_string();
    }
    let tail: String = s.chars().skip(count + 1 - max_chars).collect();
    format!("…{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_keeps_tail() {
        assert_eq!(truncate_path("short", 10), "short");
        let t = truncate_path("a/very/long/path/to/some/file.txt", 12);
        assert_eq!(t.chars().count(), 12);
        assert!(t.starts_with('…'));
        assert!(t.ends_with("file.txt"));
    }
}

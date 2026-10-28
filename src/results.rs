use crate::{
    kartouche::{self, Outcome, Style},
    ui::{Color, stderr_color},
};
use std::io::{self, Write};

#[derive(Clone, Copy)]
pub(crate) enum ResultKind {
    Done,
    Unchanged,
    Failed,
    Skipped,
    Unreachable,
    Requested,
    Unknown,
}

impl ResultKind {
    fn label(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Unchanged => "up to date (no reboot)",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
            Self::Unreachable => "unreachable",
            Self::Requested => "request sent (not verified)",
            Self::Unknown => "shutdown outcome unknown",
        }
    }

    fn color(self) -> Color {
        match self {
            Self::Done | Self::Unchanged => Color::Green,
            Self::Skipped | Self::Requested => Color::Yellow,
            Self::Failed | Self::Unreachable | Self::Unknown => Color::Red,
        }
    }
}

pub(crate) fn print_results(results: &[(String, ResultKind)]) {
    let rows = result_items(results);
    let items: Vec<_> = rows
        .iter()
        .map(|(label, outcome)| (label.as_str(), *outcome))
        .collect();
    io::stdout().flush().expect("failed to flush host output");
    kartouche::summary(Style::Rail, &items);
}

fn result_items(results: &[(String, ResultKind)]) -> Vec<(String, Outcome)> {
    results
        .iter()
        .map(|(name, result)| {
            let outcome = match result {
                ResultKind::Done | ResultKind::Unchanged => Outcome::Ok,
                ResultKind::Skipped | ResultKind::Unreachable | ResultKind::Requested => {
                    Outcome::Skipped
                }
                ResultKind::Failed | ResultKind::Unknown => Outcome::Failed,
            };
            (
                format!("{name}: {}", stderr_color(result.label(), result.color())),
                outcome,
            )
        })
        .collect()
}

fn unreachable_warning(count: usize) -> String {
    let host = if count == 1 { "host" } else { "hosts" };
    format!(
        "Warning: {count} unreachable {host}. If intentionally shut down, this is expected.\nOtherwise, check power and SSH/network connectivity."
    )
}

pub(crate) fn report_attention(results: &[(String, ResultKind)]) -> Result<(), String> {
    let unreachable = results
        .iter()
        .filter(|(_, result)| matches!(result, ResultKind::Unreachable))
        .count();
    let skipped = results
        .iter()
        .filter(|(_, result)| matches!(result, ResultKind::Skipped))
        .count();
    let failed = results
        .iter()
        .filter(|(_, result)| matches!(result, ResultKind::Failed | ResultKind::Unknown))
        .count();
    if unreachable > 0 {
        eprintln!(
            "{}",
            stderr_color(&unreachable_warning(unreachable), Color::Yellow)
        );
    }
    if skipped > 0 {
        let host = if skipped == 1 { "host" } else { "hosts" };
        eprintln!(
            "{}",
            stderr_color(
                &format!("Warning: {skipped} skipped {host}."),
                Color::Yellow
            )
        );
    }
    if failed > 0 {
        return Err(format!(
            "{failed} host(s) failed or have an unknown outcome"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn summary_rail_distinguishes_success_from_incomplete_and_failed_outcomes() {
        let outcomes = vec![
            ("ok".into(), ResultKind::Done),
            ("unchanged".into(), ResultKind::Unchanged),
            ("skipped".into(), ResultKind::Skipped),
            ("offline".into(), ResultKind::Unreachable),
            ("shutdown".into(), ResultKind::Requested),
            ("failed".into(), ResultKind::Failed),
            ("unknown".into(), ResultKind::Unknown),
        ];
        let rows = result_items(&outcomes);
        let items: Vec<_> = rows
            .iter()
            .map(|(label, outcome)| (label.as_str(), *outcome))
            .collect();
        let rendered = kartouche::render_summary(
            Style::Rail,
            "fleetty",
            env!("CARGO_PKG_VERSION"),
            &items,
            kartouche::Stream::Stderr,
        );
        // Tests can run from a terminal, where kartouche emits ANSI styles.
        let mut rail = String::new();
        let mut chars = rendered.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch == '\x1b' && chars.next_if_eq(&'[').is_some() {
                for code in chars.by_ref() {
                    if code == 'm' {
                        break;
                    }
                }
            } else {
                rail.push(ch);
            }
        }
        let (header, branch, yes, pending, no, tail, dot) =
            if matches!(std::env::var("TERM").as_deref(), Ok("linux" | "dumb")) {
                (",- *", "|-", "+", "o", "x", "`-", ".")
            } else {
                ("╭─ ●", "├─", "✓", "○", "✗", "╰─", "·")
            };
        assert_eq!(
            rail.lines().next(),
            Some(format!("{header} fleetty  v{}", env!("CARGO_PKG_VERSION")).as_str())
        );
        assert_eq!(rail.lines().count(), outcomes.len() + 2);
        for line in [
            format!("{branch} {yes} ok: done"),
            format!("{branch} {yes} unchanged: up to date (no reboot)"),
            format!("{branch} {pending} skipped: skipped"),
            format!("{branch} {pending} offline: unreachable"),
            format!("{branch} {pending} shutdown: request sent (not verified)"),
            format!("{branch} {no} failed: failed"),
            format!("{branch} {no} unknown: shutdown outcome unknown"),
            format!("{tail} {no} 2 ok {dot} 2 failed {dot} 3 skipped"),
        ] {
            assert!(
                rail.lines().any(|actual| actual == line),
                "missing {line} in {rail}"
            );
        }
    }
    #[test]
    fn unreachable_hosts_are_warnings_but_real_failures_are_errors() {
        assert!(unreachable_warning(1).contains("1 unreachable host."));
        assert!(unreachable_warning(2).contains("2 unreachable hosts."));
        assert!(unreachable_warning(1).contains("If intentionally shut down"));
        assert!(
            unreachable_warning(1)
                .contains("\nOtherwise, check power and SSH/network connectivity.")
        );
        let offline = vec![("powered off".into(), ResultKind::Unreachable)];
        assert!(report_attention(&offline).is_ok());
        let failed = vec![("update failed".into(), ResultKind::Failed)];
        assert!(report_attention(&failed).is_err());
        let skipped = vec![("pacman-only update declined".into(), ResultKind::Skipped)];
        assert!(report_attention(&skipped).is_ok());
        let unknown = vec![("shutdown outcome unknown".into(), ResultKind::Unknown)];
        assert!(report_attention(&unknown).is_err());
    }
}

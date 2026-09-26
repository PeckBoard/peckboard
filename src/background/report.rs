//! The text injected into a session when one of its background tasks ends.

use super::{TaskInfo, TaskStatus};

/// `3m12s`-style duration.
pub fn fmt_duration(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}h{m}m{s}s")
    } else if m > 0 {
        format!("{m}m{s}s")
    } else {
        format!("{s}s")
    }
}

/// `program arg1 arg2` for display.
pub fn display_command(program: &str, args: &[String]) -> String {
    if args.is_empty() {
        program.to_string()
    } else {
        format!("{program} {}", args.join(" "))
    }
}

/// The one-line headline, e.g.
/// `[background task "build" (id) finished: exit 0 after 3m12s]`.
pub fn headline(info: &TaskInfo, elapsed_secs: u64) -> String {
    let after = fmt_duration(elapsed_secs);
    let outcome = match info.status {
        TaskStatus::Running => format!("still running after {after}"),
        TaskStatus::Succeeded => format!(
            "finished: exit {} after {after}",
            info.exit_code.unwrap_or(0)
        ),
        TaskStatus::Failed => match (info.exit_code, info.signal) {
            (Some(code), _) => format!("FAILED: exit {code} after {after}"),
            (None, Some(sig)) => format!("FAILED: killed by signal {sig} after {after}"),
            (None, None) => format!("FAILED after {after}"),
        },
        TaskStatus::TimedOut => format!(
            "TIMED OUT after {after} (timeout {})",
            fmt_duration(info.timeout_secs)
        ),
        TaskStatus::Stopped => format!("STOPPED after {after}"),
    };
    format!(
        "[background task \"{}\" ({}) {outcome}]",
        info.label, info.id
    )
}

/// Full report: headline, the command, the last output lines, the log path.
pub fn compose(info: &TaskInfo, elapsed_secs: u64, last_lines: &[String]) -> String {
    let mut out = headline(info, elapsed_secs);
    out.push_str("\n\n$ ");
    out.push_str(&display_command(&info.program, &info.args));
    out.push('\n');
    if last_lines.is_empty() {
        out.push_str("\n(no output)\n");
    } else {
        out.push_str(&format!(
            "\nLast {} line(s) of output:\n```\n",
            last_lines.len()
        ));
        for l in last_lines {
            out.push_str(l);
            out.push('\n');
        }
        out.push_str("```\n");
    }
    out.push_str(&format!("\nFull log: {}", info.log_path));
    if info.log_truncated {
        out.push_str(" (truncated at the size cap)");
    }
    out.push_str(" \u{2014} read more with background_status.");
    out
}

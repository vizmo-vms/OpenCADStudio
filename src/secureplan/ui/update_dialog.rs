//! The **Update available** dialog (DSK-07, DSK-06).
//!
//! Shows the installed build (version, bridge protocol, fork commit), the new
//! version and its release notes, with **Update** and **Later**. It opens
//! only when the user asks (SecurePlan › Check for updates); an automatic
//! check announces an update on the command line and in the window title and
//! never interrupts work. Keyboard: as every SecurePlan dialog (Tab, the
//! arrows, Enter, Space, Escape, "›" on the focused button).

use iced::widget::{column, container, scrollable, text};
use iced::{Element, Length};

use super::{form_view, keys_hint, Action, Form};
use crate::app::secureplan::update::{installed_line, Release};
use crate::app::Message;

/// At most this many lines of notes, each at most `MAX_LINE` characters.
const MAX_LINES: usize = 40;
const MAX_LINE: usize = 160;

#[derive(Debug, Clone)]
pub struct UpdateDialog {
    pub form: Form,
    pub release: Release,
    pub notes: Vec<String>,
    /// Notes were cut to fit.
    pub truncated: bool,
}

impl UpdateDialog {
    pub fn new(release: Release) -> Self {
        let (notes, truncated) = notes_lines(&release.notes);
        let buttons = vec![("Update".to_string(), Action::UpdateStart), ("Later".to_string(), Action::Dismiss)];
        Self { form: Form::new(Vec::new(), buttons, Action::Dismiss), release, notes, truncated }
    }
}

/// Release notes as plain lines: Markdown heading and emphasis markers
/// dropped, runs of blank lines collapsed, and cut to fit the dialog.
pub fn notes_lines(body: &str) -> (Vec<String>, bool) {
    let mut lines: Vec<String> = Vec::new();
    let mut truncated = false;
    for raw in body.lines() {
        let line = raw.trim_end().trim_start_matches('#').trim_start().replace("**", "").replace('`', "");
        let line = match line.strip_prefix("* ") {
            Some(rest) => format!("- {rest}"),
            None => line,
        };
        if line.is_empty() && lines.last().is_none_or(|last| last.is_empty()) {
            continue;
        }
        if lines.len() == MAX_LINES {
            truncated = true;
            break;
        }
        let mut chars = line.chars();
        let mut kept: String = chars.by_ref().take(MAX_LINE).collect();
        if chars.next().is_some() {
            kept.push('…');
            truncated = true;
        }
        lines.push(kept);
    }
    while lines.last().is_some_and(|last| last.is_empty()) {
        lines.pop();
    }
    (lines, truncated)
}

pub fn view(dialog: &UpdateDialog) -> Element<'_, Message> {
    let mut notes = column![].spacing(2);
    if dialog.notes.is_empty() {
        notes = notes.push(text("This release has no notes.").size(12));
    }
    for line in &dialog.notes {
        notes = notes.push(text(line.as_str()).size(12));
    }
    if dialog.truncated {
        notes = notes.push(text("The full notes are on the SecurePlan CAD releases page.").size(12));
    }
    let published = super::format_unix_time(dialog.release.published);
    column![
        text(format!("SecurePlan CAD {} is available (published {published}).", dialog.release.version)).size(14),
        text(installed_line()).size(12),
        text("What's new").size(13),
        container(scrollable(container(notes).padding(6)).height(Length::Fixed(220.0))).width(Length::Fixed(520.0)),
        text("Update downloads the new version and checks it against the release's SHA-256 checksums. You can keep working; you are asked before it installs.").size(12).width(Length::Fixed(520.0)),
        form_view(&dialog.form),
        keys_hint(),
    ]
    .spacing(10)
    .padding(8)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::secureplan::ui::DialogKey;
    use crate::app::secureplan::update::Asset;

    fn release(notes: &str) -> Release {
        Release {
            version: "0.2.0".into(),
            tag: "secureplan-cad-v0.2.0".into(),
            notes: notes.into(),
            published: 1_790_380_800,
            asset: Asset { name: "SecurePlanCAD-macos-arm64.dmg".into(), size: 1 },
        }
    }

    #[test]
    fn notes_read_as_plain_lines_and_long_notes_are_cut() {
        let (lines, truncated) = notes_lines("## What's new\r\n\r\n\r\n* **Faster** `Apply`\n- Fixes\n\n");
        assert_eq!(lines, ["What's new", "", "- Faster Apply", "- Fixes"]);
        assert!(!truncated);
        let long = (0..100).map(|n| format!("line {n}")).collect::<Vec<_>>().join("\n");
        let (lines, truncated) = notes_lines(&long);
        assert_eq!(lines.len(), MAX_LINES);
        assert!(truncated);
        let (lines, truncated) = notes_lines(&"é".repeat(500));
        assert_eq!(lines[0].chars().count(), MAX_LINE + 1);
        assert!(truncated);
    }

    #[test]
    fn the_keyboard_chooses_update_or_later() {
        let mut dialog = UpdateDialog::new(release("notes"));
        assert_eq!(dialog.form.key(DialogKey::Activate), Some(Action::UpdateStart), "focus starts on Update");
        dialog.form.key(DialogKey::Next);
        assert_eq!(dialog.form.key(DialogKey::Space), Some(Action::Dismiss));
        assert_eq!(dialog.form.key(DialogKey::Cancel), Some(Action::Dismiss));
    }
}

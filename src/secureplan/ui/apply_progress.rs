//! The Apply progress dialog (PUB-03, DSK-06).
//!
//! It opens when the user confirms the Apply dialog and stays open until the
//! Apply ends, listing its steps with their state in words:
//!
//! 1. Preparing the drawing
//! 2. Creating the PDF
//! 3. Creating the snap file
//! 4. Sending to SecurePlan, with the bytes sent of the total
//! 5. Confirm in SecurePlan (the web's `awaitingConfirmation`)
//! 6. Saving in SecurePlan: the web's saving steps, with uploading's progress
//! 7. Done: "Applied as version N", or why the Apply stopped, with Close.
//!
//! Cancel (and Escape) is offered only while steps 1–3 run: it stops the
//! build, sends nothing and leaves the drawing as it was. From step 4 on the
//! outputs are SecurePlan's to accept or refuse, so the dialog has no button
//! until the end.

use std::sync::Arc;

use iced::widget::{column, text};
use iced::{Element, Length};

use super::{Action, Form};
use crate::app::secureplan::publish::{BuildControl, BuildStage};
use crate::app::Message;

/// Where the Apply is.
#[derive(Debug, Clone, PartialEq)]
pub enum Phase {
    /// Building the outputs (steps 1–3; [`BuildControl::stage`] says which).
    Generating,
    /// Sending `total` bytes of outputs ([`BuildControl::sent`] so far).
    Sending { total: u64 },
    /// SecurePlan asks its user to confirm.
    Confirming,
    /// SecurePlan saves: `step` is its `applyProgress` step.
    Saving { step: String, progress: Option<f64> },
    /// Committed as plan version `version`.
    Applied { version: Option<u64> },
    /// Stopped: why, and what happens to the edits.
    Stopped { title: String, lines: Vec<String> },
}

#[derive(Debug, Clone)]
pub struct ApplyProgress {
    pub tab_id: u64,
    pub control: Arc<BuildControl>,
    pub phase: Phase,
    pub form: Form,
}

impl ApplyProgress {
    pub fn new(tab_id: u64, control: Arc<BuildControl>) -> Self {
        let mut dialog = Self { tab_id, control, phase: Phase::Generating, form: Form::new(Vec::new(), Vec::new(), Action::Dismiss) };
        dialog.set_phase(Phase::Generating);
        dialog
    }

    /// Whether the Apply has ended (the dialog offers Close).
    pub fn finished(&self) -> bool {
        matches!(self.phase, Phase::Applied { .. } | Phase::Stopped { .. })
    }

    pub fn set_phase(&mut self, phase: Phase) {
        self.phase = phase;
        let (buttons, cancel) = match self.phase {
            Phase::Generating => (vec![("Cancel".to_string(), Action::ApplyProgressCancel)], Action::ApplyProgressCancel),
            Phase::Applied { .. } | Phase::Stopped { .. } => (vec![("Close".to_string(), Action::ApplyProgressClose)], Action::ApplyProgressClose),
            // Escape is ignored while SecurePlan has the outputs.
            _ => (Vec::new(), Action::ApplyProgressCancel),
        };
        self.form = Form::new(Vec::new(), buttons, cancel);
    }

    /// End with an error or a cancel reported by SecurePlan, or a lost session.
    pub fn stop(&mut self, title: &str, lines: Vec<String>) {
        self.set_phase(Phase::Stopped { title: title.to_string(), lines });
    }

    pub fn title(&self) -> &str {
        match &self.phase {
            Phase::Applied { .. } => "Applied to SecurePlan",
            Phase::Stopped { title, .. } => title,
            _ => "Applying to SecurePlan",
        }
    }

    /// The step list, each with its state in words.
    pub fn lines(&self) -> Vec<String> {
        let current = match &self.phase {
            Phase::Generating => match self.control.stage() {
                BuildStage::Drawing => 0,
                BuildStage::Pdf => 1,
                BuildStage::Snap => 2,
            },
            Phase::Sending { .. } => 3,
            Phase::Confirming => 4,
            Phase::Saving { .. } => 5,
            Phase::Applied { .. } => 6,
            Phase::Stopped { .. } => return self.stopped_lines(),
        };
        let mib = |bytes: u64| format!("{:.1} MiB", bytes as f64 / 1_048_576.0);
        let sending = match &self.phase {
            Phase::Sending { total } => format!(": {} of {}", mib(self.control.sent().min(*total)), mib(*total)),
            _ => String::new(),
        };
        let saving = match &self.phase {
            Phase::Saving { step, progress } => {
                let what = match step.as_str() {
                    "savingDraft" => "saving the design",
                    "creatingRevision" => "creating a revision",
                    "uploading" => "uploading",
                    _ => "committing",
                };
                match progress {
                    Some(p) => format!(": {what}, {:.0}%", (p * 100.0).clamp(0.0, 100.0)),
                    None => format!(": {what}"),
                }
            }
            _ => String::new(),
        };
        let steps = [
            "Preparing the drawing".to_string(),
            "Creating the PDF".to_string(),
            "Creating the snap file".to_string(),
            format!("Sending to SecurePlan{sending}"),
            "Confirm in SecurePlan".to_string(),
            format!("Saving in SecurePlan{saving}"),
        ];
        let mut lines: Vec<String> = steps
            .iter()
            .enumerate()
            .map(|(i, step)| {
                let state = match i.cmp(&current) {
                    std::cmp::Ordering::Less => "done",
                    std::cmp::Ordering::Equal if i == 4 => "waiting for you in SecurePlan",
                    std::cmp::Ordering::Equal => "in progress",
                    std::cmp::Ordering::Greater => "to do",
                };
                format!("{}. {step} — {state}", i + 1)
            })
            .collect();
        match &self.phase {
            Phase::Applied { version } => {
                lines.push(match version {
                    Some(version) => format!("Applied as version {version}."),
                    None => "Applied.".to_string(),
                });
            }
            Phase::Generating => lines.push("You can cancel until the outputs are sent; nothing is sent then and the drawing stays as it is.".into()),
            _ => lines.push("SecurePlan has the outputs; the Apply ends there.".into()),
        }
        lines
    }

    fn stopped_lines(&self) -> Vec<String> {
        match &self.phase {
            Phase::Stopped { lines, .. } => lines.clone(),
            _ => Vec::new(),
        }
    }
}

pub fn view(dialog: &ApplyProgress) -> Element<'_, Message> {
    let mut content = column![].spacing(8).padding(8);
    for line in dialog.lines() {
        content = content.push(text(line).size(13).width(Length::Fixed(480.0)));
    }
    content = content.push(super::form_view(&dialog.form));
    let keys = if dialog.finished() {
        "Enter or Escape closes."
    } else if dialog.phase == Phase::Generating {
        "Enter or Escape cancels."
    } else {
        "The dialog closes when SecurePlan has answered."
    };
    content.push(text(keys).size(12)).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::secureplan::ui::DialogKey;

    #[test]
    fn the_steps_follow_the_apply_and_cancel_only_while_generating() {
        let control = Arc::new(BuildControl::default());
        let mut dialog = ApplyProgress::new(1, Arc::clone(&control));
        let lines = dialog.lines();
        assert_eq!(lines[0], "1. Preparing the drawing — in progress");
        assert_eq!(lines[1], "2. Creating the PDF — to do");
        assert_eq!(dialog.form.key(DialogKey::Cancel), Some(Action::ApplyProgressCancel));
        assert_eq!(dialog.form.key(DialogKey::Activate), Some(Action::ApplyProgressCancel), "Cancel is the one button");
        dialog.set_phase(Phase::Sending { total: 3 * 1_048_576 });
        control.sent.store(1_048_576, std::sync::atomic::Ordering::Relaxed);
        let lines = dialog.lines();
        assert_eq!(lines[2], "3. Creating the snap file — done");
        assert_eq!(lines[3], "4. Sending to SecurePlan: 1.0 MiB of 3.0 MiB — in progress");
        assert!(dialog.form.buttons.is_empty() && dialog.form.key(DialogKey::Activate).is_none(), "no button while SecurePlan has the outputs");
        dialog.set_phase(Phase::Confirming);
        assert_eq!(dialog.lines()[4], "5. Confirm in SecurePlan — waiting for you in SecurePlan");
        dialog.set_phase(Phase::Saving { step: "uploading".into(), progress: Some(0.42) });
        assert_eq!(dialog.lines()[5], "6. Saving in SecurePlan: uploading, 42% — in progress");
        dialog.set_phase(Phase::Applied { version: Some(7) });
        assert_eq!(dialog.title(), "Applied to SecurePlan");
        assert!(dialog.lines().iter().any(|line| line == "Applied as version 7."));
        assert!(dialog.lines()[..6].iter().all(|line| line.ends_with("— done")));
        assert_eq!(dialog.form.key(DialogKey::Cancel), Some(Action::ApplyProgressClose));
        dialog.stop("Not applied", vec!["The Apply was cancelled in SecurePlan.".into()]);
        assert_eq!((dialog.title(), dialog.lines()), ("Not applied", vec!["The Apply was cancelled in SecurePlan.".to_string()]));
        assert_eq!(dialog.form.key(DialogKey::Activate), Some(Action::ApplyProgressClose));
    }
}

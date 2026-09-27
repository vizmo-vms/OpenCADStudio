//! SecurePlan dialogs. Each must be keyboard operable with visible focus,
//! text labels and non-colour status cues (DSK-06).
//!
//! Keyboard, in every dialog: Tab and Shift+Tab (or the Up and Down arrows)
//! move focus; Left and Right change a choice; typing edits a number field and
//! Backspace deletes; Enter activates the focused button (in a field it
//! activates the first button); Space activates a button or changes a choice;
//! Escape cancels. The focused control has a thick outline and a "›" marker,
//! so focus shows without relying on colour.

pub mod align_dialog;
pub mod apply_dialog;
pub mod convert_menu;
pub mod export_dialog;
pub mod import_dialog;
pub mod trust_dialog;
pub mod update_dialog;

use iced::widget::{button, column, container, row, text, text_input};
use iced::{Background, Border, Element, Length, Theme, Vector};

use crate::app::secureplan::bridge::SessionId;
use crate::app::secureplan::Msg;
use crate::app::Message;
pub use trust_dialog::DialogKey;

/// Vizmo's Signal Teal, the accent of the home screen and the SecurePlan
/// dialogs (DSK-08). Focus is never shown by colour alone.
pub const ACCENT: iced::Color = iced::Color::from_rgb8(0x00, 0x94, 0xA1);

/// What a dialog button does.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Dismiss,
    /// Cancel the import running in this tab.
    CancelLoad(u64),
    /// Close a bound tab: open Apply first, discard the work, or keep a
    /// recovery copy (DSK-03).
    CloseApply(u64),
    CloseDiscard(u64),
    CloseKeep(u64),
    RecoveryRestore(u64),
    RecoveryDiscard(u64),
    /// Answer a re-pair to an open document (BRG-05).
    Repair(SessionId, bool),
    AlignConfirm,
    ApplyConfirm,
    ApplyReset,
    /// Convert tab `.0`'s selection (CNV-01).
    Convert(u64, crate::app::secureplan::convert::Kind),
    /// The export dialog's **Export…** and Cancel (EXP-03).
    ExportSave,
    ExportCancel,
    /// Run a SecurePlan ribbon command (the keyboard menu of the tab).
    Command(&'static str),
    /// Replace tab `.0`'s drawing: choose the file now (PUB-01).
    ImportReplace(u64),
    /// The updater (DSK-07): **Update**, keep recovery copies and update,
    /// Apply tab `.0` first, **Install and restart**, and cancel a download.
    UpdateStart,
    UpdateKeepAndStart,
    UpdateApplyFirst(u64),
    UpdateInstall,
    UpdateCancelDownload,
}

/// A form field.
#[derive(Debug, Clone, PartialEq)]
pub enum FieldKind {
    Choice { options: Vec<String>, selected: usize },
    Number { text: String },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    pub label: &'static str,
    pub kind: FieldKind,
    pub enabled: bool,
}

impl Field {
    pub fn choice(label: &'static str, options: Vec<String>, selected: usize) -> Self {
        Self { label, kind: FieldKind::Choice { options, selected }, enabled: true }
    }

    pub fn number(label: &'static str, value: f64) -> Self {
        Self { label, kind: FieldKind::Number { text: format_number(value) }, enabled: true }
    }
}

/// A number as a field shows it: up to six decimals, no trailing zeros.
pub fn format_number(value: f64) -> String {
    let text = format!("{value:.6}");
    let text = text.trim_end_matches('0').trim_end_matches('.');
    if text == "-0" { "0".to_string() } else { text.to_string() }
}

/// Fields and buttons with one keyboard focus.
#[derive(Debug, Clone, PartialEq)]
pub struct Form {
    pub fields: Vec<Field>,
    pub buttons: Vec<(String, Action)>,
    /// Index into the fields, then the buttons.
    pub focus: usize,
    pub cancel: Action,
}

impl Form {
    pub fn new(fields: Vec<Field>, buttons: Vec<(String, Action)>, cancel: Action) -> Self {
        let mut form = Self { fields, buttons, focus: 0, cancel };
        if !form.focusable(0) {
            form.move_focus(true);
        }
        form
    }

    fn slots(&self) -> usize {
        self.fields.len() + self.buttons.len()
    }

    fn focusable(&self, slot: usize) -> bool {
        self.fields.get(slot).is_none_or(|field| field.enabled)
    }

    pub fn move_focus(&mut self, forward: bool) {
        let slots = self.slots();
        for step in 1..=slots {
            let slot = if forward { (self.focus + step) % slots } else { (self.focus + slots - step) % slots };
            if self.focusable(slot) {
                self.focus = slot;
                return;
            }
        }
    }

    pub fn number(&self, index: usize) -> Option<f64> {
        match &self.fields.get(index)?.kind {
            FieldKind::Number { text } => text.trim().parse::<f64>().ok().filter(|v| v.is_finite()),
            FieldKind::Choice { .. } => None,
        }
    }

    pub fn selected(&self, index: usize) -> usize {
        match self.fields.get(index).map(|f| &f.kind) {
            Some(FieldKind::Choice { selected, .. }) => *selected,
            _ => 0,
        }
    }

    pub fn set_number(&mut self, index: usize, value: f64) {
        if let Some(Field { kind: FieldKind::Number { text }, .. }) = self.fields.get_mut(index) {
            *text = format_number(value);
        }
    }

    pub fn set_text(&mut self, index: usize, value: String) {
        if let Some(Field { kind: FieldKind::Number { text }, enabled: true, .. }) = self.fields.get_mut(index) {
            *text = value.chars().filter(|c| c.is_ascii_digit() || matches!(c, '.' | '-' | 'e' | 'E' | '+')).take(32).collect();
        }
    }

    pub fn cycle(&mut self, index: usize, forward: bool) {
        if let Some(Field { kind: FieldKind::Choice { options, selected }, enabled: true, .. }) = self.fields.get_mut(index) {
            let count = options.len().max(1);
            *selected = if forward { (*selected + 1) % count } else { (*selected + count - 1) % count };
        }
    }

    /// Apply a key; returns the action it triggers, if any.
    pub fn key(&mut self, key: DialogKey) -> Option<Action> {
        let on_field = self.focus < self.fields.len();
        let field_kind = self.fields.get(self.focus).map(|f| matches!(f.kind, FieldKind::Number { .. }));
        match key {
            DialogKey::Next => self.move_focus(true),
            DialogKey::Previous => self.move_focus(false),
            DialogKey::Left | DialogKey::Right if on_field && field_kind == Some(false) => {
                self.cycle(self.focus, key == DialogKey::Right)
            }
            DialogKey::Left | DialogKey::Right if !on_field => self.move_focus(key == DialogKey::Right),
            DialogKey::Left | DialogKey::Right => {}
            DialogKey::Space if on_field && field_kind == Some(false) => self.cycle(self.focus, true),
            DialogKey::Char(c) if field_kind == Some(true) => {
                if let Some(FieldKind::Number { text }) = self.fields.get(self.focus).map(|f| f.kind.clone()) {
                    self.set_text(self.focus, format!("{text}{c}"));
                }
            }
            DialogKey::Backspace if field_kind == Some(true) => {
                if let Some(FieldKind::Number { mut text }) = self.fields.get(self.focus).map(|f| f.kind.clone()) {
                    text.pop();
                    self.set_text(self.focus, text);
                }
            }
            DialogKey::Char(_) | DialogKey::Backspace | DialogKey::Space if on_field => {}
            DialogKey::Activate if on_field => return self.buttons.first().map(|(_, action)| action.clone()),
            DialogKey::Activate | DialogKey::Space | DialogKey::Char(_) | DialogKey::Backspace => {
                if matches!(key, DialogKey::Activate | DialogKey::Space) {
                    return self.buttons.get(self.focus - self.fields.len()).map(|(_, action)| action.clone());
                }
            }
            DialogKey::Cancel => return Some(self.cancel.clone()),
        }
        None
    }
}

/// A SecurePlan dialog. One shows at a time, above the editor.
#[derive(Debug, Clone)]
pub enum Dialog {
    /// Text and buttons: notices and questions.
    Choice { title: String, form: Form, lines: Vec<String> },
    /// Work with a Cancel (import).
    Progress { tab_id: u64, title: String, text: String, started: std::time::Instant, form: Form },
    Align(Box<align_dialog::AlignDialog>),
    Apply(Box<apply_dialog::ApplyDialog>),
    Export(Box<export_dialog::ExportDialog>),
    Update(Box<update_dialog::UpdateDialog>),
}

impl Dialog {
    pub fn choice(title: &str, lines: Vec<String>, buttons: Vec<(String, Action)>) -> Self {
        let cancel = buttons.last().map(|(_, a)| a.clone()).unwrap_or(Action::Dismiss);
        Dialog::Choice { title: title.to_string(), form: Form::new(Vec::new(), buttons, cancel), lines }
    }

    pub fn notice(title: &str, lines: Vec<String>) -> Self {
        Self::choice(title, lines, vec![("OK".to_string(), Action::Dismiss)])
    }

    pub fn progress(tab_id: u64, title: &str, text: &str) -> Self {
        Dialog::Progress {
            tab_id,
            title: title.to_string(),
            text: text.to_string(),
            started: std::time::Instant::now(),
            form: Form::new(Vec::new(), vec![("Cancel".to_string(), Action::CancelLoad(tab_id))], Action::CancelLoad(tab_id)),
        }
    }

    pub fn form_mut(&mut self) -> &mut Form {
        match self {
            Dialog::Choice { form, .. } | Dialog::Progress { form, .. } => form,
            Dialog::Align(dialog) => &mut dialog.form,
            Dialog::Apply(dialog) => &mut dialog.form,
            Dialog::Export(dialog) => &mut dialog.form,
            Dialog::Update(dialog) => &mut dialog.form,
        }
    }

    pub fn form(&self) -> &Form {
        match self {
            Dialog::Choice { form, .. } | Dialog::Progress { form, .. } => form,
            Dialog::Align(dialog) => &dialog.form,
            Dialog::Apply(dialog) => &dialog.form,
            Dialog::Export(dialog) => &dialog.form,
            Dialog::Update(dialog) => &dialog.form,
        }
    }
}

/// A calendar date and time (UTC) for a Unix time, without a crate.
pub fn format_unix_time(seconds: u64) -> String {
    let days = (seconds / 86_400) as i64;
    let rem = seconds % 86_400;
    // Civil-from-days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02} {:02}:{:02} UTC", rem / 3600, (rem % 3600) / 60)
}

fn focus_style(focused: bool) -> impl Fn(&Theme) -> container::Style {
    move |theme: &Theme| {
        let palette = theme.palette();
        container::Style {
            border: Border {
                color: if focused { ACCENT } else { palette.background.strong.color },
                width: if focused { 3.0 } else { 1.0 },
                radius: 4.0.into(),
            },
            ..Default::default()
        }
    }
}

/// A dialog button: the focused one has a thick outline and a "›" marker.
pub fn dialog_button(label: &str, focused: bool, message: Message) -> Element<'static, Message> {
    let label = if focused { format!("› {label}") } else { format!("  {label}") };
    button(text(label).size(14))
        .padding([8, 16])
        .on_press(message)
        .style(move |theme: &Theme, status| {
            let palette = theme.palette();
            let pair = match status {
                button::Status::Hovered | button::Status::Pressed => palette.background.weak,
                _ => palette.background.weakest,
            };
            button::Style {
                background: Some(Background::Color(pair.color)),
                text_color: pair.text,
                border: Border {
                    color: if focused { ACCENT } else { palette.background.strong.color },
                    width: if focused { 3.0 } else { 1.0 },
                    radius: 4.0.into(),
                },
                ..Default::default()
            }
        })
        .into()
}

fn field_view<'a>(index: usize, field: &'a Field, focused: bool) -> Element<'a, Message> {
    let marker = if focused { "›" } else { " " };
    let label = text(format!("{marker} {}", field.label)).size(13).width(Length::Fixed(200.0));
    let control: Element<'a, Message> = match &field.kind {
        FieldKind::Choice { options, selected } => {
            let value = options.get(*selected).cloned().unwrap_or_default();
            let shown = if field.enabled { format!("‹ {value} ›") } else { format!("{value} (fixed)") };
            let mut choice = button(text(shown).size(13)).padding([4, 10]);
            if field.enabled {
                choice = choice.on_press(Message::SecurePlan(Msg::FormCycle(index)));
            }
            choice.into()
        }
        FieldKind::Number { text: value } => {
            let mut input = text_input("", value).size(13).width(Length::Fixed(160.0));
            if field.enabled {
                input = input.on_input(move |value| Message::SecurePlan(Msg::FormInput(index, value)));
            }
            input.into()
        }
    };
    container(row![label, control].spacing(8).align_y(iced::Alignment::Center))
        .padding([2, 6])
        .style(focus_style(focused))
        .into()
}

/// The fields and buttons of a form.
pub fn form_view(form: &Form) -> Element<'_, Message> {
    let mut content = column![].spacing(6);
    for (index, field) in form.fields.iter().enumerate() {
        content = content.push(field_view(index, field, form.focus == index));
    }
    let mut buttons = row![].spacing(12);
    for (index, (label, action)) in form.buttons.iter().enumerate() {
        let focused = form.focus == form.fields.len() + index;
        buttons = buttons.push(dialog_button(label, focused, Message::SecurePlan(Msg::Action(action.clone()))));
    }
    content.push(buttons).into()
}

const KEYS: &str = "Tab moves, Left and Right change a choice, Enter chooses, Escape cancels.";

/// The dialog, stacked over `base` as a modal.
pub fn view<'a>(base: Element<'a, Message>, dialog: &'a Dialog) -> Element<'a, Message> {
    let (title, body): (&str, Element<'a, Message>) = match dialog {
        Dialog::Choice { title, form, lines } => {
            let mut content = column![].spacing(10).padding(8);
            for line in lines {
                content = content.push(text(line.as_str()).size(13).width(Length::Fixed(480.0)));
            }
            (title.as_str(), content.push(form_view(form)).push(text(KEYS).size(12)).into())
        }
        Dialog::Progress { title, text: what, started, form, .. } => {
            let seconds = started.elapsed().as_secs();
            let content = column![
                text(format!("{what} ({seconds} s)")).size(13),
                text("Working… You can cancel; nothing changes until it finishes.").size(12),
                form_view(form),
            ]
            .spacing(10)
            .padding(8);
            (title.as_str(), content.into())
        }
        Dialog::Align(dialog) => ("Align the drawing to the survey", align_dialog::view(dialog)),
        Dialog::Apply(dialog) => ("Apply to SecurePlan", apply_dialog::view(dialog)),
        Dialog::Export(dialog) => ("Export CAD with the SecurePlan design", export_dialog::view(dialog)),
        Dialog::Update(dialog) => ("Update available", update_dialog::view(dialog)),
    };
    crate::ui::modal::modal(
        base,
        title,
        body,
        Message::SecurePlan(Msg::DialogKey(DialogKey::Cancel)),
        Vector::ZERO,
        crate::ui::modal::ModalOptions::NOTICE,
    )
}

/// Keyboard help shown under the form dialogs.
pub fn keys_hint() -> Element<'static, Message> {
    text(KEYS).size(12).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form() -> Form {
        let mut disabled = Field::number("Disabled", 1.0);
        disabled.enabled = false;
        Form::new(
            vec![Field::choice("Units", vec!["mm".into(), "m".into()], 0), disabled, Field::number("Length", 2.5)],
            vec![("OK".into(), Action::AlignConfirm), ("Cancel".into(), Action::Dismiss)],
            Action::Dismiss,
        )
    }

    #[test]
    fn the_keyboard_reaches_every_enabled_control_and_skips_disabled_ones() {
        let mut form = form();
        assert_eq!(form.focus, 0);
        assert_eq!(form.key(DialogKey::Right), None);
        assert_eq!(form.selected(0), 1, "Right changes a choice");
        form.key(DialogKey::Next);
        assert_eq!(form.focus, 2, "the disabled field is skipped");
        form.key(DialogKey::Backspace);
        form.key(DialogKey::Char('7'));
        form.key(DialogKey::Char('x'));
        assert_eq!(form.number(2), Some(2.7));
        assert_eq!(form.key(DialogKey::Activate), Some(Action::AlignConfirm), "Enter in a field chooses the first button");
        form.key(DialogKey::Next);
        form.key(DialogKey::Next);
        assert_eq!(form.focus, 4);
        assert_eq!(form.key(DialogKey::Space), Some(Action::Dismiss));
        form.key(DialogKey::Next);
        assert_eq!(form.focus, 0, "focus wraps");
        form.key(DialogKey::Previous);
        assert_eq!(form.focus, 4);
        assert_eq!(form.key(DialogKey::Cancel), Some(Action::Dismiss));
    }

    #[test]
    fn times_read_as_utc_dates() {
        assert_eq!(format_unix_time(0), "1970-01-01 00:00 UTC");
        assert_eq!(format_unix_time(1_790_380_800), "2026-09-26 00:00 UTC");
    }
}

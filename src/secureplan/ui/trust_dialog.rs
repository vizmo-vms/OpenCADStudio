//! The prompt that asks whether to allow a website (BRG-02, DSK-06).
//!
//! Shows the exact origin that asked to connect. Keyboard: Tab, Shift+Tab and
//! the arrow keys move focus between the two buttons, Enter or Space
//! activates the focused one, Escape declines. Focus starts on "Don't allow",
//! and the focused button has a thick outline and a "›" marker, so focus is
//! visible without relying on colour.

use iced::widget::{column, container, row, text};
use iced::{Element, Font, Length, Vector};

use crate::app::secureplan::trust::{Prompt, PromptButton};
use crate::app::secureplan::Msg;
use crate::app::Message;

/// Keys the SecurePlan dialogs understand; everything else is swallowed
/// while one shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogKey {
    Next,
    Previous,
    Left,
    Right,
    Activate,
    Space,
    Cancel,
    Char(char),
    Backspace,
}

impl DialogKey {
    /// Map a key press to a dialog action.
    pub fn from_key(key: &iced::keyboard::Key, modifiers: iced::keyboard::Modifiers) -> Option<Self> {
        use iced::keyboard::key::Named;
        match key.as_ref() {
            iced::keyboard::Key::Named(Named::Tab) if modifiers.shift() => Some(DialogKey::Previous),
            iced::keyboard::Key::Named(Named::Tab | Named::ArrowDown) => Some(DialogKey::Next),
            iced::keyboard::Key::Named(Named::ArrowUp) => Some(DialogKey::Previous),
            iced::keyboard::Key::Named(Named::ArrowLeft) => Some(DialogKey::Left),
            iced::keyboard::Key::Named(Named::ArrowRight) => Some(DialogKey::Right),
            iced::keyboard::Key::Named(Named::Enter) => Some(DialogKey::Activate),
            iced::keyboard::Key::Named(Named::Space) => Some(DialogKey::Space),
            iced::keyboard::Key::Named(Named::Escape) => Some(DialogKey::Cancel),
            iced::keyboard::Key::Named(Named::Backspace) => Some(DialogKey::Backspace),
            iced::keyboard::Key::Character(c) if !(modifiers.control() || modifiers.alt() || modifiers.logo()) => {
                c.chars().next().map(DialogKey::Char)
            }
            _ => None,
        }
    }
}

fn prompt_button(label: &str, focused: bool, message: Message) -> Element<'static, Message> {
    super::dialog_button(label, focused, message)
}

/// The prompt, stacked over `base` as a modal.
pub fn view<'a>(base: Element<'a, Message>, prompt: &'a Prompt) -> Element<'a, Message> {
    let decline = Message::SecurePlan(Msg::TrustAnswer(false));
    let content = column![
        text("A website asked SecurePlan CAD to connect:").size(14),
        container(text(prompt.request.origin.as_str()).font(Font::MONOSPACE).size(16)).padding([6, 10]),
        text(
            "Allow it only if this is your organisation's SecurePlan address. An allowed \
             website can open drawings in SecurePlan CAD when you choose Edit or View in \
             desktop there. You can remove it later under Allowed websites.",
        )
        .size(13)
        .width(Length::Fixed(420.0)),
        row![
            prompt_button("Don't allow", prompt.focus == PromptButton::Decline, decline.clone()),
            prompt_button(
                "Allow",
                prompt.focus == PromptButton::Allow,
                Message::SecurePlan(Msg::TrustAnswer(true)),
            ),
        ]
        .spacing(12),
        text("Tab moves between the buttons, Enter chooses, Escape declines.").size(12),
    ]
    .spacing(12)
    .padding(8);
    crate::ui::modal::modal(
        base,
        "Allow this website?",
        content,
        decline,
        Vector::ZERO,
        crate::ui::modal::ModalOptions::NOTICE,
    )
}

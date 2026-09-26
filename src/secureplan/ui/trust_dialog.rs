//! The origin-trust prompt (BRG-02, DSK-06).
//!
//! Shows the exact origin that asked to connect. Keyboard: Tab, Shift+Tab and
//! the arrow keys move focus between the two buttons, Enter or Space
//! activates the focused one, Escape declines. Focus starts on "Don't trust",
//! and the focused button has a thick outline and a "›" marker, so focus is
//! visible without relying on colour.

use iced::widget::{button, column, container, row, text};
use iced::{Background, Border, Element, Font, Length, Theme, Vector};

use crate::app::secureplan::trust::{Prompt, PromptButton};
use crate::app::secureplan::Msg;
use crate::app::Message;

/// Keys the prompt understands; everything else is swallowed while it shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogKey {
    Next,
    Previous,
    Activate,
    Cancel,
}

impl DialogKey {
    /// Map a key press to a prompt action.
    pub fn from_key(key: &iced::keyboard::Key, modifiers: iced::keyboard::Modifiers) -> Option<Self> {
        use iced::keyboard::key::Named;
        match key.as_ref() {
            iced::keyboard::Key::Named(Named::Tab) if modifiers.shift() => Some(DialogKey::Previous),
            iced::keyboard::Key::Named(Named::Tab | Named::ArrowRight | Named::ArrowDown) => Some(DialogKey::Next),
            iced::keyboard::Key::Named(Named::ArrowLeft | Named::ArrowUp) => Some(DialogKey::Previous),
            iced::keyboard::Key::Named(Named::Enter | Named::Space) => Some(DialogKey::Activate),
            iced::keyboard::Key::Named(Named::Escape) => Some(DialogKey::Cancel),
            _ => None,
        }
    }
}

fn prompt_button(label: &str, focused: bool, message: Message) -> Element<'static, Message> {
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
                    color: if focused { palette.primary.base.color } else { palette.background.strong.color },
                    width: if focused { 3.0 } else { 1.0 },
                    radius: 4.0.into(),
                },
                ..Default::default()
            }
        })
        .into()
}

/// The prompt, stacked over `base` as a modal.
pub fn view<'a>(base: Element<'a, Message>, prompt: &'a Prompt) -> Element<'a, Message> {
    let decline = Message::SecurePlan(Msg::TrustAnswer(false));
    let content = column![
        text("A website asked SecurePlan CAD to connect:").size(14),
        container(text(prompt.request.origin.as_str()).font(Font::MONOSPACE).size(16)).padding([6, 10]),
        text(
            "Trust it only if this is your organisation's SecurePlan address. A trusted \
             website can open drawings in SecurePlan CAD when you choose Edit or View in \
             desktop there. You can revoke it later with SECUREPLANREVOKE.",
        )
        .size(13)
        .width(Length::Fixed(420.0)),
        row![
            prompt_button("Don't trust", prompt.focus == PromptButton::Decline, decline.clone()),
            prompt_button(
                "Trust and connect",
                prompt.focus == PromptButton::Trust,
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
        "Trust this website?",
        content,
        decline,
        Vector::ZERO,
        crate::ui::modal::ModalOptions::NOTICE,
    )
}

//! The SecurePlan home screen, no standalone use, and Vizmo branding (DSK-08).
//!
//! Without a session the window shows only the home screen, in place of
//! upstream's Start page: the Vizmo logo, the name and version, how to start,
//! the connected surveys, **Check for updates** and **Allowed websites**. It
//! has no ribbon. Nothing opens, creates or saves a drawing outside a session:
//! a file is opened only by **Open drawing** in a session (PUB-01), which an
//! empty survey offers over its empty document. Keyboard: the `SECUREPLAN`
//! menu offers the home screen's actions with visible focus (DSK-06).

use iced::widget::{button, column, container, image, row, text};
use iced::{Alignment, Background, Border, Color, Element, Fill, Font, Theme};

use super::ui::{Action, ACCENT};
use super::{Msg, APP_NAME, VERSION};
use crate::app::{Message, OpenCADStudio};

/// The Vizmo logo mark (224 × 224, from the SecurePlan web app).
pub const LOGO_PNG: &[u8] = include_bytes!("../../assets/secureplan/vizmo-logo-mark.png");
/// The home screen's one line on how to start.
pub const START_HINT: &str = "In SecurePlan, open a survey's CAD tab and choose Edit in desktop.";
pub const KEYBOARD_HINT: &str = "Keyboard: type SECUREPLAN and press Enter to choose these actions.";
/// Where the source of every build is published (GPL-3.0).
pub const SOURCE_URL: &str = "https://github.com/vizmo-vms/OpenCADStudio";

/// The window icon: the logo mark as `size` × `size` RGBA pixels.
pub fn window_icon_rgba(size: u32) -> Option<Vec<u8>> {
    let logo = ::image::load_from_memory_with_format(LOGO_PNG, ::image::ImageFormat::Png).ok()?;
    Some(logo.resize_exact(size, size, ::image::imageops::FilterType::Lanczos3).into_rgba8().into_raw())
}

pub fn window_icon() -> Option<iced::window::Icon> {
    iced::window::icon::from_rgba(window_icon_rgba(64)?, 64, 64).ok()
}

/// SecurePlan CAD's lines in About, the only place that names upstream.
pub fn about_lines() -> [String; 2] {
    [
        format!("{APP_NAME} {VERSION} by Vizmo is built on Open CAD Studio, shown here."),
        format!("Licence: GNU General Public License version 3. Source code: {SOURCE_URL}"),
    ]
}

fn card_style(theme: &Theme) -> container::Style {
    let palette = theme.palette();
    container::Style {
        background: Some(Background::Color(palette.background.weakest.color)),
        border: Border { color: ACCENT, width: 1.0, radius: 8.0.into() },
        ..Default::default()
    }
}

/// A home-screen button: text label, teal outline.
fn home_button(label: String, message: Message) -> Element<'static, Message> {
    button(text(label).size(14))
        .padding([8, 16])
        .on_press(message)
        .style(|theme: &Theme, status| {
            let palette = theme.palette();
            let pair = match status {
                button::Status::Hovered | button::Status::Pressed => palette.background.weak,
                _ => palette.background.weakest,
            };
            button::Style {
                background: Some(Background::Color(pair.color)),
                text_color: pair.text,
                border: Border { color: ACCENT, width: 2.0, radius: 4.0.into() },
                ..Default::default()
            }
        })
        .into()
}

/// The prominent action: white bold text on teal (large text, so its
/// contrast suffices).
fn primary_button(label: &'static str, message: Message) -> Element<'static, Message> {
    let bold = Font { weight: iced::font::Weight::Bold, ..Font::DEFAULT };
    button(text(label).size(19).font(bold))
        .padding([10, 24])
        .on_press(message)
        .style(|_theme: &Theme, status| {
            let background = match status {
                button::Status::Hovered | button::Status::Pressed => Color::from_rgb8(0x00, 0x7A, 0x85),
                _ => ACCENT,
            };
            button::Style {
                background: Some(Background::Color(background)),
                text_color: Color::WHITE,
                border: Border { color: background, width: 2.0, radius: 6.0.into() },
                ..Default::default()
            }
        })
        .into()
}

fn command(command: &'static str) -> Message {
    Message::SecurePlan(Msg::Action(Action::Command(command)))
}

impl OpenCADStudio {
    /// Hook for `update`: with no standalone use, nothing creates or opens a
    /// drawing (new drawing, open file, recent documents, drag and drop, file
    /// arguments) and nothing saves one to a file (DSK-08).
    pub(crate) fn secureplan_refuses_standalone(&mut self, message: &Message) -> bool {
        if !self.secureplan.no_standalone {
            return false;
        }
        let opens = matches!(
            message,
            Message::TabNew
                | Message::OpenFile
                | Message::OpenPathPicked(Some(_))
                | Message::FileDropped(_)
                | Message::OpenRecent(_)
                | Message::OpenExternal(_)
        );
        // A bound document's own refusal explains Apply instead.
        let saves = matches!(message, Message::DocTabSaveAll)
            || (matches!(message, Message::SaveFile | Message::SaveAs) && !self.secureplan_is_bound(self.active_tab));
        if opens {
            self.command_line.push_error(
                "SecurePlan CAD opens drawings only for SecurePlan: in a survey's CAD tab, choose Edit in desktop, then Open drawing.",
            );
        } else if saves {
            self.command_line.push_error("SecurePlan CAD does not save drawings to files. Use Apply to send a drawing to SecurePlan.");
        }
        opens || saves
    }

    /// Connected surveys, for the home screen: (tab index, survey, website).
    pub(crate) fn secureplan_connected_surveys(&self) -> Vec<(usize, String, String)> {
        self.secureplan
            .sessions
            .bound
            .iter()
            .filter(|bound| bound.connected())
            .filter_map(|bound| Some((self.secureplan_tab_index(bound.tab_id)?, bound.label.expose().clone(), bound.origin.clone())))
            .collect()
    }

    /// The home screen, shown in place of upstream's Start page.
    pub(crate) fn secureplan_home_view(&self) -> Element<'_, Message> {
        let logo = image(image::Handle::from_bytes(LOGO_PNG)).width(112).height(112);
        let mut content = column![
            logo,
            text(APP_NAME).size(32).color(ACCENT),
            text(format!("Version {VERSION}")).size(13),
            text(START_HINT).size(15),
        ]
        .spacing(10)
        .align_x(Alignment::Center);
        let surveys = self.secureplan_connected_surveys();
        if !surveys.is_empty() {
            let mut list = column![text("Connected surveys").size(15)].spacing(8);
            for (index, label, origin) in surveys {
                list = list.push(
                    row![home_button(label, Message::TabSwitch(index)), text(origin).size(12)]
                        .spacing(10)
                        .align_y(Alignment::Center),
                );
            }
            content = content.push(container(list).padding(14).style(card_style));
        }
        content = content
            .push(
                row![
                    home_button("Check for updates".into(), command("SECUREPLANUPDATE")),
                    home_button("Allowed websites".into(), command("SECUREPLANTRUST")),
                ]
                .spacing(12),
            )
            .push(text(KEYBOARD_HINT).size(12));
        container(content).padding(24).center(Fill).into()
    }

    /// The bound tab showing an empty survey that can open a drawing: edit
    /// mode, connected, no plan, nothing imported and nothing drawn yet.
    pub(crate) fn secureplan_empty_session(&self) -> Option<u64> {
        let tab = self.tabs.get(self.active_tab)?;
        let bound = self.secureplan.sessions.by_tab(tab.id)?;
        let no_drawing = !bound.has_plan
            && bound.loaded.is_none()
            && bound.pending_original.is_none()
            && bound.recovered_base.is_none()
            && bound.staged.is_none();
        let untouched = !tab.dirty && tab.scene.document.entities().next().is_none();
        (no_drawing && untouched && self.secureplan_can_edit_tab(tab.id).is_ok()).then_some(tab.id)
    }

    /// **Open drawing**, offered prominently over an empty survey's document.
    pub(crate) fn secureplan_empty_session_card(&self) -> Option<Element<'_, Message>> {
        if self.active_modal.is_some() {
            return None;
        }
        self.secureplan_empty_session()?;
        let card = container(
            column![
                text("This survey has no drawing yet").size(18),
                text("Open a DWG or DXF drawing from this computer. SecurePlan receives it when you Apply.").size(13),
                primary_button("Open drawing", command("SECUREPLANIMPORT")),
                text("Keyboard: type SECUREPLANIMPORT and press Enter.").size(12),
            ]
            .spacing(12)
            .align_x(Alignment::Center),
        )
        .padding(24)
        .width(480)
        .style(card_style);
        Some(container(iced::widget::opaque(card)).center(Fill).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::secureplan::session::tests::Harness;
    use crate::app::secureplan::session::Format;
    use crate::app::secureplan::{overlay, testutil};
    use serde_json::Value;

    fn clicked(element: Element<'_, Message>, label: &str) -> Vec<Message> {
        let mut ui = iced_test::simulator(element);
        ui.click(label).unwrap_or_else(|_| panic!("no {label} to click"));
        ui.into_messages().collect()
    }

    fn shows(element: Element<'_, Message>, wanted: &str) -> bool {
        iced_test::simulator(element).find(wanted).is_ok()
    }

    /// A synthetic drawing on disk, which every open path would load.
    fn synthetic_file(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("secureplan_home_{tag}_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("synthetic.dxf");
        std::fs::write(&file, testutil::synthetic_dxf()).unwrap();
        file
    }

    fn opens(file: &std::path::Path) -> Vec<Message> {
        let size = std::fs::metadata(file).unwrap().len();
        vec![
            Message::TabNew,
            Message::OpenPathPicked(Some((file.to_path_buf(), size))),
            Message::FileDropped(file.to_path_buf()),
            Message::OpenRecent(file.to_path_buf()),
            Message::OpenExternal(file.to_path_buf()),
        ]
    }

    fn nothing_opened(app: &OpenCADStudio) -> bool {
        app.tabs.len() == 1 && app.tabs[0].is_start && app.opening.is_none() && app.pending_opens.is_empty()
    }

    #[test]
    fn no_drawing_is_created_opened_or_saved_without_a_session() {
        let file = synthetic_file("standalone");
        // Without the refusal, each of these opens or creates a drawing.
        for message in opens(&file) {
            let mut app = OpenCADStudio::new_for_test();
            let _ = app.update(message.clone());
            assert!(!nothing_opened(&app), "the fixture does not open with {message:?}");
        }
        assert!(OpenCADStudio::new_for_test().update(Message::OpenFile).units() > 0, "Open shows no file dialog");

        let mut app = OpenCADStudio::new_for_test();
        app.secureplan.no_standalone = true;
        for message in opens(&file) {
            let _ = app.update(message.clone());
            assert!(nothing_opened(&app), "{message:?} opened a drawing");
            assert!(app.command_line.last_error.as_deref().is_some_and(|e| e.contains("Edit in desktop")), "{message:?}");
        }
        assert_eq!(app.update(Message::OpenFile).units(), 0, "the file dialog opened");
        for message in [Message::SaveFile, Message::SaveAs, Message::DocTabSaveAll] {
            app.command_line.last_error = None;
            let task = app.update(message.clone());
            assert_eq!(task.units(), 0, "{message:?} started a save");
            assert!(app.command_line.last_error.as_deref().is_some_and(|e| e.contains("does not save")), "{message:?}");
        }
        assert!(app.recent_files.iter().all(|recent| recent != &file), "a recent-file entry was added");
        std::fs::remove_dir_all(file.parent().unwrap()).ok();
    }

    #[test]
    fn the_window_shows_the_home_screen_instead_of_the_start_page() {
        let app = OpenCADStudio::new_for_test();
        assert!(app.tabs[app.active_tab].is_start);
        assert!(shows(app.view_main(), START_HINT), "no home screen");
        for start_page in [crate::tr!("start", "new-drawing"), crate::tr!("start", "open-file")] {
            assert!(!shows(app.view_main(), &start_page), "the Start page shows {start_page}");
        }
        // The only "+" left is the status bar's (disabled) new-layout button.
        let mut ui = iced_test::simulator(app.view_main());
        if ui.click("+").is_ok() {
            assert!(!ui.into_messages().any(|m| matches!(m, Message::TabNew)), "the new-tab button shows");
        }
        let home = || app.secureplan_home_view();
        for wanted in [APP_NAME, &format!("Version {VERSION}"), KEYBOARD_HINT] {
            assert!(shows(home(), wanted), "{wanted}");
        }
        let run = |label: &str| clicked(home(), label);
        assert!(run("Check for updates").iter().any(|m| matches!(m, Message::SecurePlan(Msg::Action(Action::Command("SECUREPLANUPDATE"))))));
        assert!(run("Allowed websites").iter().any(|m| matches!(m, Message::SecurePlan(Msg::Action(Action::Command("SECUREPLANTRUST"))))));
        assert_eq!(app.secureplan_window_title(), APP_NAME);
        // The keyboard menu on the home screen: no drawing commands.
        let mut app = OpenCADStudio::new_for_test();
        let _ = app.dispatch_command("SECUREPLAN");
        let Some(super::super::ui::Dialog::Choice { form, .. }) = &app.secureplan.dialog else { panic!("no menu") };
        let labels: Vec<&str> = form.buttons.iter().map(|(label, _)| label.as_str()).collect();
        assert_eq!(labels, ["Allowed websites", "Remove website", "Developer origins", "Check for updates", "Automatic update checks", "Close"]);
    }

    #[test]
    fn the_home_screen_lists_connected_surveys_and_returns_when_the_last_closes() {
        let mut h = Harness::new("home_returns");
        // The harness turned the Start tab into a scratch drawing; a real
        // window keeps it as the home tab.
        h.app.tabs[0] = crate::app::document::DocumentTab::new_start();
        h.app.active_tab = 0;
        h.app.secureplan.no_standalone = true;
        h.open_dxf();
        let index = h.app.active_tab;
        assert_eq!(h.app.tabs.len(), 2);
        assert_eq!(h.app.secureplan_window_title(), "Synthetic survey — SecurePlan CAD");
        assert_eq!(h.app.secureplan_connected_surveys(), vec![(index, "Synthetic survey".to_string(), h.bound().origin.clone())]);
        let messages = clicked(h.app.secureplan_home_view(), "Synthetic survey");
        assert!(messages.iter().any(|m| matches!(m, Message::TabSwitch(i) if *i == index)), "the survey cannot be shown");

        let _ = h.app.update(Message::TabClose(index));
        assert_eq!(h.app.tabs.len(), 1);
        assert!(h.app.tabs[h.app.active_tab].is_start, "the home screen did not return");
        assert_eq!(h.app.secureplan_window_title(), APP_NAME);
        assert!(h.app.secureplan_connected_surveys().is_empty());
        assert!(shows(h.app.view_main(), START_HINT));
    }

    #[test]
    fn an_empty_survey_offers_open_drawing_and_others_do_not() {
        let mut h = Harness::new("empty_card");
        h.open(None, overlay::tests::overlay_bytes(&[]), "edit", Value::Null, "none", "edit");
        let tab_id = h.tab_id();
        assert_eq!(h.app.secureplan_empty_session(), Some(tab_id));
        let card = h.app.secureplan_empty_session_card().expect("no Open drawing card");
        let messages = clicked(card, "Open drawing");
        assert!(messages.iter().any(|m| matches!(m, Message::SecurePlan(Msg::Action(Action::Command("SECUREPLANIMPORT"))))));
        // Not while a dialog shows, in view mode, or once there is a drawing.
        h.app.secureplan.dialog = Some(super::super::ui::Dialog::notice("x", Vec::new()));
        assert!(!shows(h.app.view_main(), "This survey has no drawing yet"));
        h.app.secureplan.dialog = None;
        h.app.secureplan.sessions.by_tab_mut(tab_id).unwrap().mode = super::super::session::Mode::View;
        assert_eq!(h.app.secureplan_empty_session(), None, "view mode offered Open drawing");
        h.app.secureplan.sessions.by_tab_mut(tab_id).unwrap().mode = super::super::session::Mode::Edit;
        h.edit((0.0, 0.0), (1.0, 0.0));
        assert_eq!(h.app.secureplan_empty_session(), None, "a drawing in progress offered Open drawing");

        let mut h = Harness::new("plan_card");
        h.open(Some(("plan.dxf", Format::Dxf, testutil::synthetic_dxf())), overlay::tests::overlay_bytes(&[]), "edit", Value::Null, "none", "edit");
        assert_eq!(h.app.secureplan_empty_session(), None, "a survey with a drawing offered Open drawing");
    }

    #[test]
    fn the_window_icon_and_about_are_vizmo_and_name_the_source() {
        let pixels = window_icon_rgba(32).expect("the logo decodes");
        assert_eq!(pixels.len(), 32 * 32 * 4);
        assert!(window_icon().is_some());
        let [product, licence] = about_lines();
        assert!(product.contains(APP_NAME) && product.contains(VERSION) && product.contains("Open CAD Studio"));
        assert!(licence.contains("GNU General Public License version 3") && licence.contains(SOURCE_URL));
    }
}

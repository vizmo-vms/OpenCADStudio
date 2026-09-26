//! The application seam: every hook the upstream app calls under
//! `cfg(feature = "secureplan")`. With no SecurePlan activity they leave the
//! editor exactly as upstream draws and runs it.
//!
//! Hook points in upstream files:
//! - `src/app/mod.rs`: the `Message::SecurePlan` variant, the `secureplan`
//!   state field and the window title;
//! - `src/app/update/mod.rs`: `Message::SecurePlan` dispatch, and keyboard
//!   capture while a SecurePlan dialog is open;
//! - `src/app/view/mod.rs`: the subscription, the viewport overlay layer and
//!   the dialog layer above the in-canvas modals;
//! - `src/ui/ribbon/mod.rs`: the SecurePlan ribbon tab;
//! - `src/app/commands/mod.rs`: SecurePlan commands and the command guard;
//! - `src/io/xref.rs`, `src/io/mod.rs`, `src/scene/model/image_model.rs`,
//!   `src/scene/model/pdf_raster.rs`: the external-resource guard.

use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::Instant;

use iced::{Element, Subscription, Task};

use super::bridge::{self, Bridge, BridgeEvent};
use super::guards::{command_verb, CommandGuard};
use super::pairing::{self, LaunchRequest};
use super::redact::Redacted;
use super::settings::{self, Settings};
use super::trust::{Decision, PromptButton, Trust};
use super::ui::trust_dialog::{self, DialogKey};
use crate::app::{Message, OpenCADStudio};

/// Messages routed to the SecurePlan integration.
#[derive(Debug, Clone)]
pub enum Msg {
    /// A `secureplan-cad:` launch URL, from the OS hand-off or the test driver.
    Launch(Redacted<String>),
    Bridge(BridgeEvent),
    TrustAnswer(bool),
    DialogKey(DialogKey),
}

/// SecurePlan state held by the application.
#[derive(Debug)]
pub struct State {
    /// Refuses file-writing commands (PLOT, WBLOCK, Save, …) for bound
    /// documents once configured.
    pub command_guard: CommandGuard,
    pub settings: Settings,
    /// Where `settings` are saved (the user's config directory).
    pub settings_path: Option<PathBuf>,
    pub trust: Trust,
    /// The listener; the process-wide one unless a test supplies its own.
    pub bridge: Option<Arc<Bridge>>,
}

impl Default for State {
    fn default() -> Self {
        let settings_path = settings::path();
        Self {
            command_guard: CommandGuard::default(),
            settings: settings_path.as_deref().map(Settings::load_from).unwrap_or_default(),
            settings_path,
            trust: Trust::default(),
            bridge: None,
        }
    }
}

// ── Launch inbox ────────────────────────────────────────────────────────────

struct Inbox {
    sender: Mutex<mpsc::Sender<String>>,
    receiver: Mutex<Option<mpsc::Receiver<String>>>,
}

fn inbox() -> &'static Inbox {
    static INBOX: OnceLock<Inbox> = OnceLock::new();
    INBOX.get_or_init(|| {
        let (sender, receiver) = mpsc::channel();
        Inbox { sender: Mutex::new(sender), receiver: Mutex::new(Some(receiver)) }
    })
}

/// Hand a launch URL to the running application (OS hand-off, test driver).
pub fn deliver_launch(url: String) {
    let _ = inbox().sender.lock().unwrap_or_else(|e| e.into_inner()).send(url);
}

/// Forward a blocking std receiver into an iced subscription stream.
fn forward<T: Send + 'static>(
    receiver: Option<mpsc::Receiver<T>>,
    wrap: fn(T) -> Message,
) -> impl iced::futures::Stream<Item = Message> {
    iced::stream::channel(16, move |output: iced::futures::channel::mpsc::Sender<Message>| async move {
        if let Some(receiver) = receiver {
            std::thread::spawn(move || {
                use iced::futures::SinkExt;
                let mut output = output;
                for item in receiver {
                    if pollster::block_on(output.send(wrap(item))).is_err() {
                        break;
                    }
                }
            });
        }
        std::future::pending::<()>().await;
    })
}

fn bridge_events() -> impl iced::futures::Stream<Item = Message> {
    forward(bridge::global().and_then(|bridge| bridge.take_events()), |event| {
        Message::SecurePlan(Msg::Bridge(event))
    })
}

fn launches() -> impl iced::futures::Stream<Item = Message> {
    let receiver = inbox().receiver.lock().unwrap_or_else(|e| e.into_inner()).take();
    forward(receiver, |url| Message::SecurePlan(Msg::Launch(url.into())))
}

fn dialog_key(event: iced::Event, _status: iced::event::Status, _window: iced::window::Id) -> Option<Message> {
    match event {
        iced::Event::Keyboard(iced::keyboard::Event::KeyPressed { key, modifiers, .. }) => {
            DialogKey::from_key(&key, modifiers).map(|key| Message::SecurePlan(Msg::DialogKey(key)))
        }
        _ => None,
    }
}

impl OpenCADStudio {
    pub(crate) fn secureplan_update(&mut self, msg: Msg) -> Task<Message> {
        match msg {
            Msg::Launch(url) => self.secureplan_launch(url.expose()),
            Msg::TrustAnswer(accept) => self.secureplan_answer_trust(accept),
            Msg::DialogKey(key) => match key {
                DialogKey::Next | DialogKey::Previous => {
                    self.secureplan.trust.move_focus();
                }
                DialogKey::Activate => {
                    let accept = self.secureplan.trust.prompt().is_some_and(|p| p.focus == PromptButton::Trust);
                    self.secureplan_answer_trust(accept);
                }
                DialogKey::Cancel => {
                    self.secureplan_answer_trust(false);
                }
            },
            Msg::Bridge(event) => self.secureplan_bridge_event(event),
        }
        Task::none()
    }

    fn secureplan_bridge(&mut self) -> Option<Arc<Bridge>> {
        if self.secureplan.bridge.is_none() {
            self.secureplan.bridge = bridge::global_arc();
            self.secureplan_sync_trust();
        }
        self.secureplan.bridge.clone()
    }

    /// Tell the bridge which origins may pair now. Revoking an origin or
    /// turning the developer setting off drops its pending pairings and
    /// handshakes and closes its sessions (BRG-02).
    fn secureplan_sync_trust(&self) {
        let Some(bridge) = &self.secureplan.bridge else { return };
        let settings = &self.secureplan.settings;
        let allowed = settings
            .trusted_origins
            .iter()
            .filter(|origin| super::trust::origin_eligible(origin, settings.developer_loopback_origins))
            .cloned()
            .collect();
        bridge.set_trusted_origins(allowed);
    }

    fn secureplan_launch(&mut self, url: &str) {
        let Ok(request) = pairing::parse_launch_url(url) else {
            self.command_line.push_warning("SecurePlan: ignored an invalid SecurePlan CAD link.");
            return;
        };
        let received = Instant::now();
        match self.secureplan.trust.on_launch(request, &self.secureplan.settings, received) {
            Decision::Pair(request) => self.secureplan_pair(request, received),
            Decision::Prompt | Decision::Ignore => {}
        }
    }

    fn secureplan_answer_trust(&mut self, accept: bool) {
        let settings = &mut self.secureplan.settings;
        let Some((request, received)) = self.secureplan.trust.answer(accept, settings, Instant::now()) else { return };
        self.secureplan_save_settings();
        self.secureplan_pair(request, received);
    }

    fn secureplan_save_settings(&mut self) {
        let saved = match &self.secureplan.settings_path {
            Some(path) => self.secureplan.settings.save_to(path),
            None => Err(std::io::Error::other("no configuration directory")),
        };
        if saved.is_err() {
            self.command_line.push_error("SecurePlan: the settings could not be saved.");
        }
    }

    fn secureplan_pair(&mut self, request: LaunchRequest, received: Instant) {
        match self.secureplan_bridge() {
            Some(bridge) => {
                self.secureplan_sync_trust();
                bridge.add_pending_received(request, received);
            }
            None => self.command_line.push_error(
                "SecurePlan CAD could not listen on 127.0.0.1:47815–47819. Close other copies of SecurePlan CAD and try again.",
            ),
        }
    }

    fn secureplan_bridge_event(&mut self, event: BridgeEvent) {
        match event {
            BridgeEvent::Opened { origin, .. } => {
                self.command_line.push_info(&format!("SecurePlan: connected to {origin}."));
            }
            BridgeEvent::Closed { .. } => self.command_line.push_info("SecurePlan: disconnected."),
            // Session messages and transfers are handled by the session tasks.
            BridgeEvent::Message { .. } | BridgeEvent::Transfer { .. } => {}
        }
    }

    pub(crate) fn secureplan_subscription(&self) -> Subscription<Message> {
        let mut subscriptions = vec![Subscription::run(bridge_events), Subscription::run(launches)];
        if self.secureplan_dialog_open() {
            subscriptions.push(iced::event::listen_with(dialog_key));
        }
        Subscription::batch(subscriptions)
    }

    /// Whether a SecurePlan dialog owns the keyboard.
    pub(crate) fn secureplan_dialog_open(&self) -> bool {
        self.secureplan.trust.prompt().is_some()
    }

    /// Non-entity layer drawn over the drawing viewport, below the viewport's
    /// input layers. `None` draws nothing.
    pub(crate) fn secureplan_viewport_overlay(&self) -> Option<Element<'_, Message>> {
        None
    }

    /// SecurePlan dialogs stacked above the editor and its in-canvas modals.
    pub(crate) fn secureplan_view_layer<'a>(&'a self, base: Element<'a, Message>) -> Element<'a, Message> {
        match self.secureplan.trust.prompt() {
            Some(prompt) => trust_dialog::view(base, prompt),
            None => base,
        }
    }

    /// Run a SecurePlan command, or refuse a guarded one. `None` lets the
    /// command run normally.
    pub(crate) fn secureplan_dispatch(&mut self, command: &str) -> Option<Task<Message>> {
        let tab = self.tabs[self.active_tab].id;
        if let Err(refused) = self.secureplan.command_guard.check(tab, command) {
            self.command_line.push_error(&refused.to_string());
            return Some(Task::none());
        }
        let argument = command.split_whitespace().nth(1);
        match command_verb(command).as_str() {
            "SECUREPLANTRUST" => {
                let origins = &self.secureplan.settings.trusted_origins;
                let listing = if origins.is_empty() { "none".to_string() } else { origins.join(", ") };
                self.command_line.push_output(&format!("SecurePlan trusted websites: {listing}"));
                let developer = if self.secureplan.settings.developer_loopback_origins { "on" } else { "off" };
                self.command_line.push_output(&format!("Developer loopback origins: {developer}"));
            }
            "SECUREPLANREVOKE" => match argument {
                Some(origin) if self.secureplan.settings.revoke(origin) => {
                    self.secureplan_save_settings();
                    self.secureplan_sync_trust();
                    self.command_line.push_output(&format!("SecurePlan no longer trusts {origin}."));
                }
                _ => self.command_line.push_error("Usage: SECUREPLANREVOKE <trusted website origin>"),
            },
            "SECUREPLANDEVORIGINS" => match argument.map(str::to_ascii_uppercase).as_deref() {
                Some(value @ ("ON" | "OFF")) => {
                    self.secureplan.settings.developer_loopback_origins = value == "ON";
                    self.secureplan_save_settings();
                    self.secureplan_sync_trust();
                    self.command_line.push_output(&format!(
                        "Developer loopback origins (http://localhost, http://127.0.0.1): {}",
                        value.to_ascii_lowercase()
                    ));
                }
                _ => self.command_line.push_error("Usage: SECUREPLANDEVORIGINS ON|OFF"),
            },
            _ => return None,
        }
        Some(Task::none())
    }

    pub(crate) fn secureplan_window_title(&self) -> String {
        let product = format!("{} {}", super::APP_NAME, super::VERSION);
        match self.tabs.get(self.active_tab) {
            Some(tab) => {
                let dot = if tab.dirty { "● " } else { "" };
                format!("{dot}{product} - {}", tab.tab_display_name())
            }
            None => product,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::secureplan::bridge::tests::{connect_web, next_event, test_bridge, ORIGIN};
    use crate::app::secureplan::pairing::tests::launch;

    fn app_with_drawing() -> OpenCADStudio {
        let mut app = OpenCADStudio::new_for_test();
        app.automation_op(r#"{"op":"new"}"#);
        app.secureplan.settings = Settings::default();
        app.secureplan.settings_path = Some(
            std::env::temp_dir()
                .join(format!("secureplan_hooks_{}_{}", std::process::id(), line!()))
                .join("secureplan.json"),
        );
        app
    }

    fn active_command(app: &OpenCADStudio) -> Option<&'static str> {
        app.tabs[app.active_tab].active_cmd.as_ref().map(|cmd| cmd.name())
    }

    fn launch_url(request: &LaunchRequest) -> String {
        use crate::app::secureplan::channel::b64url_encode;
        format!(
            "secureplan-cad://pair?v=1&origin={}&pairing={}&token={}&survey={}&intent=edit",
            url::form_urlencoded::byte_serialize(request.origin.as_bytes()).collect::<String>(),
            b64url_encode(request.pairing.expose()),
            b64url_encode(request.token.expose()),
            request.survey.expose(),
        )
    }

    #[test]
    fn command_guard_blocks_a_refused_command_for_a_bound_tab() {
        let mut app = app_with_drawing();
        let tab = app.tabs[app.active_tab].id;
        app.secureplan.command_guard.refuse("LINE");
        app.secureplan.command_guard.bind_tab(tab);
        let _ = app.dispatch_command("LINE");
        assert_eq!(active_command(&app), None, "refused command started");
        let _ = app.dispatch_command("_line");
        assert_eq!(active_command(&app), None, "prefixed refused command started");

        // Other commands, and the same command in an unbound tab, still run.
        let _ = app.dispatch_command("CIRCLE");
        let circle = active_command(&app).expect("CIRCLE runs in a bound tab");
        app.secureplan.command_guard.unbind_tab(tab);
        let _ = app.dispatch_command("LINE");
        let line = active_command(&app).expect("LINE runs in an unbound tab");
        assert_ne!(line, circle);
    }

    #[test]
    fn title_names_the_secureplan_product() {
        let app = app_with_drawing();
        assert!(app.secureplan_window_title().contains("SecurePlan CAD 0.1.0"));
    }

    #[test]
    fn keyboard_trust_prompt_pairs_a_new_origin() {
        let mut app = app_with_drawing();
        let bridge = Arc::new(test_bridge(bridge::PING_INTERVAL));
        let events = bridge.take_events().unwrap();
        app.secureplan.bridge = Some(Arc::clone(&bridge));
        let request = launch(ORIGIN, 60);
        let _ = app.update(Message::SecurePlan(Msg::Launch(launch_url(&request).into())));
        assert!(app.secureplan_dialog_open(), "an untrusted origin prompts");
        // Keys meant for the drawing are swallowed while the prompt is open.
        let _ = app.update(Message::CommandInput("LINE".into()));
        assert!(app.command_line.input.is_empty());
        let _ = app.update(Message::SecurePlan(Msg::DialogKey(DialogKey::Next)));
        let _ = app.update(Message::SecurePlan(Msg::DialogKey(DialogKey::Activate)));
        assert!(!app.secureplan_dialog_open());
        assert!(app.secureplan.settings.is_trusted(ORIGIN));
        let saved = Settings::load_from(app.secureplan.settings_path.as_ref().unwrap());
        assert!(saved.is_trusted(ORIGIN), "trust persisted");
        let _web = connect_web(bridge.port(), &request).expect("the accepted launch pairs");
        assert!(matches!(next_event(&events), BridgeEvent::Opened { .. }));
        std::fs::remove_dir_all(app.secureplan.settings_path.clone().unwrap().parent().unwrap()).ok();
    }

    #[test]
    fn escape_declines_and_nothing_pairs() {
        let mut app = app_with_drawing();
        let bridge = Arc::new(test_bridge(bridge::PING_INTERVAL));
        app.secureplan.bridge = Some(Arc::clone(&bridge));
        let request = launch(ORIGIN, 61);
        let _ = app.update(Message::SecurePlan(Msg::Launch(launch_url(&request).into())));
        let _ = app.update(Message::SecurePlan(Msg::DialogKey(DialogKey::Activate)));
        assert!(!app.secureplan_dialog_open(), "Enter on the default button declines");
        assert!(!app.secureplan.settings.is_trusted(ORIGIN));
        assert!(connect_web(bridge.port(), &request).is_err());
    }

    #[test]
    fn revoking_by_command_invalidates_a_pending_pairing() {
        let mut app = app_with_drawing();
        let bridge = Arc::new(test_bridge(bridge::PING_INTERVAL));
        app.secureplan.bridge = Some(Arc::clone(&bridge));
        app.secureplan.settings.trust(ORIGIN);
        let request = launch(ORIGIN, 62);
        let _ = app.update(Message::SecurePlan(Msg::Launch(launch_url(&request).into())));
        assert!(!app.secureplan_dialog_open(), "a trusted origin pairs silently");
        let _ = app.dispatch_command(&format!("SECUREPLANREVOKE {ORIGIN}"));
        assert!(connect_web(bridge.port(), &request).is_err(), "the pending pairing survived the revoke");
        std::fs::remove_dir_all(app.secureplan.settings_path.clone().unwrap().parent().unwrap()).ok();
    }

    #[test]
    fn trust_commands_revoke_and_toggle_the_developer_setting() {
        let mut app = app_with_drawing();
        app.secureplan.settings.trust(ORIGIN);
        let _ = app.dispatch_command("SECUREPLANDEVORIGINS on");
        assert!(app.secureplan.settings.developer_loopback_origins);
        let _ = app.dispatch_command(&format!("SECUREPLANREVOKE {ORIGIN}"));
        assert!(!app.secureplan.settings.is_trusted(ORIGIN));
        let saved = Settings::load_from(app.secureplan.settings_path.as_ref().unwrap());
        assert!(saved.developer_loopback_origins && !saved.is_trusted(ORIGIN));
        std::fs::remove_dir_all(app.secureplan.settings_path.clone().unwrap().parent().unwrap()).ok();
    }
}

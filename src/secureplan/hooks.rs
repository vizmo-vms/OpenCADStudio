//! The application seam: every hook the upstream app calls under
//! `cfg(feature = "secureplan")`. With no SecurePlan activity they leave the
//! editor exactly as upstream draws and runs it.
//!
//! Hook points in upstream files:
//! - `src/main.rs`: the update helper (`--secureplan-update-helper`) runs
//!   before anything else; a process started by links alone decides before
//!   any window whether to start, and then starts without the editor window;
//! - `src/app/mod.rs`: the `Message::SecurePlan` variant, the `secureplan`
//!   state field, the window title, and booting without the editor window;
//! - `src/app/update/mod.rs`: `Message::SecurePlan` dispatch, keyboard
//!   capture while a SecurePlan dialog is open, the prompt window's close
//!   button, and refusing Save, Save As, plotting, printing and exports for a
//!   bound document;
//! - `src/app/update/file.rs`: no save of a bound document, a recovery copy
//!   instead of its autosave, recovery copies kept on exit, and a pending
//!   update's helper started on exit;
//! - `src/io/update_check.rs`: the fork's release endpoints (the upstream
//!   check stays inert; `update` checks with the SecurePlan settings);
//! - `src/app/update/command.rs`: closing a bound document (Apply, Discard or
//!   Keep);
//! - `src/io/mod.rs`: the entity admission limit (DSK-01);
//! - `src/app/view/mod.rs`: the subscription, the prompt-only window, the
//!   viewport overlay layer and the dialog layer above the in-canvas modals;
//! - `src/app/startup.rs`, `src/ui/window/options.rs`,
//!   `src/io/file_association.rs`: no file-association prompt, control or
//!   registration;
//! - `src/ui/ribbon/mod.rs`: the SecurePlan ribbon tab;
//! - `src/app/commands/mod.rs`: SecurePlan commands and the command guard;
//! - `src/io/xref.rs`, `src/io/mod.rs`, `src/scene/model/image_model.rs`,
//!   `src/scene/model/pdf_raster.rs`, `src/scene/text/font_face.rs`,
//!   `src/scene/text/shx.rs`, `src/scene/view/render.rs`,
//!   `src/scene/model/material_model.rs`, `src/scene/centerline.rs`,
//!   `src/app/annotation_data.rs`: the external-resource guard;
//! - `src/io/mod.rs`, `src/app/automation.rs`, `src/main.rs`: diagnostics
//!   without file paths.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use iced::{Element, Subscription, Task};

use super::bridge::{self, Bridge, BridgeEvent};
use super::guards::{command_verb, CommandGuard};
use super::pairing::{self, LaunchRequest};
use super::redact::Redacted;
use super::settings::{self, Settings};
use super::trust::{Decision, PromptButton, Trust};
use super::ui::trust_dialog::{self, DialogKey};
use super::ui::{Action, Dialog};
use crate::app::{Message, OpenCADStudio};

/// Messages routed to the SecurePlan integration.
#[derive(Debug, Clone)]
pub enum Msg {
    /// A `secureplan-cad:` launch URL, from the OS hand-off or the test driver.
    Launch(Redacted<String>),
    Bridge(BridgeEvent),
    TrustAnswer(bool),
    DialogKey(DialogKey),
    /// A link-started process has waited as long as its pairings can live.
    ColdStartExpired,
    /// Periodic while a SecurePlan document is open: `sessionState` changes,
    /// progress and notices.
    Tick,
    /// A drawing finished loading on a worker.
    Loaded(super::import::LoadDone),
    /// Apply's outputs finished building on a worker.
    ApplyBuilt(super::session::ApplyBuilt),
    /// Conversion candidates were assembled on a worker.
    Converted(super::convert::Done),
    /// An export's drawing was read and the design added, on a worker.
    ExportComposed(super::export::ComposeDone),
    /// An export was written and checked, on a worker.
    ExportWritten(super::export::WriteDone),
    /// The export's Save dialog closed (tab id, export, chosen file).
    ExportPicked(u64, super::export::JobKey, Option<Redacted<PathBuf>>),
    /// An export was written to its file, on a worker.
    ExportSaved(super::export::SaveDone),
    /// The import file dialog closed (tab id, chosen file).
    ImportPicked(u64, Option<Redacted<PathBuf>>),
    /// Mouse input in a dialog field.
    FormInput(usize, String),
    FormCycle(usize),
    /// A dialog button.
    Action(Action),
    /// Time for the automatic update check (startup, then daily; DSK-07).
    UpdateDue,
    /// An update check finished (`true` when the user asked for it).
    UpdateChecked(bool, super::update::Checked),
    /// An update download finished and was verified, or failed.
    UpdateDownloaded(Result<super::update::Staged, String>),
    /// A `secureplan-test` stdin driver command.
    #[cfg(feature = "secureplan-test")]
    Driver(super::testdriver::Command),
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
    /// Started by a `secureplan-cad:` link alone: no editor window until a
    /// session opens.
    pub cold_start: bool,
    /// The window showing only the trust prompt while there is no editor window.
    pub prompt_window: Option<iced::window::Id>,
    /// Bound documents and their sessions (DSK-03, BRG-05).
    pub sessions: super::session::Sessions,
    /// The SecurePlan dialog showing, if any (below the trust prompt).
    pub dialog: Option<Dialog>,
    pub recovery: super::recovery::Store,
    /// Whether the read-only design overlay is drawn (OVL-01).
    pub overlay_visible: bool,
    pub next_job: u64,
    /// Per bound tab, the load or import whose result is awaited; any other
    /// result for that tab is dropped.
    pub load_jobs: std::collections::HashMap<u64, u64>,
    /// Per bound tab: whether a parser worker is running, and the latest
    /// load waiting for it (older waiting loads are dropped unstarted).
    pub loads_running: std::collections::HashSet<u64>,
    pub loads_pending: std::collections::HashMap<u64, super::import::PendingLoad>,
    /// Worker jobs (parses, Apply builds) still running. While any runs,
    /// external references stay refused even if every bound tab closed.
    pub workers: usize,
    /// How long a message may wait for a transfer before the session is
    /// ended (tests shorten it).
    pub transfer_wait: Duration,
    /// Unit tests stand in for the import file picker with this file.
    #[cfg(test)]
    pub test_pick: Option<PathBuf>,
    /// Unit tests can hold worker jobs to interleave their completions.
    #[cfg(test)]
    pub held_jobs: Option<HeldJobs>,
    /// Unit tests stand in for an open export Save dialog: a written export
    /// with no file waits in `Saving`.
    #[cfg(test)]
    pub test_hold_save: bool,
    /// Unit tests make the next worker job panic.
    #[cfg(test)]
    pub test_panic_next_job: bool,
    /// A notice for the command line once the editor runs (expired recovery copies).
    pub notice: Option<String>,
    /// The main window is closing and SecurePlan drawings are being decided.
    pub quitting: bool,
    /// Updates from the fork's GitHub Releases (DSK-07).
    pub update: super::update::Updater,
}

impl Default for State {
    fn default() -> Self {
        let settings_path = settings::path();
        let mut command_guard = CommandGuard::default();
        for verb in super::session::REFUSED_COMMANDS {
            command_guard.refuse(verb);
        }
        let mut state = Self {
            command_guard,
            settings: settings_path.as_deref().map(Settings::load_from).unwrap_or_default(),
            settings_path,
            trust: Trust::default(),
            bridge: None,
            cold_start: COLD_START.load(Ordering::SeqCst),
            prompt_window: None,
            sessions: Default::default(),
            dialog: None,
            recovery: Default::default(),
            overlay_visible: true,
            next_job: 0,
            load_jobs: Default::default(),
            loads_running: Default::default(),
            loads_pending: Default::default(),
            workers: 0,
            transfer_wait: super::session::TRANSFER_WAIT,
            #[cfg(test)]
            test_pick: None,
            #[cfg(test)]
            held_jobs: None,
            #[cfg(test)]
            test_hold_save: false,
            #[cfg(test)]
            test_panic_next_job: false,
            notice: None,
            quitting: false,
            update: Default::default(),
        };
        // Recovery copies older than 30 days go, with a notice (DSK-03).
        // Unit tests never touch the user's folder.
        if !cfg!(test) {
            let deleted = state.recovery.sweep(std::time::SystemTime::now());
            if deleted > 0 {
                state.notice = Some(format!(
                    "SecurePlan: {deleted} recovery cop{} older than 30 days {} deleted.",
                    if deleted == 1 { "y" } else { "ies" },
                    if deleted == 1 { "was" } else { "were" }
                ));
            }
            // What the update helper did before this start (DSK-07).
            super::update::sweep_stale(&state.update.staging_root);
            let outcome = super::update_helper::result_path().and_then(|path| super::update_helper::take_outcome(&path));
            match outcome {
                Some(outcome) if outcome.installed => {
                    let line = format!("SecurePlan: {}", outcome.message);
                    state.notice = Some(match state.notice.take() {
                        Some(notice) => format!("{notice} {line}"),
                        None => line,
                    });
                }
                Some(outcome) => {
                    state.dialog = Some(Dialog::notice(
                        "Update not installed",
                        vec![format!("SecurePlan CAD {} was not installed.", outcome.version), outcome.message],
                    ));
                }
                None => {}
            }
        }
        state
    }
}

/// Worker jobs a unit test holds, to run them later in a chosen order.
#[cfg(test)]
#[derive(Default)]
pub struct HeldJobs(pub Vec<Box<dyn FnOnce() -> Msg + Send>>);

#[cfg(test)]
impl std::fmt::Debug for HeldJobs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HeldJobs({})", self.0.len())
    }
}

#[cfg(test)]
impl OpenCADStudio {
    /// Run held job `index` and deliver its result.
    pub(crate) fn secureplan_release_job(&mut self, index: usize) -> Task<Message> {
        let job = self.secureplan.held_jobs.as_mut().expect("jobs are held").0.remove(index);
        let message = job();
        self.secureplan_update(message)
    }
}

// ── Starting from a link ────────────────────────────────────────────────────

static COLD_START: AtomicBool = AtomicBool::new(false);

/// How long a link-started process waits for a session: a pairing's lifetime
/// plus a handshake begun just before it expired.
pub const COLD_START_WAIT: Duration = Duration::from_secs(pairing::TOKEN_TTL.as_secs() + bridge::HANDSHAKE_TIMEOUT.as_secs());

/// What a process started only by `secureplan-cad:` links does, decided
/// before any window exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColdStart {
    /// Every link is invalid or from a website that may never pair: open nothing.
    Exit,
    /// Start without the editor window: a trusted website pairs silently and
    /// the editor appears when its session opens; a new one shows only the
    /// trust prompt.
    Windowless,
}

pub fn cold_start(urls: &[String], settings: &Settings) -> ColdStart {
    let eligible = urls
        .iter()
        .filter_map(|url| pairing::parse_launch_url(url).ok())
        .any(|request| super::trust::origin_eligible(&request.origin, settings.developer_loopback_origins));
    if eligible {
        ColdStart::Windowless
    } else {
        ColdStart::Exit
    }
}

/// For `main`, when the process was started by links alone: whether to start
/// at all. Starting, the application boots without the editor window.
pub fn begin_cold_start(urls: &[String]) -> bool {
    let settings = settings::path().as_deref().map(Settings::load_from).unwrap_or_default();
    let start = cold_start(urls, &settings) == ColdStart::Windowless;
    COLD_START.store(start, Ordering::SeqCst);
    start
}

fn after(wait: Duration, message: Message) -> Task<Message> {
    let (sender, receiver) = iced::futures::channel::oneshot::channel();
    std::thread::spawn(move || {
        std::thread::sleep(wait);
        let _ = sender.send(());
    });
    Task::perform(async move { receiver.await.ok() }, move |_| message.clone())
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

#[cfg(feature = "secureplan-test")]
fn driver_commands() -> impl iced::futures::Stream<Item = Message> {
    forward(super::testdriver::take_commands(), |command| Message::SecurePlan(Msg::Driver(command)))
}

fn launches() -> impl iced::futures::Stream<Item = Message> {
    let receiver = inbox().receiver.lock().unwrap_or_else(|e| e.into_inner()).take();
    forward(receiver, |url| Message::SecurePlan(Msg::Launch(url.into())))
}

fn dialog_key(event: iced::Event, status: iced::event::Status, _window: iced::window::Id) -> Option<Message> {
    match event {
        iced::Event::Keyboard(iced::keyboard::Event::KeyPressed { key, modifiers, .. }) => {
            let key = DialogKey::from_key(&key, modifiers)?;
            // A text field focused with the mouse already took the typing.
            let typed = matches!(key, DialogKey::Char(_) | DialogKey::Backspace);
            (!(typed && status == iced::event::Status::Captured)).then_some(Message::SecurePlan(Msg::DialogKey(key)))
        }
        _ => None,
    }
}

impl OpenCADStudio {
    pub(crate) fn secureplan_update(&mut self, msg: Msg) -> Task<Message> {
        match msg {
            Msg::Launch(url) => self.secureplan_launch(url.expose()),
            Msg::TrustAnswer(accept) => self.secureplan_answer_trust(accept),
            Msg::DialogKey(key) if self.secureplan.trust.prompt().is_some() => match key {
                DialogKey::Next | DialogKey::Previous | DialogKey::Left | DialogKey::Right => {
                    self.secureplan.trust.move_focus();
                    Task::none()
                }
                DialogKey::Activate | DialogKey::Space => {
                    let accept = self.secureplan.trust.prompt().is_some_and(|p| p.focus == PromptButton::Trust);
                    self.secureplan_answer_trust(accept)
                }
                DialogKey::Cancel => self.secureplan_answer_trust(false),
                DialogKey::Char(_) | DialogKey::Backspace => Task::none(),
            },
            Msg::DialogKey(key) => {
                let action = self.secureplan.dialog.as_mut().and_then(|dialog| dialog.form_mut().key(key));
                self.secureplan_refresh_dialog();
                match action {
                    Some(action) => self.secureplan_action(action),
                    None => Task::none(),
                }
            }
            Msg::FormInput(index, value) => {
                if let Some(dialog) = self.secureplan.dialog.as_mut() {
                    dialog.form_mut().set_text(index, value);
                    dialog.form_mut().focus = index;
                }
                Task::none()
            }
            Msg::FormCycle(index) => {
                if let Some(dialog) = self.secureplan.dialog.as_mut() {
                    dialog.form_mut().cycle(index, true);
                    dialog.form_mut().focus = index;
                }
                self.secureplan_refresh_dialog();
                Task::none()
            }
            Msg::Action(action) => self.secureplan_action(action),
            Msg::UpdateDue => self.secureplan_check_updates(false),
            Msg::UpdateChecked(manual, checked) => self.secureplan_update_checked(manual, checked),
            Msg::UpdateDownloaded(result) => self.secureplan_update_downloaded(result),
            Msg::Tick => {
                if let Some(notice) = self.secureplan.notice.take() {
                    self.command_line.push_info(&notice);
                }
                self.secureplan_expire_waiting();
                self.secureplan_show_waiting_export();
                self.secureplan_report_states();
                Task::none()
            }
            Msg::Loaded(done) => {
                self.secureplan_worker_done();
                self.secureplan_loaded(done)
            }
            Msg::ApplyBuilt(built) => {
                self.secureplan_worker_done();
                self.secureplan_apply_built(built)
            }
            Msg::Converted(done) => {
                self.secureplan_worker_done();
                self.secureplan_converted(done)
            }
            Msg::ExportComposed(done) => {
                self.secureplan_worker_done();
                self.secureplan_export_composed(done)
            }
            Msg::ExportWritten(done) => {
                self.secureplan_worker_done();
                self.secureplan_export_written(done)
            }
            Msg::ExportPicked(tab_id, key, path) => self.secureplan_export_picked(tab_id, key, path.map(|path| path.expose().clone())),
            Msg::ExportSaved(done) => {
                self.secureplan_worker_done();
                self.secureplan_export_saved(done);
                Task::none()
            }
            Msg::ImportPicked(tab_id, Some(path)) => {
                // The tab the picker was opened for, if it may still import.
                if let Err(reason) = self.secureplan_can_edit_tab(tab_id) {
                    self.command_line.push_error(&reason);
                    return Task::none();
                }
                self.secureplan_import_path(tab_id, path.expose())
            }
            Msg::ImportPicked(_, None) => Task::none(),
            #[cfg(feature = "secureplan-test")]
            Msg::Driver(command) => self.secureplan_driver(command),
            Msg::Bridge(event) => self.secureplan_bridge_event(event),
            // Nothing opened in time: leave without ever showing the editor.
            Msg::ColdStartExpired if self.main_window.is_none() => self.exit_app(),
            Msg::ColdStartExpired => Task::none(),
        }
    }

    /// Whether the editor window is still waiting for a session.
    fn secureplan_windowless(&self) -> bool {
        self.secureplan.cold_start && self.main_window.is_none()
    }

    /// The small window that shows only the trust prompt.
    fn secureplan_open_prompt_window(&mut self) -> Task<Message> {
        if self.secureplan.prompt_window.is_some() {
            return Task::none();
        }
        let (id, open) = iced::window::open(iced::window::Settings {
            size: iced::Size::new(560.0, 320.0),
            position: iced::window::Position::Centered,
            resizable: false,
            exit_on_close_request: false,
            ..Default::default()
        });
        self.secureplan.prompt_window = Some(id);
        Task::batch([open.map(|_| Message::Noop), iced::window::gain_focus(id)])
    }

    fn secureplan_close_prompt_window(&mut self) -> Task<Message> {
        match self.secureplan.prompt_window.take() {
            Some(id) => iced::window::close(id),
            None => Task::none(),
        }
    }

    /// The prompt-only window's close button declines, like Escape.
    pub(crate) fn secureplan_window_close_requested(&mut self, id: iced::window::Id) -> Option<Task<Message>> {
        if self.secureplan.prompt_window == Some(id) {
            return Some(self.secureplan_answer_trust(false));
        }
        if self.main_window != Some(id) {
            return None;
        }
        // Quitting: each SecurePlan drawing with unapplied work is decided
        // first (Apply, Discard or Keep, DSK-03); then the upstream prompt
        // handles any other unsaved drawing.
        match (0..self.tabs.len()).find(|&index| self.secureplan_has_unapplied(index)) {
            Some(index) => {
                self.secureplan.quitting = true;
                self.active_tab = index;
                let tab_id = self.tabs[index].id;
                self.secureplan_open_close_dialog(tab_id);
                Some(Task::none())
            }
            None => {
                self.secureplan.quitting = false;
                None
            }
        }
    }

    /// After a bound tab closed during a quit, go on quitting.
    fn secureplan_continue_quit(&mut self, closed: Task<Message>) -> Task<Message> {
        match (self.secureplan.quitting, self.main_window) {
            (true, Some(id)) => Task::batch([closed, Task::done(Message::WindowCloseRequested(id))]),
            _ => closed,
        }
    }

    /// The view of a SecurePlan-owned window, if `id` is one.
    pub(crate) fn secureplan_window_view(&self, id: iced::window::Id) -> Option<Element<'_, Message>> {
        if self.secureplan.prompt_window != Some(id) {
            return None;
        }
        let base: Element<'_, Message> = iced::widget::Space::new().width(iced::Length::Fill).height(iced::Length::Fill).into();
        Some(match self.secureplan.trust.prompt() {
            Some(prompt) => trust_dialog::view(base, prompt),
            None => base,
        })
    }

    pub(crate) fn secureplan_bridge(&mut self) -> Option<Arc<Bridge>> {
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

    fn secureplan_launch(&mut self, url: &str) -> Task<Message> {
        let windowless = self.secureplan_windowless();
        let Ok(request) = pairing::parse_launch_url(url) else {
            self.command_line.push_warning("SecurePlan: ignored an invalid SecurePlan CAD link.");
            return Task::none();
        };
        let received = Instant::now();
        let decision = self.secureplan.trust.on_launch(request, &self.secureplan.settings, received);
        if !windowless {
            if let Decision::Pair(request) = decision {
                self.secureplan_pair(request, received);
            }
            return Task::none();
        }
        // No editor yet: a trusted website pairs silently, a new one gets the
        // prompt on its own; either way the process leaves if no session
        // opens while the pairing can live.
        let shown = match decision {
            Decision::Pair(request) => {
                self.secureplan_pair(request, received);
                Task::none()
            }
            Decision::Prompt => self.secureplan_open_prompt_window(),
            Decision::Ignore => Task::none(),
        };
        Task::batch([shown, after(COLD_START_WAIT, Message::SecurePlan(Msg::ColdStartExpired))])
    }

    fn secureplan_answer_trust(&mut self, accept: bool) -> Task<Message> {
        let closed = self.secureplan_close_prompt_window();
        let settings = &mut self.secureplan.settings;
        let Some((request, received)) = self.secureplan.trust.answer(accept, settings, Instant::now()) else {
            // Declined with no editor open: nothing more will happen.
            if self.secureplan_windowless() {
                return Task::batch([closed, self.exit_app()]);
            }
            return closed;
        };
        self.secureplan_save_settings();
        self.secureplan_pair(request, received);
        closed
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

    fn secureplan_bridge_event(&mut self, event: BridgeEvent) -> Task<Message> {
        match event {
            BridgeEvent::Opened { session, origin, survey, intent, needs_confirmation } => {
                self.command_line.push_info(&format!("SecurePlan: connected to {origin}."));
                if needs_confirmation {
                    // Re-pairing to a drawing that is open here (BRG-05).
                    self.secureplan.dialog = Some(Dialog::choice(
                        "Reconnect this drawing?",
                        vec![format!("{origin} wants to reconnect a SecurePlan drawing that is open here.")],
                        vec![
                            ("Reconnect".to_string(), Action::Repair(session, true)),
                            ("Don't reconnect".to_string(), Action::Repair(session, false)),
                        ],
                    ));
                }
                self.secureplan_session_opened(session, origin, survey, intent);
                // The editor appears once there is a session to work in.
                if self.secureplan_windowless() {
                    return Task::batch([self.open_main_window(), self.focus_cmd_input()]);
                }
            }
            BridgeEvent::Closed { session, .. } => {
                if self.secureplan.sessions.by_session_mut(session).is_some() {
                    self.command_line.push_info("SecurePlan: disconnected. Your edits stay here; open the survey from SecurePlan again to Apply them.");
                } else {
                    self.command_line.push_info("SecurePlan: disconnected.");
                }
                self.secureplan_session_closed(session);
            }
            BridgeEvent::Message { session, message } => {
                if let super::protocol::Inbound::Session { kind, request_id, body } = message {
                    return self.secureplan_message(session, kind, request_id, body);
                }
            }
            BridgeEvent::Transfer { session, transfer } => return self.secureplan_transfer(session, transfer),
        }
        Task::none()
    }

    pub(crate) fn secureplan_subscription(&self) -> Subscription<Message> {
        let mut subscriptions = vec![Subscription::run(bridge_events), Subscription::run(launches)];
        #[cfg(feature = "secureplan-test")]
        subscriptions.push(Subscription::run(driver_commands));
        if self.secureplan_dialog_open() {
            subscriptions.push(iced::event::listen_with(dialog_key));
        }
        // Automatic update checks: at startup, then daily, unless turned off.
        if self.secureplan.update.automatic(&self.secureplan.settings) {
            subscriptions.push(Subscription::run(super::update::schedule));
        }
        if !self.secureplan.sessions.bound.is_empty() || self.secureplan.notice.is_some() || !self.secureplan.sessions.deferred.is_empty() {
            subscriptions.push(iced::time::every(Duration::from_millis(500)).map(|_| Message::SecurePlan(Msg::Tick)));
        }
        Subscription::batch(subscriptions)
    }

    /// Whether a SecurePlan dialog owns the keyboard.
    pub(crate) fn secureplan_dialog_open(&self) -> bool {
        self.secureplan.trust.prompt().is_some() || self.secureplan.dialog.is_some()
    }

    /// Non-entity layer drawn over the drawing viewport, below the viewport's
    /// input layers: the read-only design overlay (OVL-01). `None` draws
    /// nothing.
    pub(crate) fn secureplan_viewport_overlay(&self) -> Option<Element<'_, Message>> {
        self.secureplan_overlay_layer()
    }

    /// SecurePlan dialogs stacked above the editor and its in-canvas modals.
    pub(crate) fn secureplan_view_layer<'a>(&'a self, base: Element<'a, Message>) -> Element<'a, Message> {
        match (self.secureplan.trust.prompt(), &self.secureplan.dialog) {
            (Some(prompt), _) => trust_dialog::view(base, prompt),
            (None, Some(dialog)) => super::ui::view(base, dialog),
            (None, None) => base,
        }
    }

    /// Run `work` off the UI thread and handle its message when it is done.
    /// If `work` panics, `failed` is delivered instead, so the operation it
    /// belongs to always ends (never busy for good). Unit tests run it at
    /// once, on their own thread.
    pub(crate) fn secureplan_run_job<F>(&mut self, work: F, failed: Msg) -> Task<Message>
    where
        F: FnOnce() -> Msg + Send + 'static,
    {
        // References stay refused for the worker's whole life (DSK-02).
        self.secureplan.workers += 1;
        self.secureplan_refresh_guards();
        #[cfg(test)]
        let test_panic = std::mem::take(&mut self.secureplan.test_panic_next_job);
        let guarded = move || {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                #[cfg(test)]
                if test_panic {
                    panic!("a worker failed (test)");
                }
                work()
            }))
            .unwrap_or(failed)
        };
        #[cfg(test)]
        {
            if let Some(held) = self.secureplan.held_jobs.as_mut() {
                held.0.push(Box::new(guarded));
                return Task::none();
            }
            let message = guarded();
            self.secureplan_update(message)
        }
        #[cfg(not(test))]
        {
            let (sender, receiver) = iced::futures::channel::oneshot::channel();
            std::thread::spawn(move || {
                let _ = sender.send(guarded());
            });
            Task::perform(async move { receiver.await.ok() }, |message| match message {
                Some(message) => Message::SecurePlan(message),
                None => Message::Noop,
            })
        }
    }

    /// A worker finished: references may be allowed again if nothing else
    /// needs them refused.
    fn secureplan_worker_done(&mut self) {
        self.secureplan.workers = self.secureplan.workers.saturating_sub(1);
        self.secureplan_refresh_guards();
    }

    /// Keep a form dialog's derived state in step with its fields.
    fn secureplan_refresh_dialog(&mut self) {
        match self.secureplan.dialog.as_mut() {
            Some(Dialog::Align(dialog)) => dialog.refresh(),
            Some(Dialog::Apply(dialog)) => dialog.refresh(),
            Some(Dialog::Export(dialog)) => dialog.refresh(),
            _ => {}
        }
    }

    /// Carry out a dialog button.
    pub(crate) fn secureplan_action(&mut self, action: Action) -> Task<Message> {
        // The dialog that asked closes, unless the action keeps it.
        let keeps_dialog =
            matches!(action, Action::AlignConfirm | Action::ApplyConfirm | Action::ApplyReset | Action::ExportSave | Action::ExportCancel);
        if !keeps_dialog {
            self.secureplan.dialog = None;
        }
        // Anything but Discard or Keep in the close prompt stops a quit and
        // abandons a Close All.
        if !matches!(action, Action::CloseDiscard(_) | Action::CloseKeep(_)) {
            self.secureplan.quitting = false;
            self.pending_tab_closes.clear();
        }
        let task = match action {
            Action::Dismiss => Task::none(),
            Action::CancelLoad(tab_id) => {
                self.secureplan_cancel_load(tab_id);
                Task::none()
            }
            Action::CloseApply(tab_id) => {
                if let Some(index) = self.secureplan_tab_index(tab_id) {
                    self.active_tab = index;
                }
                self.secureplan_begin_apply()
            }
            Action::CloseDiscard(tab_id) => {
                self.secureplan_discard_recovery(tab_id);
                let closed = self.secureplan_close_tab(tab_id);
                self.secureplan_continue_quit(closed)
            }
            Action::CloseKeep(tab_id) => {
                let kept = self
                    .secureplan_tab_index(tab_id)
                    .is_some_and(|index| self.secureplan_keep_recovery(index) != super::session::Preserve::Failed);
                if kept {
                    let closed = self.secureplan_close_tab(tab_id);
                    self.secureplan_continue_quit(closed)
                } else {
                    // Nothing closes without its copy.
                    self.secureplan.quitting = false;
                    self.pending_tab_closes.clear();
                    self.secureplan.dialog = Some(Dialog::notice(
                        "Not closed",
                        vec!["The recovery copy could not be saved, so the drawing stays open with your edits.".into()],
                    ));
                    Task::none()
                }
            }
            Action::RecoveryRestore(tab_id) => {
                self.secureplan_restore_recovery(tab_id);
                Task::none()
            }
            Action::RecoveryDiscard(tab_id) => {
                self.secureplan_discard_recovery(tab_id);
                Task::none()
            }
            Action::Repair(session, accept) => {
                if let Some(bridge) = self.secureplan_bridge() {
                    bridge.confirm(session, accept);
                }
                Task::none()
            }
            Action::Command(command) => self.dispatch_command(command),
            Action::AlignConfirm => self.secureplan_confirm_align(),
            Action::ApplyConfirm => self.secureplan_confirm_apply(),
            Action::ApplyReset => {
                if let Some(Dialog::Apply(dialog)) = self.secureplan.dialog.as_mut() {
                    dialog.reset_window();
                }
                Task::none()
            }
            Action::Convert(tab_id, kind) => self.secureplan_convert(tab_id, kind),
            Action::ExportSave => self.secureplan_export_save(),
            Action::ExportCancel => {
                self.secureplan_export_cancel();
                Task::none()
            }
            Action::UpdateStart => self.secureplan_update_start(),
            Action::UpdateKeepAndStart => self.secureplan_update_keep_and_start(),
            Action::UpdateApplyFirst(tab_id) => self.secureplan_update_apply_first(tab_id),
            Action::UpdateInstall => self.secureplan_update_install(),
            Action::UpdateDiscard => {
                self.secureplan_update_discard();
                Task::none()
            }
            Action::UpdateCancelDownload => {
                self.secureplan_update_cancel_download();
                Task::none()
            }
        };
        // An export dialog that waited for this one comes up now.
        self.secureplan_show_waiting_export();
        task
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
            // Registration as the .dwg/.dxf opener is not part of SecurePlan CAD (DSK-05).
            "FILEASSOC" => self.command_line.push_error("FILEASSOC is not available in SecurePlan CAD."),
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
                // From the ribbon: list the trusted websites and start the
                // command so the user types (or pastes) the one to revoke.
                None => {
                    let origins = &self.secureplan.settings.trusted_origins;
                    if origins.is_empty() {
                        self.command_line.push_output("SecurePlan trusts no websites.");
                    } else {
                        self.command_line.push_output(&format!("SecurePlan trusted websites: {}", origins.join(", ")));
                        self.command_line.push_info("Type the website to revoke and press Enter.");
                        self.command_line.input = "SECUREPLANREVOKE ".to_string();
                        return Some(self.focus_cmd_input());
                    }
                }
                _ => self.command_line.push_error("Usage: SECUREPLANREVOKE <trusted website origin>"),
            },
            "SECUREPLANDEVORIGINS" => match argument.map(str::to_ascii_uppercase).as_deref() {
                // From the ribbon: toggle.
                None => {
                    let on = !self.secureplan.settings.developer_loopback_origins;
                    return self.secureplan_dispatch(if on { "SECUREPLANDEVORIGINS ON" } else { "SECUREPLANDEVORIGINS OFF" });
                }
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
            // The SecurePlan tab's buttons as a keyboard menu with visible focus.
            "SECUREPLAN" => {
                let mut buttons: Vec<(String, Action)> =
                    super::ribbon::COMMANDS.iter().map(|(command, label, _)| (label.to_string(), Action::Command(command))).collect();
                buttons.push(("Close".to_string(), Action::Dismiss));
                self.secureplan.dialog = Some(Dialog::choice("SecurePlan", vec!["Choose a SecurePlan action.".to_string()], buttons));
            }
            "SECUREPLANIMPORT" => return Some(self.secureplan_start_import(self.tabs[self.active_tab].id)),
            "SECUREPLANALIGN" => self.secureplan_open_align(false),
            "SECUREPLANAPPLY" => return Some(self.secureplan_begin_apply()),
            "SECUREPLANCONVERT" => match argument.map(super::convert::Kind::parse) {
                None => return Some(self.secureplan_open_convert(None)),
                Some(Some(kind)) => return Some(self.secureplan_open_convert(Some(kind))),
                Some(None) => self.command_line.push_error("Usage: SECUREPLANCONVERT [WALLS|ROUTE]"),
            },
            // Updates (DSK-07): check now; automatic checks on or off.
            "SECUREPLANUPDATE" => return Some(self.secureplan_update_command()),
            "SECUREPLANAUTOUPDATE" => match argument.map(str::to_ascii_uppercase).as_deref() {
                None => {
                    let on = self.secureplan.settings.update_checks_off;
                    return self.secureplan_dispatch(if on { "SECUREPLANAUTOUPDATE ON" } else { "SECUREPLANAUTOUPDATE OFF" });
                }
                Some(value @ ("ON" | "OFF")) => {
                    self.secureplan.settings.update_checks_off = value == "OFF";
                    self.secureplan_save_settings();
                    self.command_line.push_output(&format!(
                        "Automatic update checks (at startup and daily): {}. SECUREPLANUPDATE checks now.",
                        value.to_ascii_lowercase()
                    ));
                }
                _ => self.command_line.push_error("Usage: SECUREPLANAUTOUPDATE ON|OFF"),
            },
            "SECUREPLANOVERLAY" => {
                self.secureplan.overlay_visible = !self.secureplan.overlay_visible;
                let state = if self.secureplan.overlay_visible { "shown" } else { "hidden" };
                self.command_line.push_output(&format!("{}: {state}.", super::overlay::LABEL));
            }
            _ => return None,
        }
        Some(Task::none())
    }

    pub(crate) fn secureplan_window_title(&self) -> String {
        let title = self.secureplan_window_title_without_update();
        format!("{title}{}", self.secureplan_update_title_suffix())
    }

    fn secureplan_window_title_without_update(&self) -> String {
        if let Some(title) = self.secureplan_window_title_for_bound() {
            return title;
        }
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
        // Each test its own settings folder: they run in parallel.
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::SeqCst);
        app.secureplan.settings_path = Some(
            std::env::temp_dir()
                .join(format!("secureplan_hooks_{}_{n}", std::process::id()))
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
    fn file_associations_can_never_be_registered_or_removed() {
        let mut app = app_with_drawing();
        assert!(!app.pending_startup_modals.contains(&crate::app::ModalKind::AssocPrompt), "startup queued the association prompt");
        app.queue_startup_prompts();
        assert!(!app.pending_startup_modals.contains(&crate::app::ModalKind::AssocPrompt));
        assert_ne!(app.active_modal, Some(crate::app::ModalKind::AssocPrompt));
        let before = app.file_assoc_enabled;
        for command in ["FILEASSOC", "FILEASSOC 1", "FILEASSOC 0", "_fileassoc 0"] {
            let _ = app.dispatch_command(command);
            assert_eq!(active_command(&app), None, "{command} started a prompt");
            assert_eq!(app.file_assoc_enabled, before, "{command} changed the setting");
        }
        // The platform calls themselves refuse, so no other path can register,
        // unregister or install thumbnails either.
        assert!(crate::io::file_association::register_as_handler().is_err());
        assert!(crate::io::file_association::unregister_handler().is_err());
        assert!(pollster::block_on(crate::io::file_association::set_default_app()).is_err());
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
    #[test]
    fn a_link_start_decides_before_any_window() {
        let mut settings = Settings::default();
        let url = |origin: &str| launch_url(&launch(origin, 90));
        assert_eq!(cold_start(&[url(ORIGIN)], &settings), ColdStart::Windowless);
        // A website that may never pair, or no valid link at all: nothing opens.
        assert_eq!(cold_start(&[url("http://secureplan.example")], &settings), ColdStart::Exit);
        assert_eq!(cold_start(&[url("http://localhost:5173")], &settings), ColdStart::Exit);
        assert_eq!(cold_start(&["secureplan-cad://pair?v=1".to_string()], &settings), ColdStart::Exit);
        assert_eq!(cold_start(&[], &settings), ColdStart::Exit);
        settings.developer_loopback_origins = true;
        assert_eq!(cold_start(&[url("http://localhost:5173")], &settings), ColdStart::Windowless);
    }

    fn link_started_app(pairing: u8) -> (OpenCADStudio, Arc<Bridge>, mpsc::Receiver<BridgeEvent>, LaunchRequest) {
        let mut app = app_with_drawing();
        app.secureplan.cold_start = true;
        let bridge = Arc::new(test_bridge(bridge::PING_INTERVAL));
        let events = bridge.take_events().unwrap();
        app.secureplan.bridge = Some(Arc::clone(&bridge));
        (app, bridge, events, launch(ORIGIN, pairing))
    }

    #[test]
    fn a_trusted_link_start_shows_the_editor_only_when_the_session_opens() {
        let (mut app, bridge, events, request) = link_started_app(91);
        app.secureplan.settings.trust(ORIGIN);
        let _ = app.update(Message::SecurePlan(Msg::Launch(launch_url(&request).into())));
        assert_eq!(app.main_window, None, "the editor opened before the session");
        assert_eq!(app.secureplan.prompt_window, None, "a trusted website was prompted for");
        let _web = connect_web(bridge.port(), &request).expect("the link pairs silently");
        let opened = next_event(&events);
        assert!(matches!(opened, BridgeEvent::Opened { .. }));
        let _ = app.update(Message::SecurePlan(Msg::Bridge(opened)));
        assert!(app.main_window.is_some(), "the editor opens with the session");
    }

    #[test]
    fn a_new_website_link_start_shows_only_the_trust_prompt() {
        // Declined, by button or by closing the prompt's window: nothing else appears.
        for decline in [Message::SecurePlan(Msg::TrustAnswer(false)), Message::WindowCloseRequested(iced::window::Id::unique())] {
            let (mut app, bridge, _events, request) = link_started_app(92);
            let _ = app.update(Message::SecurePlan(Msg::Launch(launch_url(&request).into())));
            let prompt = app.secureplan.prompt_window.expect("the prompt has its own window");
            assert_eq!(app.main_window, None, "the editor opened behind the prompt");
            assert!(app.secureplan_window_view(prompt).is_some());
            let decline = match decline {
                Message::WindowCloseRequested(_) => Message::WindowCloseRequested(prompt),
                other => other,
            };
            let _ = app.update(decline);
            assert_eq!(app.secureplan.prompt_window, None);
            assert_eq!(app.main_window, None);
            assert!(!app.secureplan.settings.is_trusted(ORIGIN));
            assert!(connect_web(bridge.port(), &request).is_err());
        }
        // Accepted: the prompt goes, and the editor waits for the session.
        let (mut app, bridge, events, request) = link_started_app(93);
        let _ = app.update(Message::SecurePlan(Msg::Launch(launch_url(&request).into())));
        let _ = app.update(Message::SecurePlan(Msg::TrustAnswer(true)));
        assert_eq!(app.secureplan.prompt_window, None);
        assert_eq!(app.main_window, None);
        let _web = connect_web(bridge.port(), &request).expect("the accepted link pairs");
        let opened = next_event(&events);
        let _ = app.update(Message::SecurePlan(Msg::Bridge(opened)));
        assert!(app.main_window.is_some());
        std::fs::remove_dir_all(app.secureplan.settings_path.clone().unwrap().parent().unwrap()).ok();
    }
}

//! TUI state and input handling: agent tabs, transcripts, the message box, permission prompts and click targets.
use crate::acp_client::{AgentEvent, SessionInfo};
use crate::config::Config;
use crate::event::{PermissionRequest, UiEvent, decider};
use crate::mcp_server;
use crate::worker::{self, Command};
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;

const MAX_ACTIVITY_LINES: usize = 200;
const WHEEL_LINES: usize = 3;
const PAGE_LINES: usize = 10;

/// Who or what produced a transcript entry; controls how it is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Agent,
    Tool,
    Image,
    Error,
}

/// One transcript item. Agent entries grow as streamed chunks arrive.
pub struct Entry {
    pub role: Role,
    pub text: String,
}

/// Connection state of an agent tab, shown as a colored dot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Offline,
    Connecting,
    Idle,
    Busy,
}

/// One agent's tab: transcript plus the channel to its worker thread (None until first message).
pub struct Chat {
    pub name: String,
    pub entries: Vec<Entry>,
    pub status: Status,
    pub worker: Option<Sender<Command>>,
    /// Lines scrolled up from the bottom; the renderer clamps it to the transcript length.
    pub scroll_up: usize,
}

/// What clicking a screen region does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Select(usize),
    ToggleAutoApprove,
    Choose(usize),
    CancelPermission,
    PickSession(usize),
    ClosePicker,
}

/// The open `/resume` list: which tab it belongs to and the sessions to choose from.
pub struct Picker {
    pub agent: usize,
    pub sessions: Vec<SessionInfo>,
}

/// Everything needed to start agents and wire them to the A2A server.
pub struct Env {
    pub config: Arc<Config>,
    pub exe: PathBuf,
    pub cwd: PathBuf,
    pub a2a_url: String,
    pub token: String,
    pub ui: Sender<UiEvent>,
}

/// The whole TUI state.
pub struct App {
    pub chats: Vec<Chat>,
    pub selected: usize,
    pub input: Vec<char>,
    pub cursor: usize,
    pub activity: Vec<String>,
    pub permissions: VecDeque<PermissionRequest>,
    pub picker: Option<Picker>,
    pub auto_allow: Arc<AtomicBool>,
    /// Click targets recorded by the last draw, in paint order (topmost last).
    pub hitboxes: Vec<(Rect, Action)>,
    pub quit: bool,
    env: Env,
}

impl App {
    /// Creates one tab per available agent.
    pub fn new(env: Env, auto_allow: Arc<AtomicBool>) -> Self {
        let chats = env
            .config
            .agent_names()
            .into_iter()
            .map(|name| Chat {
                name,
                entries: Vec::new(),
                status: Status::Offline,
                worker: None,
                scroll_up: 0,
            })
            .collect();
        App {
            chats,
            selected: 0,
            input: Vec::new(),
            cursor: 0,
            activity: Vec::new(),
            permissions: VecDeque::new(),
            picker: None,
            auto_allow,
            hitboxes: Vec::new(),
            quit: false,
            env,
        }
    }

    /// The tab currently shown.
    pub fn chat(&self) -> &Chat {
        &self.chats[self.selected]
    }

    /// Applies an event from a background thread.
    pub fn apply(&mut self, event: UiEvent) {
        match event {
            UiEvent::Activity(line) => {
                self.activity.push(line);
                if self.activity.len() > MAX_ACTIVITY_LINES {
                    self.activity.remove(0);
                }
            }
            UiEvent::Permission(request) => self.permissions.push_back(request),
            UiEvent::Agent { agent, event } => {
                let Some(index) = self.chats.iter().position(|chat| chat.name == agent) else {
                    return;
                };
                if let AgentEvent::Sessions(sessions) = event {
                    self.chats[index].status = Status::Idle;
                    if sessions.is_empty() {
                        self.chats[index].entries.push(Entry {
                            role: Role::Error,
                            text: "no past sessions for this directory".to_owned(),
                        });
                    } else {
                        self.picker = Some(Picker {
                            agent: index,
                            sessions,
                        });
                    }
                } else {
                    self.chats[index].apply(event);
                }
            }
        }
    }

    /// Handles a key press. A pending permission prompt captures the keyboard.
    pub fn key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        if !self.permissions.is_empty() {
            match key.code {
                KeyCode::Esc => self.answer_permission(None),
                KeyCode::Char(digit @ '1'..='9') => self.choose(digit as usize - '1' as usize),
                _ => {}
            }
            return;
        }
        if self.picker.is_some() {
            match key.code {
                KeyCode::Esc => self.picker = None,
                KeyCode::Char(digit @ '1'..='9') => {
                    self.pick_session(digit as usize - '1' as usize)
                }
                _ => {}
            }
            return;
        }
        match key.code {
            KeyCode::Enter => self.submit(),
            KeyCode::Char(c) => {
                self.input.insert(self.cursor, c);
                self.cursor += 1;
            }
            KeyCode::Backspace if self.cursor > 0 => {
                self.cursor -= 1;
                self.input.remove(self.cursor);
            }
            KeyCode::Delete if self.cursor < self.input.len() => {
                self.input.remove(self.cursor);
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.input.len()),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.input.len(),
            KeyCode::Tab => self.selected = (self.selected + 1) % self.chats.len(),
            KeyCode::BackTab => {
                self.selected = (self.selected + self.chats.len() - 1) % self.chats.len()
            }
            KeyCode::PageUp => self.scroll(PAGE_LINES as isize),
            KeyCode::PageDown => self.scroll(-(PAGE_LINES as isize)),
            KeyCode::Esc => {
                self.input.clear();
                self.cursor = 0;
            }
            _ => {}
        }
    }

    /// Handles mouse clicks on recorded hitboxes and wheel scrolling.
    pub fn mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let hit = self
                    .hitboxes
                    .iter()
                    .rev()
                    .find(|(rect, _)| rect.contains((mouse.column, mouse.row).into()))
                    .map(|(_, action)| *action);
                if let Some(action) = hit {
                    self.perform(action);
                }
            }
            MouseEventKind::ScrollUp => self.scroll(WHEEL_LINES as isize),
            MouseEventKind::ScrollDown => self.scroll(-(WHEEL_LINES as isize)),
            _ => {}
        }
    }

    /// Executes a click action.
    fn perform(&mut self, action: Action) {
        match action {
            Action::Select(index) => self.selected = index,
            Action::ToggleAutoApprove => {
                self.auto_allow.fetch_xor(true, Ordering::Relaxed);
            }
            Action::Choose(index) => self.choose(index),
            Action::CancelPermission => self.answer_permission(None),
            Action::PickSession(index) => self.pick_session(index),
            Action::ClosePicker => self.picker = None,
        }
    }

    /// Scrolls the transcript; positive moves toward older lines.
    fn scroll(&mut self, lines: isize) {
        let chat = &mut self.chats[self.selected];
        chat.scroll_up = chat.scroll_up.saturating_add_signed(lines);
    }

    /// Answers the oldest permission prompt with its `index`-th option, if that option exists.
    fn choose(&mut self, index: usize) {
        let id = self
            .permissions
            .front()
            .and_then(|request| request.options.get(index))
            .map(|option| option.id.clone());
        if id.is_some() {
            self.answer_permission(id);
        }
    }

    /// Sends the user's answer (None = cancel) back to the waiting agent thread and dequeues the prompt.
    fn answer_permission(&mut self, answer: Option<String>) {
        if let Some(request) = self.permissions.pop_front() {
            let _ = request.reply.send(answer);
        }
    }

    /// Handles the message box on Enter: `/resume` and `/new` are commands, anything else is a prompt.
    fn submit(&mut self) {
        let text: String = self.input.iter().collect::<String>().trim().to_owned();
        if text.is_empty() {
            return;
        }
        self.input.clear();
        self.cursor = 0;
        let index = self.selected;
        self.chats[index].scroll_up = 0;
        match text.as_str() {
            "/resume" => self.send(index, Command::ListSessions),
            "/new" => {
                self.chats[index].entries.clear();
                self.send(index, Command::NewSession);
            }
            command if command.starts_with('/') => self.chats[index].entries.push(Entry {
                role: Role::Error,
                text: format!("unknown command {command} (try /resume or /new)"),
            }),
            _ => {
                self.chats[index].entries.push(Entry {
                    role: Role::User,
                    text: text.clone(),
                });
                self.send(index, Command::Prompt(text));
            }
        }
    }

    /// Selects the tab named `agent` and opens its `/resume` list, as `central --resume <agent>` does.
    /// Returns false when no such agent exists.
    pub fn start_resume(&mut self, agent: &str) -> bool {
        let Some(index) = self.chats.iter().position(|chat| chat.name == agent) else {
            return false;
        };
        self.selected = index;
        self.send(index, Command::ListSessions);
        true
    }

    /// Starts the tab's worker if needed, queues `command`, and marks the tab busy.
    fn send(&mut self, index: usize, command: Command) {
        self.ensure_worker(index);
        let chat = &mut self.chats[index];
        if chat
            .worker
            .as_ref()
            .is_some_and(|worker| worker.send(command).is_ok())
        {
            chat.status = Status::Busy;
        }
    }

    /// Reopens the `index`-th session of the open picker in its tab, clearing the transcript for the replay.
    fn pick_session(&mut self, index: usize) {
        let Some(picker) = self.picker.take() else {
            return;
        };
        let Some(session) = picker.sessions.get(index) else {
            self.picker = Some(picker);
            return;
        };
        self.chats[picker.agent].entries.clear();
        self.send(picker.agent, Command::Resume(session.id.clone()));
    }

    /// Starts the agent behind chat `index` unless it is already running. The agent gets this
    /// binary's MCP server so it can delegate to other agents through A2A.
    fn ensure_worker(&mut self, index: usize) {
        if self.chats[index].worker.is_some() {
            return;
        }
        let name = self.chats[index].name.clone();
        let env = &self.env;
        let spec = match env.config.resolve(&name) {
            Ok(spec) => spec,
            Err(error) => {
                self.chats[index].entries.push(Entry {
                    role: Role::Error,
                    text: format!("{error:#}"),
                });
                return;
            }
        };
        let launch = spec.launch(&name, &env.exe, &env.config.path);
        let bridge = mcp_server::session_entry(
            &env.exe,
            &env.config.path,
            &env.a2a_url,
            &env.token,
            &name,
            env.config.image_agent(),
        );
        let decide = decider(&name, Some(env.ui.clone()), self.auto_allow.clone());
        let sender = worker::spawn(
            name,
            launch,
            env.cwd.clone(),
            vec![bridge],
            decide,
            env.ui.clone(),
        );
        let chat = &mut self.chats[index];
        chat.worker = Some(sender);
        chat.status = Status::Connecting;
    }
}

impl Chat {
    /// Updates the transcript and status from an agent event.
    fn apply(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::Started => {
                if self.status == Status::Connecting {
                    self.status = Status::Idle;
                }
            }
            AgentEvent::Text(text) => match self.entries.last_mut() {
                Some(last) if last.role == Role::Agent => last.text.push_str(&text),
                _ => self.entries.push(Entry {
                    role: Role::Agent,
                    text,
                }),
            },
            AgentEvent::UserText(text) => self.entries.push(Entry {
                role: Role::User,
                text,
            }),
            AgentEvent::Tool(title) => self.entries.push(Entry {
                role: Role::Tool,
                text: title,
            }),
            AgentEvent::Image(path) => {
                self.entries.push(Entry {
                    role: Role::Image,
                    text: format!("image saved: {}", path.display()),
                });
            }
            AgentEvent::Done | AgentEvent::Resumed | AgentEvent::Sessions(_) => {
                self.status = Status::Idle
            }
            AgentEvent::Notice(message) => {
                self.entries.push(Entry {
                    role: Role::Error,
                    text: message,
                });
                self.status = Status::Idle;
            }
            AgentEvent::Failed(error) => {
                self.entries.push(Entry {
                    role: Role::Error,
                    text: error,
                });
                self.status = Status::Offline;
                self.worker = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::PermissionOption;
    use ratatui::crossterm::event::{KeyEventKind, KeyEventState};
    use std::sync::mpsc;

    /// An app with no agents configured, plus the receiver for its UI events.
    fn app() -> (App, mpsc::Receiver<UiEvent>) {
        let (ui, events) = mpsc::channel();
        let env = Env {
            config: Arc::new(Config::default()),
            exe: PathBuf::from("central"),
            cwd: PathBuf::from("."),
            a2a_url: "http://127.0.0.1:1".into(),
            token: "t".into(),
            ui,
        };
        (App::new(env, Arc::new(AtomicBool::new(false))), events)
    }

    /// A key-press event with no modifiers.
    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    /// A left-button press at the given screen cell.
    fn click(column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    // Typing edits at the cursor, including inserting in the middle and deleting.
    #[test]
    fn input_editing() {
        let (mut app, _events) = app();
        for c in "helo".chars() {
            app.key(press(KeyCode::Char(c)));
        }
        app.key(press(KeyCode::Left));
        app.key(press(KeyCode::Char('l')));
        app.key(press(KeyCode::End));
        app.key(press(KeyCode::Backspace));
        assert_eq!(app.input.iter().collect::<String>(), "hell");
    }

    // Clicking a recorded hitbox runs its action; the topmost overlapping box wins.
    #[test]
    fn click_selects_agent_and_topmost_wins() {
        let (mut app, _events) = app();
        app.hitboxes = vec![
            (Rect::new(0, 0, 10, 10), Action::Select(0)),
            (Rect::new(0, 2, 10, 1), Action::Select(1)),
        ];
        app.mouse(click(3, 2));
        assert_eq!(app.selected, 1);
        app.mouse(click(3, 5));
        assert_eq!(app.selected, 0);
    }

    // The auto-approve flag toggles from a click and is visible to agent threads.
    #[test]
    fn click_toggles_auto_approve() {
        let (mut app, _events) = app();
        app.hitboxes = vec![(Rect::new(0, 0, 5, 1), Action::ToggleAutoApprove)];
        app.mouse(click(1, 0));
        assert!(app.auto_allow.load(Ordering::Relaxed));
        app.mouse(click(1, 0));
        assert!(!app.auto_allow.load(Ordering::Relaxed));
    }

    // A permission prompt blocks typing and is answered by key or click.
    #[test]
    fn permission_prompt_captures_input_and_replies() {
        let (mut app, _events) = app();
        let (reply, answer) = mpsc::channel();
        let options = vec![PermissionOption {
            id: "o".into(),
            label: "Allow once".into(),
        }];
        app.apply(UiEvent::Permission(PermissionRequest {
            agent: "a".into(),
            title: "ls".into(),
            options,
            reply,
        }));
        app.key(press(KeyCode::Char('x')));
        assert!(app.input.is_empty());
        app.key(press(KeyCode::Char('1')));
        assert_eq!(answer.recv().unwrap().as_deref(), Some("o"));
        assert!(app.permissions.is_empty());
    }

    // Streamed chunks extend the last agent entry; a tool call starts a new one.
    #[test]
    fn streamed_text_accumulates() {
        let (mut app, _events) = app();
        let agent = app.chats[0].name.clone();
        for event in [
            AgentEvent::Text("he".into()),
            AgentEvent::Text("llo".into()),
            AgentEvent::Tool("ls".into()),
        ] {
            app.apply(UiEvent::Agent {
                agent: agent.clone(),
                event,
            });
        }
        let entries = &app.chats[0].entries;
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].text, "hello");
    }

    // A failed worker is dropped so the next message restarts the agent.
    #[test]
    fn failure_resets_worker() {
        let (mut app, _events) = app();
        let agent = app.chats[0].name.clone();
        app.chats[0].worker = Some(mpsc::channel().0);
        app.apply(UiEvent::Agent {
            agent,
            event: AgentEvent::Failed("boom".into()),
        });
        assert!(app.chats[0].worker.is_none());
        assert_eq!(app.chats[0].status, Status::Offline);
    }

    // Sending a message records it and marks the tab busy.
    #[test]
    fn submit_sends_to_worker() {
        let (mut app, _events) = app();
        let (worker, inbox) = mpsc::channel();
        app.chats[0].worker = Some(worker);
        for c in "hi ".chars() {
            app.key(press(KeyCode::Char(c)));
        }
        app.key(press(KeyCode::Enter));
        assert!(matches!(inbox.recv().unwrap(), Command::Prompt(text) if text == "hi"));
        assert_eq!(app.chats[0].status, Status::Busy);
        assert!(app.input.is_empty());
    }
}

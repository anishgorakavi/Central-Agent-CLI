//! TUI state and input handling: agent tabs, transcripts, the message box, permission prompts and click targets.
use crate::acp_client::{AgentEvent, SessionInfo};
use crate::config::{AgentSpec, Config};
use crate::event::{DelegationStep, PermissionRequest, UiEvent, decider};
use crate::keys;
use crate::llm;
use crate::mcp_server;
use crate::pty_pane::PtyPane;
use crate::terminal_agents::{self, Bridge};
use crate::worker::{self, Command};
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;

const MAX_ACTIVITY_LINES: usize = 200;
const WHEEL_LINES: usize = 3;
const PAGE_LINES: usize = 10;
/// Ctrl+] starts a chord in embedded terminals, where every other key belongs to the program inside.
const PREFIX_KEY: char = ']';
const PREFIX_BYTE: u8 = 0x1d;

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

/// One agent's tab. Native tabs embed the agent's real CLI in `pane`; the rest are chat tabs with a
/// transcript and a channel to their ACP worker thread (None until the first message).
pub struct Chat {
    pub name: String,
    pub entries: Vec<Entry>,
    pub status: Status,
    pub worker: Option<Sender<Command>>,
    /// Lines scrolled up from the bottom; the renderer clamps it to the transcript length.
    pub scroll_up: usize,
    /// True when this agent runs as its real CLI in an embedded terminal.
    pub native: bool,
    pub pane: Option<PtyPane>,
    /// Set once a launch was tried, so a failing CLI is not respawned on every frame.
    pub launch_attempted: bool,
    /// Start the CLI in its resume mode on the next launch.
    pub resume_next: bool,
    /// The latest task another agent delegated here, shown over the CLI until dismissed.
    pub delegation: Option<Delegation>,
}

/// A task delegated to a tab's agent over A2A, with the work it has streamed so far.
pub struct Delegation {
    pub caller: String,
    pub text: String,
    pub running: bool,
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
    /// Models picked with `/model`, by agent name; they outlast the agent's worker.
    model_overrides: HashMap<String, String>,
    /// Screen area of the embedded terminal, recorded by the event loop before each draw.
    pub terminal_area: Rect,
    /// True after Ctrl+] while the next key is a chord command.
    pub prefix_pending: bool,
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
                native: terminal_agents::is_native(&name) && !env.config.agents.contains_key(&name),
                name,
                entries: Vec::new(),
                status: Status::Offline,
                worker: None,
                scroll_up: 0,
                pane: None,
                launch_attempted: false,
                resume_next: false,
                delegation: None,
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
            model_overrides: HashMap::new(),
            terminal_area: Rect::default(),
            prefix_pending: false,
            env,
        }
    }

    /// The model the `index`-th tab's agent runs: the `/model` choice, else the configured one.
    pub fn model_label(&self, index: usize) -> Option<String> {
        let name = &self.chats[index].name;
        self.model_overrides.get(name).cloned().or_else(|| {
            let spec = self.env.config.resolve(name).ok()?;
            spec.model().map(str::to_owned)
        })
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
            UiEvent::Delegation {
                agent,
                caller,
                step,
            } => {
                if let Some(chat) = self.chats.iter_mut().find(|chat| chat.name == agent) {
                    chat.apply_delegation(caller, step);
                }
            }
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
        let native = self.chats[self.selected].native;
        // In an embedded terminal Ctrl+C belongs to the program inside (it interrupts Claude).
        if !native
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && key.code == KeyCode::Char('c')
        {
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
        if native {
            self.native_key(key);
            return;
        }
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Enter => self.submit(),
            KeyCode::Left if alt => self.cursor = self.word_left(),
            KeyCode::Right if alt => self.cursor = self.word_right(),
            // Terminals that send Option+arrow as ESC b / ESC f arrive as Alt+b / Alt+f.
            KeyCode::Char('b') if alt => self.cursor = self.word_left(),
            KeyCode::Char('f') if alt => self.cursor = self.word_right(),
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

    /// Handles a mouse event: clicks on recorded hitboxes first, then the embedded terminal
    /// (forwarded if the program asked for the mouse, else the wheel scrolls its history),
    /// then wheel scrolling of a chat transcript.
    pub fn mouse(&mut self, mouse: MouseEvent) {
        if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
            let hit = self
                .hitboxes
                .iter()
                .rev()
                .find(|(rect, _)| rect.contains((mouse.column, mouse.row).into()))
                .map(|(_, action)| *action);
            if let Some(action) = hit {
                self.perform(action);
                return;
            }
        }
        if self.dialog_open() {
            return;
        }
        let inside_terminal = self
            .terminal_area
            .contains((mouse.column, mouse.row).into());
        if self.chats[self.selected].native {
            if inside_terminal {
                self.forward_mouse(mouse);
            }
            return;
        }
        match mouse.kind {
            MouseEventKind::ScrollUp => self.scroll(WHEEL_LINES as isize),
            MouseEventKind::ScrollDown => self.scroll(-(WHEEL_LINES as isize)),
            _ => {}
        }
    }

    /// True while a permission prompt or the session picker is on screen.
    pub fn dialog_open(&self) -> bool {
        !self.permissions.is_empty() || self.picker.is_some()
    }

    /// Sends a mouse event to the selected embedded terminal. Programs that did not enable mouse
    /// reporting do not get it; the wheel then scrolls the terminal's own history instead.
    fn forward_mouse(&mut self, mouse: MouseEvent) {
        let area = self.terminal_area;
        let Some(pane) = self.chats[self.selected]
            .pane
            .as_mut()
            .filter(|p| p.is_alive())
        else {
            return;
        };
        let (mode, encoding) =
            pane.with_screen(|s| (s.mouse_protocol_mode(), s.mouse_protocol_encoding()));
        let encoded = keys::encode_mouse(
            mouse,
            mouse.column - area.x,
            mouse.row - area.y,
            mode,
            encoding,
        );
        match (encoded, mouse.kind) {
            (Some(bytes), _) => pane.write(&bytes),
            (None, MouseEventKind::ScrollUp) => pane.scroll(WHEEL_LINES as isize),
            (None, MouseEventKind::ScrollDown) => pane.scroll(-(WHEEL_LINES as isize)),
            _ => {}
        }
    }

    /// Handles pasted text: into the embedded terminal (bracketed if it asked), or the message box.
    pub fn paste(&mut self, text: &str) {
        if self.dialog_open() {
            return;
        }
        if self.chats[self.selected].native {
            if let Some(pane) = self.chats[self.selected]
                .pane
                .as_mut()
                .filter(|p| p.is_alive())
            {
                let bracketed = pane.with_screen(|s| s.bracketed_paste());
                pane.write(&keys::encode_paste(text, bracketed));
            }
            return;
        }
        for c in text
            .chars()
            .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        {
            self.input.insert(self.cursor, c);
            self.cursor += 1;
        }
    }

    /// Handles a key while an embedded terminal is selected: Ctrl+] starts a chord, everything else
    /// goes to the program. After the chord key: n/p or Tab/Shift+Tab switch tab, 1-9 pick a tab,
    /// a toggles auto-approve, r restarts in resume mode, d dismisses the delegated-task panel, q quits, and Ctrl+] again sends a literal Ctrl+].
    fn native_key(&mut self, key: KeyEvent) {
        // Terminals send Ctrl+] as byte 0x1d, which crossterm reports as Ctrl+5.
        let is_prefix = matches!(key.code, KeyCode::Char(PREFIX_KEY | '5'))
            && key.modifiers.contains(KeyModifiers::CONTROL);
        if self.prefix_pending {
            self.prefix_pending = false;
            // Checked before the digit arm: crossterm reports Ctrl+] as Char('5') with CONTROL.
            if is_prefix {
                self.write_to_selected_pane(&[PREFIX_BYTE]);
                return;
            }
            let count = self.chats.len();
            match key.code {
                KeyCode::Char('n') | KeyCode::Tab => self.selected = (self.selected + 1) % count,
                KeyCode::Char('p') | KeyCode::BackTab => {
                    self.selected = (self.selected + count - 1) % count
                }
                KeyCode::Char(digit @ '1'..='9') => {
                    let index = digit as usize - '1' as usize;
                    if index < count {
                        self.selected = index;
                    }
                }
                KeyCode::Char('a') => self.perform(Action::ToggleAutoApprove),
                KeyCode::Char('r') => self.restart_native(self.selected, true),
                KeyCode::Char('d') => self.chats[self.selected].delegation = None,
                KeyCode::Char('q') => self.quit = true,
                _ => {}
            }
            return;
        }
        if is_prefix {
            self.prefix_pending = true;
            return;
        }
        let chat = &mut self.chats[self.selected];
        match chat.pane.as_mut() {
            Some(pane) if pane.is_alive() => {
                let application_cursor = pane.with_screen(|s| s.application_cursor());
                pane.write(&keys::encode_key(key, application_cursor));
            }
            // A finished program is relaunched fresh with Enter.
            Some(_) if key.code == KeyCode::Enter => self.restart_native(self.selected, false),
            _ => {}
        }
    }

    /// Writes raw bytes to the selected embedded terminal, if it is running.
    fn write_to_selected_pane(&mut self, bytes: &[u8]) {
        if let Some(pane) = self.chats[self.selected]
            .pane
            .as_mut()
            .filter(|p| p.is_alive())
        {
            pane.write(bytes);
        }
    }

    /// Kills the tab's terminal program so the next frame relaunches it, optionally in resume mode.
    fn restart_native(&mut self, index: usize, resume: bool) {
        let chat = &mut self.chats[index];
        chat.pane = None;
        chat.launch_attempted = false;
        chat.resume_next = resume;
        chat.entries.clear();
    }

    /// Keeps embedded terminals in step with the screen: launches the selected one on first view,
    /// resizes it to `area`, and refreshes every native tab's status dot. Called before each draw.
    pub fn sync_terminal(&mut self, area: Rect) {
        self.terminal_area = area;
        for index in 0..self.chats.len() {
            if !self.chats[index].native {
                continue;
            }
            if index == self.selected {
                if self.chats[index].pane.is_none() && !self.chats[index].launch_attempted {
                    self.launch_native(index, area);
                }
                if let Some(pane) = self.chats[index].pane.as_mut() {
                    pane.resize(area.height, area.width);
                }
            }
            let chat = &mut self.chats[index];
            chat.status = match &chat.pane {
                Some(pane) if pane.is_alive() => Status::Idle,
                _ => Status::Offline,
            };
        }
    }

    /// Starts tab `index`'s real CLI at the size of `area`, recording a launch error in the transcript.
    fn launch_native(&mut self, index: usize, area: Rect) {
        let name = self.chats[index].name.clone();
        let resume = self.chats[index].resume_next;
        let env = &self.env;
        let bridge = Bridge {
            exe: &env.exe,
            config_path: &env.config.path,
            a2a_url: &env.a2a_url,
            token: &env.token,
            image_agent: env.config.image_agent(),
        };
        let launched = terminal_agents::launch(&name, &bridge, resume)
            .ok_or_else(|| anyhow::anyhow!("{name} has no native CLI"))
            .and_then(|launch| PtyPane::spawn(&launch, &env.cwd, area.height, area.width));
        let chat = &mut self.chats[index];
        chat.launch_attempted = true;
        chat.resume_next = false;
        match launched {
            Ok(pane) => chat.pane = Some(pane),
            Err(error) => chat.entries.push(Entry {
                role: Role::Error,
                text: format!("{error:#}"),
            }),
        }
    }

    /// Cursor index one word to the left of the cursor, skipping spaces first.
    fn word_left(&self) -> usize {
        let mut index = self.cursor;
        while index > 0 && self.input[index - 1].is_whitespace() {
            index -= 1;
        }
        while index > 0 && !self.input[index - 1].is_whitespace() {
            index -= 1;
        }
        index
    }

    /// Cursor index at the end of the next word to the right, skipping spaces first.
    fn word_right(&self) -> usize {
        let mut index = self.cursor;
        while index < self.input.len() && self.input[index].is_whitespace() {
            index += 1;
        }
        while index < self.input.len() && !self.input[index].is_whitespace() {
            index += 1;
        }
        index
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

    /// Handles the message box on Enter: `/resume`, `/new` and `/model` are commands, anything else is a prompt.
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
            command if command == "/model" || command.starts_with("/model ") => {
                self.model_command(index, command["/model".len()..].trim());
            }
            command if command.starts_with('/') => self.chats[index].entries.push(Entry {
                role: Role::Error,
                text: format!("unknown command {command} (try /resume, /new or /model)"),
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

    /// `/model` lists the installed Ollama models and the current one; `/model <name>` restarts the
    /// tab's agent on that model with a fresh conversation, since history does not carry across models.
    fn model_command(&mut self, index: usize, requested: &str) {
        let name = self.chats[index].name.clone();
        let spec = self.env.config.resolve(&name);
        let notice = |app: &mut App, role, text: String| {
            app.chats[index].entries.push(Entry { role, text });
        };
        let Some(current) = self.model_label(index) else {
            let text = format!("{name} does not support choosing a model here");
            return notice(self, Role::Error, text);
        };
        if requested.is_empty() {
            let mut text = format!("model: {current}");
            if let Ok(AgentSpec::Ollama { base_url, .. }) = &spec {
                match llm::ollama_models(base_url) {
                    Ok(models) => text.push_str(&format!("\ninstalled: {}", models.join(", "))),
                    Err(error) => text.push_str(&format!("\ncannot list models: {error:#}")),
                }
            }
            text.push_str("\nswitch with /model <name>");
            return notice(self, Role::Tool, text);
        }
        self.model_overrides.insert(name, requested.to_owned());
        // Dropping the worker's sender stops its thread; the next message starts one on the new model.
        let chat = &mut self.chats[index];
        chat.worker = None;
        chat.status = Status::Offline;
        chat.entries.clear();
        notice(
            self,
            Role::Tool,
            format!("model set to {requested}; the next message starts a fresh session"),
        );
    }

    /// Selects the tab named `agent` and opens its `/resume` list, as `central --resume <agent>` does.
    /// Returns false when no such agent exists.
    pub fn start_resume(&mut self, agent: &str) -> bool {
        let Some(index) = self.chats.iter().position(|chat| chat.name == agent) else {
            return false;
        };
        self.selected = index;
        if self.chats[index].native {
            self.restart_native(index, true);
        } else {
            self.send(index, Command::ListSessions);
        }
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
        let spec =
            match env
                .config
                .resolve(&name)
                .and_then(|spec| match self.model_overrides.get(&name) {
                    Some(model) => spec.with_model(&name, model),
                    None => Ok(spec),
                }) {
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
    /// Records progress of a task delegated to this agent; a new task replaces the previous panel.
    fn apply_delegation(&mut self, caller: String, step: DelegationStep) {
        if let DelegationStep::Start(prompt) = step {
            self.delegation = Some(Delegation {
                caller,
                text: format!("Task: {prompt}\n\n"),
                running: true,
            });
            return;
        }
        let Some(delegation) = self.delegation.as_mut() else {
            return;
        };
        match step {
            DelegationStep::Text(text) => delegation.text.push_str(&text),
            DelegationStep::Tool(title) => delegation.text.push_str(&format!("\n> {title}\n")),
            DelegationStep::Failed(error) => {
                delegation.text.push_str(&format!("\nfailed: {error}\n"));
                delegation.running = false;
            }
            DelegationStep::Done => delegation.running = false,
            DelegationStep::Start(_) => {}
        }
    }

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

    /// An app with the chat-only agent `local` selected, plus the receiver for its UI events.
    fn app() -> (App, mpsc::Receiver<UiEvent>) {
        let (ui, events) = mpsc::channel();
        let env = Env {
            config: Arc::new(
                serde_json::from_str::<Config>(
                    r#"{"agents": {"local": {"kind": "ollama", "model": "m"}}}"#,
                )
                .unwrap(),
            ),
            exe: PathBuf::from("central"),
            cwd: PathBuf::from("."),
            a2a_url: "http://127.0.0.1:1".into(),
            token: "t".into(),
            ui,
        };
        let mut app = App::new(env, Arc::new(AtomicBool::new(false)));
        // Select the chat-only custom agent; built-in agents are embedded terminals.
        app.selected = app
            .chats
            .iter()
            .position(|chat| chat.name == "local")
            .unwrap();
        (app, events)
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
        let agent = app.chats[app.selected].name.clone();
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
        let entries = &app.chats[app.selected].entries;
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].text, "hello");
    }

    // A failed worker is dropped so the next message restarts the agent.
    #[test]
    fn failure_resets_worker() {
        let (mut app, _events) = app();
        let agent = app.chats[app.selected].name.clone();
        app.chats[app.selected].worker = Some(mpsc::channel().0);
        app.apply(UiEvent::Agent {
            agent,
            event: AgentEvent::Failed("boom".into()),
        });
        assert!(app.chats[app.selected].worker.is_none());
        assert_eq!(app.chats[app.selected].status, Status::Offline);
    }

    // Sending a message records it and marks the tab busy.
    #[test]
    fn submit_sends_to_worker() {
        let (mut app, _events) = app();
        let (worker, inbox) = mpsc::channel();
        app.chats[app.selected].worker = Some(worker);
        for c in "hi ".chars() {
            app.key(press(KeyCode::Char(c)));
        }
        app.key(press(KeyCode::Enter));
        assert!(matches!(inbox.recv().unwrap(), Command::Prompt(text) if text == "hi"));
        assert_eq!(app.chats[app.selected].status, Status::Busy);
        assert!(app.input.is_empty());
    }

    // /model <name> stops the worker, clears the transcript and shows the new model in the title.
    #[test]
    fn model_command_switches_model_and_resets_tab() {
        let (mut app, _events) = app();
        let (worker, _inbox) = mpsc::channel();
        let index = app.selected;
        app.chats[index].worker = Some(worker);
        app.input = "/model qwen3".chars().collect();
        app.submit();
        assert!(app.chats[index].worker.is_none());
        assert_eq!(app.model_label(index).as_deref(), Some("qwen3"));
        assert!(app.chats[index].entries[0].text.contains("qwen3"));
    }

    /// A pane running `sh -c script` on a 10x40 terminal.
    fn shell_pane(script: &str) -> PtyPane {
        let launch = crate::config::Launch {
            program: "sh".to_owned(),
            args: vec!["-c".to_owned(), script.to_owned()],
            env: Default::default(),
        };
        PtyPane::spawn(&launch, std::path::Path::new("."), 10, 40).unwrap()
    }

    /// Key press with modifiers.
    fn press_with(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            modifiers,
            ..press(code)
        }
    }

    // Option+Left/Right (and the ESC b / ESC f form) move the message-box cursor by whole words.
    #[test]
    fn alt_arrows_move_by_words() {
        let (mut app, _events) = app();
        for c in "one two  three".chars() {
            app.key(press(KeyCode::Char(c)));
        }
        app.key(press_with(KeyCode::Left, KeyModifiers::ALT));
        assert_eq!(app.cursor, 9);
        app.key(press_with(KeyCode::Char('b'), KeyModifiers::ALT));
        assert_eq!(app.cursor, 4);
        app.key(press_with(KeyCode::Right, KeyModifiers::ALT));
        assert_eq!(app.cursor, 7);
        app.key(press_with(KeyCode::Char('f'), KeyModifiers::ALT));
        assert_eq!(app.cursor, 14);
    }

    // In an embedded terminal Ctrl+C is the program's, and the Ctrl+] chord drives the app instead.
    #[test]
    fn chord_controls_app_but_ctrl_c_is_not_quit() {
        let (mut app, _events) = app();
        app.selected = app.chats.iter().position(|chat| chat.native).unwrap();
        let start = app.selected;
        app.key(press_with(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(!app.quit);
        app.key(press_with(KeyCode::Char(']'), KeyModifiers::CONTROL));
        assert!(app.prefix_pending);
        app.key(press(KeyCode::Char('n')));
        assert_eq!(app.selected, (start + 1) % app.chats.len());
        app.selected = start;
        // The form a real terminal delivers for byte 0x1d.
        app.key(press_with(KeyCode::Char('5'), KeyModifiers::CONTROL));
        app.key(press(KeyCode::Char('q')));
        assert!(app.quit);
    }

    // Keystrokes in a native tab are written to the program's pty, and pastes arrive intact.
    #[test]
    fn native_tab_forwards_keys_to_the_pty() {
        let (mut app, _events) = app();
        app.selected = app.chats.iter().position(|chat| chat.native).unwrap();
        app.chats[app.selected].pane = Some(shell_pane("read line; echo got:$line"));
        app.paste("hi");
        app.key(press(KeyCode::Char('!')));
        app.key(press(KeyCode::Enter));
        let pane = app.chats[app.selected].pane.as_ref().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !pane.with_screen(|s| s.contents().contains("got:hi!")) {
            assert!(
                std::time::Instant::now() < deadline,
                "program never saw the input"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    // Ctrl+] then Ctrl+] sends one literal Ctrl+] to the program instead of selecting tab 5.
    #[test]
    fn double_prefix_sends_literal_ctrl_bracket() {
        let (mut app, _events) = app();
        app.selected = app.chats.iter().position(|chat| chat.native).unwrap();
        app.chats[app.selected].pane = Some(shell_pane("stty raw -echo; cat -v"));
        let start = app.selected;
        app.key(press_with(KeyCode::Char('5'), KeyModifiers::CONTROL));
        app.key(press_with(KeyCode::Char('5'), KeyModifiers::CONTROL));
        assert!(!app.prefix_pending);
        assert_eq!(app.selected, start);
        let pane = app.chats[app.selected].pane.as_ref().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !pane.with_screen(|s| s.contents().contains("^]")) {
            assert!(
                std::time::Instant::now() < deadline,
                "program never saw the literal Ctrl+]"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    // Delegated work streams into the target tab's panel, and a new task replaces the old one.
    #[test]
    fn delegation_steps_fill_the_panel() {
        let (mut app, _events) = app();
        let send = |app: &mut App, step| {
            app.apply(UiEvent::Delegation {
                agent: "codex".into(),
                caller: "claude".into(),
                step,
            })
        };
        send(&mut app, DelegationStep::Start("fix it".into()));
        send(&mut app, DelegationStep::Tool("cargo test".into()));
        send(&mut app, DelegationStep::Text("done".into()));
        let codex = app.chats.iter().find(|c| c.name == "codex").unwrap();
        let panel = codex.delegation.as_ref().unwrap();
        assert!(panel.running && panel.text.contains("cargo test") && panel.text.ends_with("done"));
        send(&mut app, DelegationStep::Done);
        let codex = app.chats.iter().find(|c| c.name == "codex").unwrap();
        assert!(!codex.delegation.as_ref().unwrap().running);
    }
}

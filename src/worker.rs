//! Background thread that owns one agent's ACP connection for a TUI chat tab.
use crate::acp_client::{AgentEvent, Client, Decide};
use crate::config::Launch;
use crate::event::UiEvent;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::mpsc::{self, Sender};
use std::thread;

const IMAGE_DIR_NAME: &str = "generated-images";

/// Work the TUI can ask of an agent thread.
pub enum Command {
    Prompt(String),
    /// Report the agent's past sessions for this directory.
    ListSessions,
    /// Reopen a past session by id, replaying its history.
    Resume(String),
    /// Drop the current conversation and start a fresh session.
    NewSession,
}

/// Starts the agent in a thread and returns the channel used to send it commands.
/// Progress and failures arrive at the TUI as `UiEvent::Agent`; dropping the sender stops the thread.
/// The session is opened lazily, so a resume or list request does not first create a throwaway one.
pub fn spawn(
    agent: String,
    launch: Launch,
    cwd: PathBuf,
    mcp_servers: Vec<Value>,
    decide: Decide,
    ui: Sender<UiEvent>,
) -> Sender<Command> {
    let (commands, inbox) = mpsc::channel::<Command>();
    thread::spawn(move || {
        let emit = move |event| {
            let _ = ui.send(UiEvent::Agent {
                agent: agent.clone(),
                event,
            });
        };
        let image_dir = cwd.join(IMAGE_DIR_NAME);
        let mut client =
            match Client::connect(&launch, &cwd, decide, image_dir, Box::new(emit.clone())) {
                Ok(client) => client,
                Err(error) => return emit(AgentEvent::Failed(format!("{error:#}"))),
            };
        emit(AgentEvent::Started);
        for command in inbox {
            match command {
                Command::Prompt(text) => {
                    let result = (|| {
                        if !client.has_session() {
                            client.open_new(mcp_servers.clone())?;
                        }
                        client.prompt(&text)
                    })();
                    match result {
                        Ok(_) => emit(AgentEvent::Done),
                        // The agent process is likely dead; the TUI restarts the worker on the next message.
                        Err(error) => return emit(AgentEvent::Failed(format!("{error:#}"))),
                    }
                }
                Command::ListSessions => emit(match client.list_sessions() {
                    Ok(sessions) => AgentEvent::Sessions(sessions),
                    Err(error) => AgentEvent::Notice(format!("{error:#}")),
                }),
                Command::Resume(id) => emit(match client.open_existing(&id, mcp_servers.clone()) {
                    Ok(()) => AgentEvent::Resumed,
                    Err(error) => AgentEvent::Notice(format!("{error:#}")),
                }),
                Command::NewSession => emit(match client.open_new(mcp_servers.clone()) {
                    Ok(()) => AgentEvent::Done,
                    Err(error) => AgentEvent::Notice(format!("{error:#}")),
                }),
            }
        }
    });
    commands
}

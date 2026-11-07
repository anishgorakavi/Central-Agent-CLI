//! Messages that background threads (agent workers, the A2A server) send to the TUI, plus the permission bridge.
use crate::acp_client::{AgentEvent, Decide, pick_option};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};

/// One selectable answer to a permission request.
pub struct PermissionOption {
    pub id: String,
    pub label: String,
}

/// A pending question for the user; the answer goes back through `reply` (None = cancel).
pub struct PermissionRequest {
    pub agent: String,
    pub title: String,
    pub options: Vec<PermissionOption>,
    pub reply: Sender<Option<String>>,
}

/// One step of a delegated task, shown in the target agent's tab so its work is visible there.
pub enum DelegationStep {
    Start(String),
    Text(String),
    Tool(String),
    Done,
    Failed(String),
}

/// Everything the TUI event loop receives from other threads.
pub enum UiEvent {
    Agent {
        agent: String,
        event: AgentEvent,
    },
    /// A line for the A2A activity pane.
    Activity(String),
    Permission(PermissionRequest),
    /// Progress of a task another agent delegated to `agent`.
    Delegation {
        agent: String,
        caller: String,
        step: DelegationStep,
    },
}

/// Builds the permission policy for `agent`: auto-approve while the flag is set, otherwise ask the
/// TUI and block until the user answers. With no TUI attached and no auto-approve, requests are cancelled.
pub fn decider(agent: &str, ui: Option<Sender<UiEvent>>, auto_allow: Arc<AtomicBool>) -> Decide {
    let agent = agent.to_owned();
    Box::new(move |title, options| {
        // Re-read the flag on every request so toggling it in the TUI applies immediately.
        if auto_allow.load(Ordering::Relaxed) {
            return pick_option(options, true);
        }
        let (reply, answer) = mpsc::channel();
        let request = PermissionRequest {
            agent: agent.clone(),
            title: title.to_owned(),
            options: options
                .iter()
                .filter_map(|option| {
                    Some(PermissionOption {
                        id: option["optionId"].as_str()?.to_owned(),
                        label: option["name"].as_str().unwrap_or("?").to_owned(),
                    })
                })
                .collect(),
            reply,
        };
        ui.as_ref()?.send(UiEvent::Permission(request)).ok()?;
        answer.recv().ok().flatten()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Without a TUI and without auto-approve a request must be cancelled, never granted.
    #[test]
    fn headless_without_auto_allow_cancels() {
        let mut decide = decider("a", None, Arc::new(AtomicBool::new(false)));
        let options = [json!({"optionId": "o", "name": "Once", "kind": "allow_once"})];
        assert_eq!(decide("run ls", &options), None);
    }

    // The flag is read per request, so flipping it takes effect for the next prompt.
    #[test]
    fn auto_allow_flag_is_live() {
        let flag = Arc::new(AtomicBool::new(false));
        let mut decide = decider("a", None, flag.clone());
        let options = [json!({"optionId": "o", "name": "Once", "kind": "allow_once"})];
        flag.store(true, Ordering::Relaxed);
        assert_eq!(decide("run ls", &options).as_deref(), Some("o"));
    }

    // A user answer travels back through the reply channel.
    #[test]
    fn asks_the_ui_and_returns_its_answer() {
        let (ui, events) = mpsc::channel();
        let mut decide = decider("a", Some(ui), Arc::new(AtomicBool::new(false)));
        let options = [json!({"optionId": "o", "name": "Once", "kind": "allow_once"})];
        let asker = std::thread::spawn(move || decide("run ls", &options));
        let Ok(UiEvent::Permission(request)) = events.recv() else {
            panic!("no request")
        };
        assert_eq!(request.options[0].id, "o");
        request.reply.send(Some("o".into())).unwrap();
        assert_eq!(asker.join().unwrap().as_deref(), Some("o"));
    }
}

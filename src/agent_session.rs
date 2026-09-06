//! One face for the coding agents a pane can be running — Claude Code and
//! Codex — so everything downstream (pane title, Cmd+J, session restore, IPC)
//! treats them alike instead of knowing only about Claude.
//!
//! The per-agent detection stays in `claude_session` and `codex_session`: they
//! find a live session in completely different ways (a file per process on one
//! side, the open transcript of a process on the other). This module only picks
//! between them and says how to reopen what it found.

use serde::{Deserialize, Serialize};

/// Which agent a pane is running.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Agent {
    Claude,
    Codex,
}

impl Agent {
    /// Name used in the session file and over IPC. Stable: a restore reads it.
    pub fn as_str(&self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
        }
    }
}

/// A conversation open in a pane, whichever agent holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentSession {
    pub agent: Agent,
    /// Conversation id — what the agent's `resume` takes.
    pub id: String,
    /// Name the user gave the conversation. Claude Code only (`/rename`);
    /// Codex has no equivalent surfaced anywhere Kova can read.
    pub name: Option<String>,
}

/// The agent session running under `shell_pid`, if any.
///
/// Claude is asked first: its lookup is a small directory read, where the Codex
/// one walks the process table. A pane runs one agent at a time, so the second
/// scan only happens for panes that are not Claude.
pub fn for_shell(shell_pid: u32) -> Option<AgentSession> {
    if let Some(s) = crate::claude_session::session_for_shell(shell_pid) {
        return Some(AgentSession { agent: Agent::Claude, id: s.id, name: s.name });
    }
    let id = crate::codex_session::for_shell(shell_pid)?;
    Some(AgentSession { agent: Agent::Codex, id, name: None })
}

/// The command line that reopens this conversation, or `None` when the id is
/// not one we are willing to type into a shell.
///
/// `last_command` is the line the pane last ran: on the Claude side it carries
/// the flags the session was started with, which the resume line reuses.
pub fn resume_command(agent: Agent, session_id: &str, last_command: Option<&str>) -> Option<String> {
    match agent {
        Agent::Claude => crate::claude_session::resume_command(last_command, session_id),
        Agent::Codex => crate::codex_session::resume_command(session_id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_agent_resumes_with_its_own_command() {
        let id = "01a07651-015e-78a3-97f2-2eaf0f0cd663";
        assert_eq!(
            resume_command(Agent::Codex, id, None).as_deref(),
            Some("codex resume 01a07651-015e-78a3-97f2-2eaf0f0cd663")
        );
        let claude = resume_command(Agent::Claude, id, None).expect("a claude line");
        assert!(claude.starts_with("claude "), "got {}", claude);
        assert!(claude.contains(id));
    }

    #[test]
    fn an_unsafe_id_is_refused_for_both() {
        assert!(resume_command(Agent::Claude, "id\nrm -rf ~", None).is_none());
        assert!(resume_command(Agent::Codex, "id\nrm -rf ~", None).is_none());
    }

    #[test]
    fn the_agent_name_round_trips_through_the_session_file() {
        for agent in [Agent::Claude, Agent::Codex] {
            let json = serde_json::to_string(&agent).unwrap();
            assert_eq!(json, format!("\"{}\"", agent.as_str()));
            assert_eq!(serde_json::from_str::<Agent>(&json).unwrap(), agent);
        }
    }
}

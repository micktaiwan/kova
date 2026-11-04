//! One face for the coding agents a pane can be running — Claude Code, Codex
//! and Pi, plus omp and Tazu — so everything downstream (pane title, Cmd+J, session
//! restore, IPC) treats them alike instead of knowing only about Claude.
//!
//! The per-agent detection stays in `claude_session`, `codex_session`,
//! `pi_session`, `omp_session` and `tazu_session`: they find a live session in completely
//! different ways (a file per process on one side, the open transcript of a
//! process on the other, the start time of a process against the names of its
//! session files elsewhere).
//! This module only picks between them and says how to reopen what it found.

use serde::{Deserialize, Serialize};

/// Which agent a pane is running.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Agent {
    Claude,
    Codex,
    Pi,
    Omp,
    Tazu,
}

impl Default for Agent {
    fn default() -> Self {
        Agent::Claude
    }
}

impl Agent {
    /// Name used in the session file and over IPC. Stable: a restore reads it.
    pub fn as_str(&self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
            Agent::Pi => "pi",
            Agent::Omp => "omp",
            Agent::Tazu => "tazu",
        }
    }

    /// Human name for a row or a label.
    pub fn label(&self) -> &'static str {
        match self {
            Agent::Claude => "Claude",
            Agent::Codex => "Codex",
            Agent::Pi => "Pi",
            Agent::Omp => "Omp",
            Agent::Tazu => "Tazu",
        }
    }
}

/// A conversation open in a pane, whichever agent holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentSession {
    pub agent: Agent,
    /// Conversation id — what the agent's `resume` takes.
    pub id: String,
    /// The agent's own process, when its detection knows it (Claude only).
    /// A launcher script that starts the agent leads the foreground process
    /// group, so this is the only way to name the agent rather than `bash`.
    pub pid: Option<u32>,
    /// Conversation name: Claude's explicit `/rename` or Codex's persisted name.
    pub name: Option<String>,
    /// Claude only: the name other Claude sessions address this one by with
    /// `SendMessage` (see `claude_session::Session::peer_name`). Codex and Pi
    /// have no such thing, so it is always `None` there.
    pub peer_name: Option<String>,
    /// Claude only: the session's `CLAUDE_CONFIG_DIR` when it is not the
    /// default one (`claude_session::Session::config_dir`).
    pub config_dir: Option<String>,
    /// Claude only: shells the session started in the background are still
    /// running (`claude_session::Session::background_shells`).
    pub background_shells: bool,
}

/// The agent session running under `shell_pid`, if any.
///
/// Claude is asked first: its lookup is a small directory read, where the Codex
/// and Pi ones walk the process table. A pane runs one agent at a time, so the
/// scans after it only happen for panes that are not Claude.
pub fn for_shell(shell_pid: u32) -> Option<AgentSession> {
    if let Some(s) = crate::claude_session::session_for_shell(shell_pid) {
        return Some(AgentSession {
            agent: Agent::Claude,
            pid: Some(s.pid),
            id: s.id,
            name: s.name,
            peer_name: s.peer_name,
            config_dir: s.config_dir,
            background_shells: s.background_shells,
        });
    }
    if let Some(s) = crate::codex_session::for_shell(shell_pid) {
        return Some(AgentSession {
            agent: Agent::Codex,
            pid: None,
            id: s.id,
            name: s.name,
            peer_name: None,
            config_dir: None,
            background_shells: false,
        });
    }
    if let Some(s) = crate::omp_session::for_shell(shell_pid) {
        return Some(AgentSession {
            agent: Agent::Omp,
            pid: None,
            id: s.id,
            name: s.name,
            peer_name: None,
            config_dir: None,
            background_shells: false,
        });
    }
    if let Some(s) = crate::tazu_session::for_shell(shell_pid) {
        return Some(AgentSession {
            agent: Agent::Tazu,
            pid: None,
            id: s.id,
            name: s.name,
            peer_name: None,
            config_dir: None,
            background_shells: false,
        });
    }
    let s = crate::pi_session::for_shell(shell_pid)?;
    Some(AgentSession {
        agent: Agent::Pi,
        pid: None,
        id: s.id,
        name: s.name,
        peer_name: None,
        config_dir: None,
        background_shells: false,
    })
}

/// The command line that reopens this conversation, or `None` when the id is
/// not one we are willing to type into a shell.
///
/// `last_command` is the line the pane last ran: on the Claude and Pi sides it
/// carries the flags the session was started with, which the resume line reuses.
/// `config_dir` is Claude's non-default `CLAUDE_CONFIG_DIR`, ignored elsewhere.
pub fn resume_command(
    agent: Agent,
    session_id: &str,
    last_command: Option<&str>,
    config_dir: Option<&str>,
) -> Option<String> {
    match agent {
        Agent::Claude => crate::claude_session::resume_command(last_command, session_id, config_dir),
        Agent::Codex => crate::codex_session::resume_command(session_id),
        Agent::Pi => crate::pi_session::resume_command(last_command, session_id),
        Agent::Omp => crate::omp_session::resume_command(last_command, session_id),
        Agent::Tazu => crate::tazu_session::resume_command(last_command, session_id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_agent_resumes_with_its_own_command() {
        let id = "01a07651-015e-78a3-97f2-2eaf0f0cd663";
        assert_eq!(
            resume_command(Agent::Codex, id, None, None).as_deref(),
            Some("codex resume 01a07651-015e-78a3-97f2-2eaf0f0cd663")
        );
        let claude = resume_command(Agent::Claude, id, None, None).expect("a claude line");
        assert!(claude.starts_with("claude "), "got {}", claude);
        assert!(claude.contains(id));
        assert_eq!(
            resume_command(Agent::Pi, id, Some("pi"), None).as_deref(),
            Some("pi --session 01a07651-015e-78a3-97f2-2eaf0f0cd663")
        );
        assert_eq!(
            resume_command(Agent::Omp, id, Some("omp"), None).as_deref(),
            Some("omp --resume 01a07651-015e-78a3-97f2-2eaf0f0cd663")
        );
        assert_eq!(
            resume_command(Agent::Tazu, "20261101-132200-ab12", Some("tazu"), None).as_deref(),
            Some("tazu --resume 20261101-132200-ab12")
        );
    }

    #[test]
    fn an_unsafe_id_is_refused_for_both() {
        assert!(resume_command(Agent::Claude, "id\nrm -rf ~", None, None).is_none());
        assert!(resume_command(Agent::Codex, "id\nrm -rf ~", None, None).is_none());
        assert!(resume_command(Agent::Pi, "id\nrm -rf ~", None, None).is_none());
        assert!(resume_command(Agent::Omp, "id\nrm -rf ~", None, None).is_none());
        assert!(resume_command(Agent::Tazu, "id\nrm -rf ~", None, None).is_none());
    }

    #[test]
    fn the_agent_name_round_trips_through_the_session_file() {
        for agent in [Agent::Claude, Agent::Codex, Agent::Pi, Agent::Omp, Agent::Tazu] {
            let json = serde_json::to_string(&agent).unwrap();
            assert_eq!(json, format!("\"{}\"", agent.as_str()));
            assert_eq!(serde_json::from_str::<Agent>(&json).unwrap(), agent);
        }
    }
}

#[cfg(test)]
mod live_probe {
    /// Measures the real path on a live pane: the status bar's signal, as Kova
    /// reads it from the session registry. Needs a pane's shell pid.
    #[test]
    #[ignore = "needs KOVA_SHELL_PID, the shell of a pane running Claude Code"]
    fn prints_what_a_live_pane_reports() {
        let pid: u32 = std::env::var("KOVA_SHELL_PID").unwrap().parse().unwrap();
        let session = super::for_shell(pid).expect("an agent session under this shell");
        println!("agent={:?} background_shells={}", session.agent, session.background_shells);
    }
}

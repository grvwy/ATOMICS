//! Safe ticket data and requests carried by the `host` capability. Configuration, credentials and
//! operation arguments stay on the host; a client names only configured views and filters.
use serde::{Deserialize, Serialize};

use crate::tickets::{TicketCard, TicketLink, TicketMode};

pub fn request_id() -> Result<String, getrandom::Error> {
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TicketsInfo {
    pub system: String,
    pub identity: Option<String>,
    pub search: bool,
    pub semantic_search: bool,
    pub views: Vec<String>,
    pub default_view: usize,
    pub filters: Vec<String>,
    pub mode: TicketMode,
    pub states: crate::tickets::StateMap,
    /// Bounded completed requests. Each client consumes only its own request IDs.
    pub replies: Vec<TicketReply>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TicketTree {
    pub plan: String,
    pub ticket: Option<TicketLink>,
    pub tasks: Vec<TicketLink>,
    pub unassigned: Vec<TicketLink>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum TicketAction {
    Search { query: String, keyword: bool, view: usize, filters: Vec<usize>, cursor: Option<String> },
    Read { ticket: TicketLink },
    Children { ticket: TicketLink },
    Related { ticket: TicketLink },
    Plan { ticket: TicketLink, mode: TicketMode, extra: String },
    Link { plan: String, task: Option<String>, ticket: TicketLink },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TicketReply {
    pub request: String,
    pub cards: Vec<TicketCard>,
    pub next: Option<String>,
    pub fallback: Option<String>,
    pub error: Option<String>,
    pub plan: Option<String>,
}

impl TicketReply {
    pub fn new(request: String) -> Self {
        Self { request, cards: Vec::new(), next: None, fallback: None, error: None, plan: None }
    }
}

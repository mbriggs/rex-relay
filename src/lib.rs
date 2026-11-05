//! `rex-relay`: a guarded relay that drives Claude Code panes in Rex sessions over Rex's control
//! API (`GET /api/control`, subprotocol `rex.control.v1`, API revision 8).

pub mod audit;
pub mod cli;
pub mod commands;
pub mod config;
pub mod endpoint;
pub mod exit;
pub mod guard;
pub mod host;
pub mod ids;
pub mod output;
pub mod pane;
pub mod prompt;
pub mod registry;
pub mod rex;
pub mod status;
pub mod transport;

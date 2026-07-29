pub mod assets;
pub mod commands;
pub(crate) mod config;
pub mod docker;
pub mod git;
pub(crate) mod ssh;
pub mod template;
pub mod ui;
pub mod utils;

pub const SSH_AGENT_PORT: u16 = 2222;

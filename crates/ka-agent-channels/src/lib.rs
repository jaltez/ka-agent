//! Channel adapters. M2: Telegram (Bot API long polling in, throttled
//! messages + approval keyboards out). M4: the MCP bridge.

pub mod channel;
pub mod commands;
pub mod fake;
pub mod grants;
pub mod matrix;
pub mod mcp;
pub mod multi;
pub mod secret;
pub mod telegram;

pub use channel::Channel;
pub use matrix::{MatrixChannel, MatrixConfig, MatrixSurface};
pub use mcp::run_mcp;
pub use multi::MultiSurface;
pub use telegram::{TelegramChannel, TelegramConfig, TelegramSurface};

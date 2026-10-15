//! Channel adapters. M2: Telegram (Bot API long polling in, throttled
//! messages + approval keyboards out). M4: the MCP bridge.

pub mod fake;
pub mod mcp;
pub mod secret;
pub mod telegram;

pub use mcp::run_mcp;
pub use telegram::{TelegramChannel, TelegramConfig, TelegramSurface};

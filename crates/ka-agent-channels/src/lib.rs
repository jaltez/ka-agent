//! Channel adapters. M2: Telegram (Bot API long polling in, throttled
//! messages + approval keyboards out). M4: the MCP bridge.

pub(crate) mod attachments;
pub mod calendar;
pub mod channel;
pub mod commands;
pub mod fake;
pub mod grants;
pub mod matrix;
pub mod mcp;
pub mod multi;
pub mod pairing;
pub mod refs;
pub mod telegram;
pub mod voice;

pub use channel::Channel;
pub use commands::{cron_add, cron_list_text, fmt_interval, help_for, help_text};
pub use matrix::{MatrixChannel, MatrixConfig, MatrixSurface};
pub use mcp::run_mcp;
pub use multi::MultiSurface;
pub use telegram::{TelegramChannel, TelegramConfig, TelegramSurface};

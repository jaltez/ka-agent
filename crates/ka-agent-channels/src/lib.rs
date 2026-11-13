//! Channel adapters. M2: Telegram (Bot API long polling in, throttled
//! messages + approval keyboards out). M4: the MCP bridge.

pub(crate) mod attachments;
pub mod calendar;
pub mod channel;
pub mod commands;
pub(crate) mod errfmt;
pub mod fake;
pub mod discord;
pub mod fake_discord;
pub mod fake_slack;
pub mod grants;
pub mod kbsync;
pub mod matrix;
pub mod multi;
pub mod pairing;
pub mod ratelimit;
pub mod slack;
pub mod refs;
pub mod telegram;
pub mod tools;
pub mod voice;

pub use channel::Channel;
pub use commands::{cron_add, cron_list_text, fmt_interval, help_for, help_text};
pub use matrix::{MatrixChannel, MatrixConfig, MatrixSurface};
pub use multi::MultiSurface;
pub use telegram::{TelegramChannel, TelegramConfig, TelegramSurface};

//! Channel adapters. M2: Telegram (Bot API long polling in, throttled
//! messages + approval keyboards out).

pub mod fake;
pub mod secret;
pub mod telegram;

pub use telegram::{TelegramChannel, TelegramConfig, TelegramSurface};

//! Wire protocol conversion for provider-core.

#![forbid(unsafe_code)]

mod bridge;
mod claude;
mod claude_usage_observer;
mod openai_chat;
mod sse;
mod usage_observer;

pub use bridge::DefaultProtocolBridge;
pub use claude_usage_observer::observe_claude_messages_usage;
pub use usage_observer::{observe_chat_completions_usage, observe_responses_usage};

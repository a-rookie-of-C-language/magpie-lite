//! magpie-core: the protocol-neutral message IR and the wire codecs for
//! OpenAI Chat Completions and Anthropic Messages, streaming included.
//!
//! Every request is decoded into [`ir::CompletionRequest`]; every reply is
//! built from [`ir::CompletionResponse`]. A stream is decoded into
//! [`stream::StreamEvent`]s and encoded back out. New protocols plug in as
//! one decoder plus one encoder.

pub mod anthropic;
pub mod chat;
pub mod effort;
pub mod error;
pub mod ir;
pub mod responses;
pub mod search;
pub mod stream;

mod tool_choice;
mod tool_output;

pub use error::{CoreError, Result};
pub mod gemini;

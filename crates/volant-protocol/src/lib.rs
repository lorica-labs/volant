// SPDX-License-Identifier: GPL-3.0-or-later
//! Wire protocol between the Volant controller and its agent.
//!
//! Every message is one frame (see [`frame`]) whose payload is a JSON document.

pub mod encoding;
pub mod facts;
pub mod frame;
pub mod interpreter;
pub mod lines;
pub mod messages;
pub mod modules;
pub mod shape;

pub use messages::*;

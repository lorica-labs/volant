// SPDX-License-Identifier: GPL-3.0-or-later
//! Wire protocol between the Volant controller and its agent.
//!
//! Every message is one frame (see [`frame`]) whose payload is a JSON document.

pub mod frame;
pub mod messages;

pub use messages::*;

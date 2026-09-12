//! The MjolnirVSS window.
//!
//! Native Win32 controls, chosen deliberately: they work inside Windows PE,
//! they need no graphics driver beyond the basic display adapter a recovery
//! environment provides, they follow the user's text size and high contrast
//! settings, and they are reachable from the keyboard and by a screen reader
//! without any of that being implemented here.
//!
//! The interface is meant to be boring. A person whose computer has just failed
//! should not have to learn anything.
//!
//! This crate is a toolkit and nothing more. It knows about windows, controls,
//! fonts and worker threads, and deliberately not about backups or restores.
//! That is what lets the recovery application use it without dragging in the
//! shadow copy code, which must never be linked into a binary that has to start
//! inside Windows PE.
//!
//! Copyright (C) the MjolnirVSS contributors.
//! Licensed under the GNU General Public License, version 3 or later.

#![warn(missing_docs)]
#![cfg(windows)]

pub mod console;
pub mod message_box;
pub mod shell;
pub mod sys;
pub mod window;
pub mod worker;

pub use window::{Window, WindowConfig, WindowHandler};
pub use worker::{SharedProgress, Worker};

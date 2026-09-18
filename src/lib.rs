//! unit-lint — systemd units that are valid and still silently broken.
//!
//! `check` reads the units of a built NixOS system (and of its containers)
//! before they are deployed; `live` asks the running host what the files
//! cannot tell. Every rule stands for a failure that stayed invisible to
//! `systemctl --failed`, to a green deploy and to the service's own status.

pub mod config;
pub mod live;
pub mod report;
pub mod rules;
pub mod unit;

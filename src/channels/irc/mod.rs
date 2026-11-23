pub mod busy;
pub mod cli;
pub mod commands;
pub mod config;
pub mod doctor;
pub mod gateway;
#[allow(clippy::module_inception)]
pub mod irc;
pub mod lifecycle;
pub mod pairing;
pub mod runner;
pub mod service;
pub mod session;
pub mod setup;
pub mod sidecar;
pub mod store;

pub mod main;

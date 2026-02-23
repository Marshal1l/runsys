// src/cli.rs
use clap::{Parser, Subcommand, command};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "runsys")]
#[command(about = "A simple OCI-compatible runtime written in Rust")]
#[command(arg_required_else_help = true)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,
}
#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Create a new container (prepare state, do not start yet)
    Create {
        /// Container unique ID
        #[arg(required = true)]
        id: String,

        /// Path to OCI bundle directory (must contain config.json)
        #[arg(required = true)]
        bundle: PathBuf,
    },
    Start {
        /// Container unique ID
        #[arg(required = true)]
        id: String,
    },
}

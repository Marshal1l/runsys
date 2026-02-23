use clap::Parser;

use crate::cli::{Cli, Commands};
use crate::runtime::{Container, RuntimeError};

mod cli;
mod runtime;

fn main() -> Result<(), RuntimeError> {
    let cli = Cli::parse();
    match cli.command {
        Some(Commands::Create { id, bundle }) => {
            println!(
                "Creating container '{}' from bundle: {}",
                id,
                bundle.display()
            );
            let _container = Container::create(id, bundle)?;
            println!("Container created successfully!");
            Ok(())
        }
        Some(Commands::Start { id }) => {
            println!("Starting container '{}'", id);
            // 从 state.json 加载容器实例
            let mut container = Container::load(&id)?;
            // 调用 start 方法
            container.start()?;
            println!("Container '{}' started successfully", id);
            Ok(())
        }
        None => {
            println!("Run `myruntime --help` to see all available commands.");
            Ok(())
        }
    }
}

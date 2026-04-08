use anyhow::{Context, Result};
use clap::Parser;

use crate::cli::{Cli, Commands};

mod cli;
mod java;
mod kotlin;
mod manifest;
mod maven;
mod project;
mod status;
mod types;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let status_rx = status::StatusHandle::init();
    let progress = status::spawn_progress(status_rx);

    let result = run(cli).await;

    status::StatusHandle::get().shutdown();
    let _ = progress.await;

    if let Err(e) = &result {
        let msg = format!("{e:#}");
        if !msg.is_empty() {
            eprintln!("error: {msg}");
        }
    }

    std::process::exit(if result.is_ok() { 0 } else { 1 });
}

async fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Commands::Build(cmd) => {
            let b = &cmd.build_args;
            project::Project::new(&b.project_args, b.out.as_ref(), b.packaging)?
                .build()
                .await?;
            eprintln!();
        }
        Commands::Run(cmd) => {
            let b = &cmd.build_args;
            let mut project = project::Project::new(&b.project_args, b.out.as_ref(), b.packaging)?;
            let jar_path = project.build().await?;

            let native_dirs = project.native_library_dirs();
            eprintln!();

            if let Some(jar_path) = jar_path {
                project
                    .java()
                    .run_jar(&project.dir, &jar_path, &native_dirs, &cmd.args)?;
            } else {
                let entry = cmd
                    .entry
                    .as_ref()
                    .or(project.manifest.as_ref().and_then(|m| m.entry.as_ref()))
                    .context("no entry point specified and none found in manifest")?;

                project.java().run(
                    &project.dir,
                    &project.build_dir,
                    project.class_path_iter(),
                    entry,
                    &native_dirs,
                    &cmd.args,
                )?;
            }
            eprintln!();
        }
        Commands::Test(cmd) => {
            let b = &cmd.build_args;
            let mut project = project::Project::new(&b.project_args, b.out.as_ref(), b.packaging)?;
            project.test(&cmd).await?;
            eprintln!();
        }
        Commands::Clean(cmd) => {
            project::Project::new(&cmd.project_args, None, None)?.clean(cmd.purge)?;
        }
    }

    Ok(())
}

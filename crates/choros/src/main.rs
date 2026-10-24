mod app;
mod cli;
mod land_cli;
mod ls;
mod queue_tui;
mod settings_tui;
mod tui;
mod ui;

use choros_core::{agent, choros, git, json_diff, registry};

use color_eyre::Result;

fn main() -> Result<()> {
    color_eyre::install()?;
    let mut args = std::env::args().skip(1);
    let sub = args.next();
    let rest: Vec<String> = args.collect();
    match sub.as_deref() {
        None => {
            cli::print_usage();
            std::process::exit(2);
        }
        Some("work") => cli::run_work(rest),
        Some("init") => {
            reject_extra_args("init", &rest);
            cli::run_init()
        }
        Some("ls") => cli::run_ls(rest),
        Some("branch") => cli::run_branch(rest),
        Some("archive") => cli::run_archive(rest),
        Some("add") => cli::run_add(rest),
        Some("agent") => cli::run_agent(rest),
        Some("repo") => cli::run_repo(rest),
        Some("land") => land_cli::run_land(rest),
        Some("sync") => land_cli::run_sync(rest),
        Some("status") => land_cli::run_status(rest),
        Some("queue") => land_cli::run_queue(rest),
        Some("landing") => land_cli::run_landing(rest),
        Some("shell-init") => {
            reject_extra_args("shell-init", &rest);
            cli::emit_shell_init();
            Ok(())
        }
        Some("-h") | Some("--help") => {
            cli::print_usage();
            Ok(())
        }
        Some(other) => {
            eprintln!("unknown subcommand: {other}");
            cli::print_usage();
            std::process::exit(2);
        }
    }
}

fn reject_extra_args(sub: &str, rest: &[String]) {
    if !rest.is_empty() {
        eprintln!("`choros {sub}` takes no arguments, got: {}", rest.join(" "));
        cli::print_usage();
        std::process::exit(2);
    }
}

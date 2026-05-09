mod app;
mod choros;
mod cli;
mod git;
mod registry;
mod ui;

use color_eyre::Result;

fn main() -> Result<()> {
    color_eyre::install()?;
    let mut args = std::env::args().skip(1);
    let sub = args.next();
    let rest: Vec<String> = args.collect();
    match sub.as_deref() {
        None => cli::run_full_tui(),
        Some("work") => cli::run_work(rest),
        Some("init") => {
            reject_extra_args("init", &rest);
            cli::run_init()
        }
        Some("archive") => cli::run_archive(rest),
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

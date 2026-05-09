mod app;
mod cli;
mod git;
mod registry;
mod ui;
mod workspace;

use color_eyre::Result;

fn main() -> Result<()> {
    color_eyre::install()?;
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        None => cli::run_full_tui(),
        Some("work") => cli::run_work(args.collect()),
        Some("init") => cli::run_init(),
        Some("shell-init") => {
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

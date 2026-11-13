mod app;
mod cli;
mod here;
mod land_cli;
mod ls;
mod queue_tui;
mod set_graph;
mod settings_tui;
mod tui;
mod ui;

use choros_core::{agent, choros, git, json_diff, registry};

use color_eyre::Result;

fn main() {
    color_eyre::install().expect("installing the panic and error reports");
    if let Err(e) = run() {
        // The full report (where it was raised, a backtrace) is for debugging
        // choros, not for the person using it.
        if std::env::var_os("RUST_BACKTRACE").is_some() {
            eprintln!("{e:?}");
        } else {
            eprintln!("choros: {e}");
            for cause in e.chain().skip(1) {
                eprintln!("  because: {cause}");
            }
        }
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let mut args = std::env::args().skip(1).flat_map(split_flag);
    let sub = args.next();
    let rest: Vec<String> = args.collect();
    if let Some(sub) = sub.as_deref().filter(|s| !s.starts_with('-'))
        && rest.iter().any(|a| a == "--help" || a == "-h")
    {
        return cli::print_help(sub);
    }
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
        Some("hook") => land_cli::run_hook(rest),
        Some("events") => land_cli::run_events(rest),
        Some("trunks") => {
            reject_extra_args("trunks", &rest);
            cli::run_trunks()
        }
        Some("here") => match rest.as_slice() {
            [] => here::run(),
            [json] if json == "--json" => here::run_json(),
            _ => {
                eprintln!("usage: choros here [--json]");
                std::process::exit(2);
            }
        },
        Some("help") => match rest.as_slice() {
            [] => {
                cli::print_usage();
                Ok(())
            }
            [t] if t == "agent" => {
                print!("{}", cli::AGENT_DOC);
                Ok(())
            }
            [t] => cli::print_help(t),
            _ => {
                eprintln!("choros: `choros help` takes at most one topic, got: {}", rest.join(" "));
                std::process::exit(2);
            }
        },
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
            eprintln!("choros: no command `{other}`; `choros --help` lists them");
            std::process::exit(2);
        }
    }
}

/// `--flag=value` as `--flag value`, which is the form each command reads.
/// Only what is spelled like a flag is split: a title may hold `=` too.
fn split_flag(arg: String) -> Vec<String> {
    let flag = |f: &str| {
        f.strip_prefix("--")
            .is_some_and(|name| !name.is_empty() && name.chars().all(|c| c.is_ascii_lowercase() || c == '-'))
    };
    match arg.split_once('=') {
        Some((f, value)) if flag(f) => vec![f.to_string(), value.to_string()],
        _ => vec![arg],
    }
}

fn reject_extra_args(sub: &str, rest: &[String]) {
    if !rest.is_empty() {
        eprintln!("choros: `choros {sub}` takes no arguments, got: {}", rest.join(" "));
        std::process::exit(2);
    }
}

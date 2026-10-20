mod anim;
mod config;
mod dbus;
mod render;
mod ui;

use std::time::Duration;

use calloop::{ping::make_ping, EventLoop};
use calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::{
    compositor::CompositorState,
    shell::wlr_layer::LayerShell,
    shm::{slot::SlotPool, Shm},
};
use wayland_client::{globals::registry_queue_init, Connection};

use dbus::Dismiss;

const USAGE: &str = "\
usage: plush              run the daemon
       plush dismiss      close the notification on top of the stack
       plush dismiss all  close every notification on screen
       plush --help       print this
       plush --version    print the version";

const DISMISS_USAGE: &str = "\
usage: plush dismiss      close the notification on top of the stack
       plush dismiss all  close every notification on screen

`plush dismiss` takes the newest notification, which is the panel on top of the
stack. `plush dismiss all` takes everything currently on screen. Either way the
daemon announces NotificationClosed with reason 2, exactly as a click does.";

/// Which command's help was asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Topic {
    Plush,
    Dismiss,
}

/// What the command line asked for.
#[derive(Debug, PartialEq, Eq)]
enum Command {
    Daemon,
    Dismiss(Dismiss),
    Help(Topic),
    Version,
}

/// Splits `args` at the first `--`. What follows it is a word rather than a flag, so
/// `plush dismiss -- --help` is an argument and not the help flag.
fn split_terminator<'a>(args: &'a [&'a str]) -> (&'a [&'a str], &'a [&'a str]) {
    match args.iter().position(|arg| *arg == "--") {
        Some(at) => (&args[..at], &args[at + 1..]),
        None => (args, &[]),
    }
}

fn parse(args: &[&str]) -> Result<Command, String> {
    let (before, after) = split_terminator(args);

    // Flags are only ever first, and only before the terminator.
    if let Some(flag) = before.first() {
        match *flag {
            "-h" | "--help" => return Ok(Command::Help(Topic::Plush)),
            "-V" | "--version" => return Ok(Command::Version),
            _ => {}
        }
    }

    // The flags returned above, so every remaining word is a command or one of its
    // arguments: the command first, then whatever follows it across the terminator.
    let words: Vec<&str> = before.iter().chain(after.iter()).copied().collect();
    let (command, arguments): (Option<&str>, &[&str]) = match words.split_first() {
        Some((command, rest)) => (Some(*command), rest),
        None => (None, &[]),
    };

    match command {
        None => Ok(Command::Daemon),
        Some("dismiss") => {
            // `dismiss` takes at most one argument, so a flag and an argument can never be
            // interleaved: only a `--help` from before the terminator counts as the flag.
            if after.is_empty() && matches!(before.get(1).copied(), Some("-h" | "--help")) {
                return Ok(Command::Help(Topic::Dismiss));
            }
            match arguments {
                [] => Ok(Command::Dismiss(Dismiss::Newest)),
                ["all"] => Ok(Command::Dismiss(Dismiss::All)),
                // Two arms rather than one so the report names the argument that is actually
                // wrong: for `dismiss all extra` that is the `extra`, not the `all` that fitted.
                ["all", extra, ..] => {
                    Err(format!("`dismiss` takes nothing or `all`, not {extra:?}"))
                }
                [other, ..] => Err(format!("`dismiss` takes nothing or `all`, not {other:?}")),
            }
        }
        Some(other) => Err(format!("unknown command {other:?}")),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();

    match parse(&borrowed) {
        Ok(Command::Daemon) => run_daemon(),
        Ok(Command::Dismiss(what)) => {
            if let Err(err) = dbus::request_dismiss(what) {
                eprintln!("plush: {err}");
                std::process::exit(1);
            }
        }
        Ok(Command::Help(Topic::Plush)) => println!("{USAGE}"),
        Ok(Command::Help(Topic::Dismiss)) => println!("{DISMISS_USAGE}"),
        Ok(Command::Version) => println!("plush {}", env!("CARGO_PKG_VERSION")),
        Err(message) => {
            eprintln!("plush: {message}\n{USAGE}");
            std::process::exit(2);
        }
    }
}

fn run_daemon() {
    // Before anything else: a configuration file that exists but is wrong should
    // stop the daemon, not be quietly ignored.
    let config = match config::load() {
        Ok(config) => config,
        Err(err) => {
            eprintln!("plush: {err}");
            std::process::exit(1);
        }
    };

    let connection =
        Connection::connect_to_env().expect("plush: no Wayland compositor in this session");
    let (globals, event_queue) =
        registry_queue_init::<ui::Ui>(&connection).expect("plush: could not read the Wayland registry");

    let qh = event_queue.handle();
    let compositor =
        CompositorState::bind(&globals, &qh).expect("plush: wl_compositor is unavailable");
    let layer_shell = LayerShell::bind(&globals, &qh)
        .expect("plush: zwlr_layer_shell_v1 is unavailable (a wlroots compositor is required)");
    let shm = Shm::bind(&globals, &qh).expect("plush: wl_shm is unavailable");

    // Room for a spare buffer per card, each the panel plus its shadow margin. memfd
    // pages stay uncommitted until a card actually paints them, so oversizing this costs
    // address space, not memory.
    let (surface_w, surface_h) = render::surface(render::PANEL_W, render::MAX_H);
    let pool_size = surface_w as usize * surface_h as usize * 4 * render::MAX_CARDS * 2;
    let pool = SlotPool::new(pool_size, &shm).expect("plush: could not create the shm pool");

    let mut event_loop: EventLoop<'static, ui::Ui> =
        EventLoop::try_new().expect("plush: could not create the event loop");
    let handle = event_loop.handle();

    let (ping, ping_source) = make_ping().expect("plush: could not create the wakeup source");
    handle
        .insert_source(ping_source, |_, _, ui: &mut ui::Ui| ui.sync())
        .expect("plush: could not register the wakeup source");

    WaylandSource::new(connection, event_queue)
        .insert(handle.clone())
        .expect("plush: could not register the Wayland source");

    let store = dbus::shared();
    let closed = dbus::spawn(store.clone(), ping);

    let mut ui = ui::Ui::new(
        qh,
        handle.clone(),
        &globals,
        compositor,
        layer_shell,
        shm,
        pool,
        config,
        store,
        closed,
    );
    ui.sync();

    event_loop
        .run(None::<Duration>, &mut ui, |_| {})
        .expect("plush: event loop failed");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_arguments_runs_the_daemon() {
        assert_eq!(parse(&[]), Ok(Command::Daemon));
        assert_eq!(parse(&["dismiss"]), Ok(Command::Dismiss(Dismiss::Newest)));
        assert_eq!(parse(&["dismiss", "all"]), Ok(Command::Dismiss(Dismiss::All)));
    }

    /// A typo must not quietly start a second daemon.
    #[test]
    fn anything_else_is_an_error() {
        assert!(parse(&["dismss"]).is_err());
        assert!(parse(&["dismiss", "everything"]).is_err());
        assert!(parse(&["dismiss", "all", "now"]).is_err());
        assert!(parse(&["-v"]).is_err(), "only -V is the version");
    }

    #[test]
    fn help_and_version_are_commands_of_their_own() {
        assert_eq!(parse(&["-h"]), Ok(Command::Help(Topic::Plush)));
        assert_eq!(parse(&["--help"]), Ok(Command::Help(Topic::Plush)));
        assert_eq!(parse(&["-V"]), Ok(Command::Version));
        assert_eq!(parse(&["--version"]), Ok(Command::Version));
        // They win over whatever follows rather than being taken as stray arguments.
        assert_eq!(parse(&["--help", "dismiss"]), Ok(Command::Help(Topic::Plush)));
        assert_eq!(parse(&["--version", "junk"]), Ok(Command::Version));
    }

    #[test]
    fn a_subcommand_has_its_own_help() {
        assert_eq!(parse(&["dismiss", "-h"]), Ok(Command::Help(Topic::Dismiss)));
        assert_eq!(parse(&["dismiss", "--help"]), Ok(Command::Help(Topic::Dismiss)));
        // Help still wins over a stray argument behind it.
        assert_eq!(parse(&["dismiss", "--help", "all"]), Ok(Command::Help(Topic::Dismiss)));
    }

    /// Everything after `--` is a word, so it can no longer be read as a flag.
    #[test]
    fn the_terminator_ends_flag_parsing() {
        assert_eq!(parse(&["--"]), Ok(Command::Daemon));
        assert_eq!(parse(&["--", "dismiss"]), Ok(Command::Dismiss(Dismiss::Newest)));
        assert_eq!(parse(&["dismiss", "--"]), Ok(Command::Dismiss(Dismiss::Newest)));
        assert_eq!(parse(&["dismiss", "--", "all"]), Ok(Command::Dismiss(Dismiss::All)));

        assert!(parse(&["--", "--help"]).is_err(), "a word, not the flag");
        assert!(parse(&["dismiss", "--", "--help"]).is_err(), "a word, not the flag");
        assert!(parse(&["--", "--version"]).is_err());
    }

    /// A rejected argument must be the one that is wrong, not the one before it.
    #[test]
    fn the_error_names_the_argument_that_is_wrong() {
        let message = parse(&["dismiss", "all", "extra"]).unwrap_err();
        assert!(message.contains("extra"), "blames the good argument: {message}");
        assert!(!message.contains("\"all\""), "blames the good argument: {message}");

        let message = parse(&["dismiss", "extra", "all"]).unwrap_err();
        assert!(message.contains("extra"), "{message}");

        // And the terminator does not change which argument is reported.
        let message = parse(&["dismiss", "--", "all", "extra"]).unwrap_err();
        assert!(message.contains("extra"), "{message}");
    }

    /// The usage text is the help output, so every command has to be in the right one.
    #[test]
    fn usage_lists_every_command() {
        for expected in ["plush dismiss", "plush dismiss all", "--help", "--version"] {
            assert!(USAGE.contains(expected), "usage does not mention {expected}");
        }
        assert!(DISMISS_USAGE.contains("plush dismiss all"));
        assert!(DISMISS_USAGE.contains("NotificationClosed"));
    }
}

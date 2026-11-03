mod api;
mod app;
mod config;
mod db;
mod ui;

use std::io::{self, BufWriter, Write, stdout};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event};
use ratatui::crossterm::execute;

use api::{Api, spawn_worker};
use app::{App, KeyAction};

struct Args {
    dump: bool,
    db: Option<PathBuf>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = parse_args()?;
    let db_path = args.db.unwrap_or_else(db::default_db_path);
    if !db_path.exists() {
        return Err(format!("OpenCode database not found: {}", db_path.display()).into());
    }
    if args.dump {
        dump(&db_path)?;
        return Ok(());
    }
    run_tui(db_path)
}

fn parse_args() -> Result<Args, Box<dyn std::error::Error>> {
    let mut args = Args {
        dump: false,
        db: None,
    };
    let mut iter = std::env::args().skip(1);
    while let Some(argument) = iter.next() {
        match argument.as_str() {
            "--dump" => args.dump = true,
            "--db" => {
                let path = iter.next().ok_or("--db expects a path")?;
                args.db = Some(PathBuf::from(path));
            }
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            "-V" | "--version" => {
                println!("ocsess {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}").into()),
        }
    }
    Ok(args)
}

fn print_help() {
    println!(
        "ocsess {}\n\
         TUI to browse and manage OpenCode sessions.\n\n\
         USAGE:\n  ocsess [--db PATH] [--dump]\n\n\
         OPTIONS:\n\
           --db PATH   OpenCode SQLite database (default: auto-discover)\n\
           --dump      print sessions as TSV and exit\n\
           -h, --help  this help\n\
           -V          version\n\n\
         Keys: press ? in the TUI",
        env!("CARGO_PKG_VERSION")
    );
}

fn dump(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let mut sessions = db::load_sessions(path)?;
    sessions.sort_by_key(|session| std::cmp::Reverse(session.time_updated));
    let stdout = io::stdout();
    let mut out = BufWriter::new(stdout.lock());
    writeln!(
        out,
        "id\tstatus\ttitle\tproject\tdirectory\tagent\tmodel\tcost\ttokens\tmessages\tcreated\tupdated\tarchived\toutcome"
    )?;
    for session in &sessions {
        if let Err(error) = writeln!(
            out,
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{:.6}\t{}\t{}\t{}\t{}\t{}\t{}",
            session.id,
            session.status().label(),
            clean(&session.title),
            clean(&session.project),
            clean(&session.directory),
            clean(session.agent.as_deref().unwrap_or("")),
            clean(session.model.as_deref().unwrap_or("")),
            session.cost,
            session.total_tokens(),
            session.messages,
            session.time_created,
            session.time_updated,
            session
                .time_archived
                .map(|value| value.to_string())
                .unwrap_or_default(),
            clean(session.outcome.as_deref().unwrap_or("")),
        ) {
            if error.kind() == io::ErrorKind::BrokenPipe {
                return Ok(());
            }
            return Err(error.into());
        }
    }
    Ok(())
}

fn clean(text: &str) -> String {
    text.chars()
        .map(|character| {
            if matches!(character, '\t' | '\n' | '\r') {
                ' '
            } else {
                character
            }
        })
        .collect()
}

fn run_tui(db_path: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    let api = Arc::new(Api::new());
    let (command_tx, event_rx) = spawn_worker(api.clone());
    let mut app = App::new(db_path, config::Config::load(), api, command_tx, event_rx);

    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        ratatui::restore();
        let _ = execute!(stdout(), DisableMouseCapture);
        hook(info);
    }));
    let mut terminal = ratatui::init();
    let _guard = TerminalGuard;
    execute!(stdout(), EnableMouseCapture)?;

    loop {
        terminal.draw(|frame| ui::render(frame, &mut app))?;
        if event::poll(Duration::from_millis(200))? {
            match event::read()? {
                Event::Key(key) => match app.on_key(key) {
                    KeyAction::Quit => app.quit = true,
                    KeyAction::Open(index) => open_session(&mut terminal, &mut app, index)?,
                    KeyAction::None => {}
                },
                Event::Mouse(mouse) => {
                    if let Some(index) = app.on_mouse(mouse) {
                        open_session(&mut terminal, &mut app, index)?;
                    }
                }
                Event::Resize(_, _) => {}
                _ => {}
            }
        }
        app.tick();
        if app.quit {
            break;
        }
    }

    app.persist_now();
    Ok(())
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        ratatui::restore();
        let _ = execute!(stdout(), DisableMouseCapture);
    }
}

fn open_session(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    index: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let Some((id, directory, title)) = app.sessions.get(index).map(|session| {
        (
            session.id.clone(),
            session.directory.clone(),
            session.title.clone(),
        )
    }) else {
        return Ok(());
    };
    if !Path::new(&directory).is_dir() {
        app.notify(format!("Directory not found · {directory}"), true);
        return Ok(());
    }
    if app.is_current(&id) {
        app.notify("Current session · open it from its terminal", true);
        return Ok(());
    }

    execute!(stdout(), DisableMouseCapture)?;
    ratatui::restore();
    let result = Command::new("opencode")
        .arg("-s")
        .arg(&id)
        .arg(&directory)
        .status();
    *terminal = ratatui::init();
    execute!(stdout(), EnableMouseCapture)?;
    terminal.clear()?;

    match result {
        Ok(status) if status.success() => {
            app.notify(format!("Returned from \"{title}\""), false);
        }
        Ok(status) => {
            app.notify(format!("opencode exited · {status}"), true);
        }
        Err(error) => {
            app.notify(format!("Could not launch opencode · {error}"), true);
        }
    }
    app.reload();
    Ok(())
}

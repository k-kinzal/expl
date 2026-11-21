//! `expl-ui` — a live window onto an expl exploration.
//!
//! It only reads: the graph (nodes, edges, verdicts, and the trunk's
//! lineage of origins), the process table, and the event log, polled from
//! `.expl/expl.db`. Everything it shows is also
//! available from `expl graph show`, `expl ps`, and `expl event ls`; the
//! window just shows it at once and keeps it current.

mod layout;
mod snapshot;
mod theme;
mod view;

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;
use expl_core::Project;
use gpui::{
    App, AppContext as _, Application, Bounds, KeyBinding, TitlebarOptions, WindowBounds,
    WindowOptions, actions, point, px, size,
};

use crate::view::ExplView;

actions!(expl_ui, [Quit]);

/// Watch an expl exploration: the graph, node states, and agent processes.
#[derive(Debug, Parser)]
#[command(name = "expl-ui", version)]
struct Args {
    /// Run as if started in this directory.
    #[arg(short = 'C', long = "directory", value_name = "DIR")]
    directory: Option<PathBuf>,
    /// How often to re-read the database, in milliseconds.
    #[arg(long, default_value_t = 750, value_name = "MS")]
    interval: u64,
}

fn main() -> ExitCode {
    let args = Args::parse();
    let start = match args.directory {
        Some(dir) => dir,
        None => match std::env::current_dir() {
            Ok(dir) => dir,
            Err(error) => {
                eprintln!("expl-ui: current directory: {error}");
                return ExitCode::from(2);
            }
        },
    };
    let project = match Project::discover(&start) {
        Ok(project) => project,
        Err(error) => {
            eprintln!("expl-ui: {error}");
            return ExitCode::from(1);
        }
    };
    let interval = Duration::from_millis(args.interval.max(100));

    Application::new().run(move |cx: &mut App| {
        cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        let title = project.root().file_name().map_or_else(
            || "expl".to_owned(),
            |name| format!("expl — {}", name.to_string_lossy()),
        );
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                None,
                size(px(1360.0), px(820.0)),
                cx,
            ))),
            titlebar: Some(TitlebarOptions {
                title: Some(title.into()),
                appears_transparent: true,
                traffic_light_position: Some(point(px(16.0), px(15.0))),
            }),
            ..WindowOptions::default()
        };
        let opened = cx.open_window(options, |window, cx| {
            cx.new(|cx| ExplView::new(project.clone(), interval, window, cx))
        });
        if let Err(error) = opened {
            eprintln!("expl-ui: cannot open a window: {error}");
            cx.quit();
            return;
        }
        cx.activate(true);
    });
    ExitCode::SUCCESS
}

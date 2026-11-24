//! Native window host for a Spore web surface.
//!
//! The window owns the webview. The Spore owns the HTML. The URL is the only
//! boundary between them; no component handle, filesystem path or credential
//! crosses it.

use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use tao::{
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoop},
    window::WindowBuilder,
};
use wry::WebViewBuilder;

#[derive(Debug)]
struct Args {
    url: String,
    title: String,
    size: (u32, u32),
    devtools: bool,
}

fn usage() -> &'static str {
    "usage: loam-window --url URL [--title TITLE] [--size WxH] [--devtools]"
}

fn parse_args() -> Result<Args> {
    let mut url = None;
    let mut title = "Loam".to_string();
    let mut size = (1000, 700);
    let mut devtools = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--url" => url = Some(args.next().context("--url needs URL")?),
            "--title" => title = args.next().context("--title needs TITLE")?,
            "--size" => {
                let value = args.next().context("--size needs WxH")?;
                let (width, height) = value.split_once('x').context("--size needs WxH")?;
                size = (
                    width.parse().context("--size width")?,
                    height.parse().context("--size height")?,
                );
            }
            "--devtools" => devtools = true,
            "-h" | "--help" => {
                println!("{}", usage());
                std::process::exit(0);
            }
            other => bail!("unknown option {other}"),
        }
    }
    Ok(Args {
        url: url.context("--url is required")?,
        title,
        size,
        devtools,
    })
}

fn run(args: Args) -> Result<()> {
    let event_loop = EventLoop::new();
    let window = WindowBuilder::new()
        .with_title(args.title)
        .with_inner_size(tao::dpi::LogicalSize::new(args.size.0, args.size.1))
        .build(&event_loop)
        .context("create native window")?;
    let mut builder = WebViewBuilder::new().with_url(args.url);
    if args.devtools {
        builder = builder.with_devtools(true);
    }
    builder = builder.with_navigation_handler(|url| {
        // A launcher asks its host window to go away after it has handed the
        // app to a new window. No other scheme is intercepted.
        if url == "loam://close" || url == "mycelium://close" {
            std::process::exit(0);
        }
        true
    });
    let webview = builder.build(&window).context("create webview")?;
    let _ = &webview;
    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        if let Event::WindowEvent {
            event: WindowEvent::CloseRequested,
            ..
        } = event
        {
            *control_flow = ControlFlow::Exit;
        }
    });
}

fn main() -> ExitCode {
    match parse_args().and_then(run) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("loam-window: {error:#}");
            ExitCode::FAILURE
        }
    }
}

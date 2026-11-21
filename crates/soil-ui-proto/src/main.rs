// SPDX-License-Identifier: MIT OR Apache-2.0
//! A throwaway host for the first Lisp -> HTML/CSS UI slice.
//!
//! The contract this prototype is testing:
//!
//! ```text
//! (initial-state)            -> value
//! (update state action)      -> value
//! (view state)               -> UI form
//! (styles)                   -> CSS text
//! ```
//!
//! Lisp never touches the DOM. It emits a tree of records:
//!
//! ```lisp
//! ((:tag "div") (:attrs ((:class "app"))) (:children (...)))
//! ((:text "Hello"))
//! ```
//!
//! This binary is a dev server, not the real `soil:ui` host. It exists to find
//! out which parts of the loop want to be Rust/WIT and which are just Lisp.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use soil_lang::sexp::Value;
use soil_lang::{FakeHost, Runtime, to_json};

const INDEX_HTML: &str = include_str!("../web/index.html");
const UI_JS: &str = include_str!("../web/ui.js");
const MAX_BODY: usize = 1 << 20;

struct App {
    source: PathBuf,
    prelude: Option<PathBuf>,
}

fn main() -> Result<()> {
    let mut source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("counter.soil");
    let mut prelude = None;
    let mut port = 7878u16;
    let mut render = false;
    let mut state = None;
    let mut action = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--source" => source = PathBuf::from(next(&mut args, "--source")?),
            "--prelude" => prelude = Some(PathBuf::from(next(&mut args, "--prelude")?)),
            "--port" => port = next(&mut args, "--port")?.parse()?,
            "--state" => state = Some(next(&mut args, "--state")?.parse::<i64>()?),
            "--action" => action = Some(next(&mut args, "--action")?),
            "--render" => render = true,
            "-h" | "--help" => {
                println!(
                    "usage: soil-ui-proto [--source FILE.soil] [--prelude FILE] \
                     [--port N] [--render [--state N] [--action NAME]]"
                );
                return Ok(());
            }
            other => bail!("unknown argument `{other}`"),
        }
    }

    let app = Arc::new(App { source, prelude });

    if render {
        let value = render_json(&app, state, action.as_deref())?;
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }

    let listener =
        TcpListener::bind(("127.0.0.1", port)).with_context(|| format!("bind 127.0.0.1:{port}"))?;
    println!("soil-ui-proto: http://127.0.0.1:{port}/");
    println!("source: {}", app.source.display());
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(stream) => stream,
            Err(error) => {
                eprintln!("accept: {error}");
                continue;
            }
        };
        let app = Arc::clone(&app);
        std::thread::spawn(move || {
            if let Err(error) = handle(stream, &app) {
                eprintln!("request: {error:#}");
            }
        });
    }
    Ok(())
}

fn next(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<String> {
    args.next()
        .ok_or_else(|| anyhow::anyhow!("{flag} needs a value"))
}

fn build_runtime(app: &App) -> Result<Runtime> {
    let mut runtime = Runtime::new(Box::new(FakeHost::new().deny_all()));
    if let Some(prelude) = &app.prelude {
        runtime
            .load_file(prelude)
            .with_context(|| format!("load prelude {}", prelude.display()))?;
    }
    runtime
        .load_file(&app.source)
        .with_context(|| format!("load {}", app.source.display()))?;
    Ok(runtime)
}

fn render_json(app: &App, state: Option<i64>, action: Option<&str>) -> Result<serde_json::Value> {
    let mut runtime = build_runtime(app)?;

    let mut state = match state {
        Some(value) => Value::Int(value),
        None => runtime
            .call("initial-state", &[])
            .context("call (initial-state)")?,
    };

    if let Some(action) = action {
        state = runtime
            .call("update", &[state, Value::String(action.to_string())])
            .with_context(|| format!("call (update state \"{action}\")"))?;
    }

    let tree = runtime
        .call("view", &[state.clone()])
        .context("call (view state)")?;
    let css = runtime.call("styles", &[]).context("call (styles)")?;
    let Value::String(css) = css else {
        bail!("(styles) must return a string, got {css}");
    };

    Ok(serde_json::json!({
        "state": to_json(&state),
        "tree": to_json(&tree),
        "css": css,
    }))
}

fn handle(mut stream: TcpStream, app: &App) -> Result<()> {
    let read_stream = stream.try_clone()?;
    let mut reader = BufReader::new(read_stream);

    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("/");

    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().unwrap_or(0);
            }
        }
    }
    if content_length > MAX_BODY {
        return respond(&mut stream, 413, "text/plain", b"body too large");
    }
    let mut body = vec![0u8; content_length];
    reader.read_exact(&mut body)?;

    let (path, query) = target.split_once('?').unwrap_or((target, ""));

    match (method, path) {
        ("GET", "/") => respond(
            &mut stream,
            200,
            "text/html; charset=utf-8",
            INDEX_HTML.as_bytes(),
        ),
        ("GET", "/ui.js") => respond(
            &mut stream,
            200,
            "text/javascript; charset=utf-8",
            UI_JS.as_bytes(),
        ),
        ("GET", "/source") => match fs::read(&app.source) {
            Ok(bytes) => respond(&mut stream, 200, "text/plain; charset=utf-8", &bytes),
            Err(error) => respond_error(&mut stream, 500, &format!("read source: {error}")),
        },
        ("POST", "/source") => {
            let text = String::from_utf8_lossy(&body);
            match write_source(&app.source, &text) {
                Ok(()) => respond(&mut stream, 200, "application/json", br#"{"ok":true}"#),
                Err(error) => respond_error(&mut stream, 500, &format!("{error:#}")),
            }
        }
        ("GET", "/view") => {
            let state = query_param(query, "state").and_then(|value| value.parse::<i64>().ok());
            let action = query_param(query, "action");
            match render_json(app, state, action.as_deref()) {
                Ok(value) => {
                    let body = serde_json::to_vec(&value)?;
                    respond(&mut stream, 200, "application/json", &body)
                }
                Err(error) => respond_error(&mut stream, 422, &format!("{error:#}")),
            }
        }
        ("GET", "/health") => respond(&mut stream, 200, "text/plain", b"ok"),
        _ => respond(&mut stream, 404, "text/plain", b"not found"),
    }
}

fn write_source(path: &Path, text: &str) -> Result<()> {
    let temporary = path.with_extension("soil.tmp");
    fs::write(&temporary, text).with_context(|| format!("write {}", temporary.display()))?;
    fs::rename(&temporary, path).with_context(|| format!("rename into {}", path.display()))?;
    Ok(())
}

fn query_param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        (name == key).then(|| percent_decode(value))
    })
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => match (hex(bytes[index + 1]), hex(bytes[index + 2]))
            {
                (Some(high), Some(low)) => {
                    out.push(high * 16 + low);
                    index += 3;
                }
                _ => {
                    out.push(b'%');
                    index += 1;
                }
            },
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn respond(stream: &mut TcpStream, status: u16, content_type: &str, body: &[u8]) -> Result<()> {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        413 => "Payload Too Large",
        422 => "Unprocessable Entity",
        _ => "Internal Server Error",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-store\r\n\
         Connection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    stream.flush()?;
    Ok(())
}

fn respond_error(stream: &mut TcpStream, status: u16, message: &str) -> Result<()> {
    let body = serde_json::json!({ "error": message });
    respond(
        stream,
        status,
        "application/json",
        &serde_json::to_vec(&body)?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> App {
        App {
            source: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("counter.soil"),
            prelude: None,
        }
    }

    #[test]
    fn initial_state_and_view() {
        let value = render_json(&app(), None, None).unwrap();
        assert_eq!(value["state"], serde_json::json!(0));
        assert_eq!(value["tree"]["tag"], serde_json::json!("div"));
        assert!(value["css"].as_str().unwrap().contains(".app"));
    }

    #[test]
    fn actions_transition_state() {
        let inc = render_json(&app(), Some(5), Some("inc")).unwrap();
        assert_eq!(inc["state"], serde_json::json!(6));

        let dec = render_json(&app(), Some(5), Some("dec")).unwrap();
        assert_eq!(dec["state"], serde_json::json!(4));

        let reset = render_json(&app(), Some(5), Some("reset")).unwrap();
        assert_eq!(reset["state"], serde_json::json!(0));

        let unknown = render_json(&app(), Some(5), Some("wat")).unwrap();
        assert_eq!(unknown["state"], serde_json::json!(5));
    }

    #[test]
    fn view_reflects_state() {
        let value = render_json(&app(), Some(42), None).unwrap();
        let text = serde_json::to_string(&value["tree"]).unwrap();
        assert!(text.contains("42"));
    }
}

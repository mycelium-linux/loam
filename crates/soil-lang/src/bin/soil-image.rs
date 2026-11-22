// SPDX-License-Identifier: MIT OR Apache-2.0
//! A session-scoped image server over stdin or a local Unix socket.
//!
//! One process holds one `Runtime` per session. Source is read and written
//! through `NativeHost`'s VFS seam, so the image never needs a host path.
//!
//! Session:
//!   {"op":"attach","session":"s1","workspace":"/vfs","records":"/records"}
//!   {"op":"load","session":"s1","path":"lib/foo.soil"}
//!   {"op":"eval","session":"s1","source":"(defun f (x) x)"}
//!   {"op":"status","session":"s1"}
//!   {"op":"flush","session":"s1","dry_run":true}
//!   {"op":"flush_new","session":"s1","path":"scratch.soil","names":[...]}
//!   {"op":"list"}
//!   {"op":"detach","session":"s1"}
//! Response: {"ok":true,"result":...} or {"ok":false,"error":"..."}
//!
//! `name` is accepted as a deprecated alias for `session` so the v1 stdin
//! protocol keeps working while clients move to session-scoped images.

use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::sync::Arc;

use serde_json::{Value as Json, json};
use soil_lang::eval::CompiledFunction;
use soil_lang::sexp::Value;
use soil_lang::{NativeHost, Runtime};
use wasmtime::{Engine, Instance, Module, Store, Val};

struct Image {
    runtime: Runtime,
    loaded: HashMap<String, String>,
    desktop: Option<String>,
    project: Option<String>,
    workspace: String,
    records: String,
    book: String,
    engine: Engine,
    modules: HashMap<String, Arc<Module>>,
}

fn prelude() -> Result<String, String> {
    let path = std::env::var("SOIL_PRELUDE").unwrap_or_else(|_| {
        std::env::current_dir()
            .unwrap_or_else(|_| ".".into())
            .join("crates/soil-lang/stdlib/prelude.soil")
            .display()
            .to_string()
    });
    std::fs::read_to_string(&path).map_err(|error| format!("read prelude {path}: {error}"))
}

impl Image {
    fn remember(&mut self, path: &str) -> anyhow::Result<()> {
        let hash = self.runtime.source_hash(path)?;
        self.loaded.insert(path.to_string(), hash);
        Ok(())
    }
}

fn result_text(value: &Value) -> String {
    if matches!(value, Value::Nil) {
        "nil".to_string()
    } else {
        value.to_string()
    }
}

fn flush_json(report: soil_lang::eval::FlushReport) -> Json {
    json!({
        "files": report.files.into_iter().map(|file| json!({
            "path": file.path,
            "definitions": file.definitions,
            "source": file.source,
        })).collect::<Vec<_>>(),
        "conflicts": report.conflicts,
    })
}

fn text_field<'a>(request: &'a Json, name: &str) -> &'a str {
    request.get(name).and_then(Json::as_str).unwrap_or_default()
}

/// The image key. `name` is the v1 spelling; `session` is the model spelling.
fn session_of(request: &Json) -> &str {
    let session = text_field(request, "session");
    if session.is_empty() {
        text_field(request, "name")
    } else {
        session
    }
}

fn form_returns_bool(form: &Value) -> bool {
    let Some(items) = form.as_list() else {
        return false;
    };
    if items.first().and_then(Value::as_symbol) != Some("defun") {
        return false;
    }
    items.get(3).is_some_and(expr_returns_bool)
}

fn expr_returns_bool(value: &Value) -> bool {
    let Some(items) = value.as_list() else {
        return false;
    };
    let Some(head) = items.first().and_then(Value::as_symbol) else {
        return false;
    };
    match head {
        "=" | ">" | "<" | ">=" | "<=" | "not" | "and" | "or" => true,
        "if" => {
            items.get(2).is_some_and(expr_returns_bool)
                && items.get(3).is_some_and(expr_returns_bool)
        }
        "cond" => items[1..].iter().all(|clause| {
            clause
                .as_list()
                .and_then(|parts| parts.get(1))
                .is_some_and(expr_returns_bool)
        }),
        "let" => items
            .last()
            .is_some_and(|body| body != value && expr_returns_bool(body)),
        _ => false,
    }
}

struct WasmFunction {
    engine: Engine,
    module: Arc<Module>,
    function: String,
    bool_result: bool,
}

impl CompiledFunction for WasmFunction {
    fn call(&mut self, args: &[Value]) -> anyhow::Result<Option<Value>> {
        let params = match args
            .iter()
            .map(|value| match value {
                Value::Int(value) => Some(Val::I64(*value)),
                Value::Bool(value) => Some(Val::I64(i64::from(*value))),
                _ => None,
            })
            .collect::<Option<Vec<_>>>()
        {
            Some(params) => params,
            None => return Ok(None),
        };
        let mut store = Store::new(&self.engine, ());
        let instance = Instance::new(&mut store, &self.module, &[])?;
        let Some(function) = instance.get_func(&mut store, &self.function) else {
            return Ok(None);
        };
        let ty = function.ty(&store);
        if ty.params().len() != params.len() {
            return Ok(None);
        }
        let mut results = vec![Val::I64(0); ty.results().len()];
        function.call(&mut store, &params, &mut results)?;
        let value = match results.first() {
            Some(Val::I64(value)) => *value,
            _ => return Ok(None),
        };
        Ok(Some(if self.bool_result {
            Value::Bool(value != 0)
        } else {
            Value::Int(value)
        }))
    }
}

fn dispatch(images: &mut HashMap<String, Image>, prelude: &str, request: &Json) -> Json {
    let op = text_field(request, "op");
    let session = session_of(request).to_string();

    let response: Result<Json, String> = match op {
        "ping" => Ok(json!({"pong": true})),
        "list" => {
            let mut sessions: Vec<String> = images.keys().cloned().collect();
            sessions.sort();
            Ok(json!({"sessions": sessions, "images": sessions}))
        }
        "attach" => {
            if session.is_empty() {
                Err("attach needs session".to_string())
            } else if let Some(image) = images.get(&session) {
                // Attaching again is how a second agent joins the same
                // session. It must not replace the live definitions.
                Ok(json!({
                    "session": session,
                    "image": session,
                    "existing": true,
                    "desktop": image.desktop,
                    "project": image.project,
                }))
            } else {
                (|| -> Result<Json, String> {
                    let workspace = text_field(request, "workspace");
                    let records = text_field(request, "records");
                    let book = request.get("book").and_then(Json::as_str).unwrap_or("soil");
                    if workspace.is_empty() {
                        return Err("attach needs workspace".to_string());
                    }
                    if records.is_empty() {
                        return Err("attach needs records".to_string());
                    }
                    let host = NativeHost::open(workspace, records, book)
                        .map_err(|error| format!("open host: {error:#}"))?;
                    let mut runtime = Runtime::new(Box::new(host));
                    runtime
                        .load(prelude)
                        .map_err(|error| format!("load prelude: {error:#}"))?;
                    runtime.clear_provenance();
                    let desktop = request
                        .get("desktop")
                        .and_then(Json::as_str)
                        .map(str::to_string);
                    let project = request
                        .get("project")
                        .and_then(Json::as_str)
                        .map(str::to_string);
                    images.insert(
                        session.clone(),
                        Image {
                            runtime,
                            loaded: HashMap::new(),
                            desktop: desktop.clone(),
                            project: project.clone(),
                            workspace: workspace.to_string(),
                            records: records.to_string(),
                            book: book.to_string(),
                            engine: Engine::default(),
                            modules: HashMap::new(),
                        },
                    );
                    Ok(json!({
                        "session": session,
                        "image": session,
                        "existing": false,
                        "desktop": desktop,
                        "project": project,
                    }))
                })()
            }
        }
        "detach" => {
            if session.is_empty() {
                Err("detach needs session".to_string())
            } else {
                images.remove(&session);
                Ok(json!({"detached": session, "session": session}))
            }
        }
        "compile" => (|| -> Result<Json, String> {
            let source = text_field(request, "source").to_string();
            let image = images
                .get(&session)
                .ok_or_else(|| format!("unknown session {session}"))?;
            if source.trim().is_empty() {
                return Err("compile needs source".to_string());
            }
            let program = soil_lang::parse(&source).map_err(|error| format!("{error:#}"))?;
            let functions: Vec<String> = program
                .functions
                .iter()
                .map(|function| function.name.clone())
                .collect();
            let wasm = soil_lang::compile(&program).map_err(|error| format!("{error:#}"))?;
            let hash = blake3::hash(&wasm).to_hex().to_string();
            let path = request
                .get("path")
                .and_then(Json::as_str)
                .filter(|path| !path.trim().is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("compiled/{hash}.wasm"));
            let mut writer = NativeHost::open(&image.workspace, &image.records, &image.book)
                .map_err(|error| format!("open host: {error:#}"))?;
            writer
                .write_bytes(&path, &wasm, "application/wasm")
                .map_err(|error| format!("write {path}: {error:#}"))?;
            Ok(json!({
                "session": session,
                "path": path,
                "bytes": wasm.len(),
                "functions": functions,
                "hash": hash,
            }))
        })(),
        "call-compiled" => (|| -> Result<Json, String> {
            let function = text_field(request, "function").to_string();
            if function.is_empty() {
                return Err("call-compiled needs function".to_string());
            }
            let args: Vec<i64> = request
                .get("args")
                .and_then(Json::as_array)
                .map(|args| {
                    args.iter()
                        .map(|arg| {
                            arg.as_i64()
                                .ok_or_else(|| "call-compiled args must be i64".to_string())
                        })
                        .collect::<Result<Vec<_>, _>>()
                })
                .transpose()?
                .unwrap_or_default();
            let image = images
                .get_mut(&session)
                .ok_or_else(|| format!("unknown session {session}"))?;
            let source = text_field(request, "source").to_string();
            let requested_path = request
                .get("path")
                .and_then(Json::as_str)
                .filter(|path| !path.trim().is_empty())
                .map(str::to_string);
            let (wasm, path) = if !source.trim().is_empty() {
                let program = soil_lang::parse(&source).map_err(|error| format!("{error:#}"))?;
                let wasm = soil_lang::compile(&program).map_err(|error| format!("{error:#}"))?;
                let hash = blake3::hash(&wasm).to_hex().to_string();
                let path = requested_path.unwrap_or_else(|| format!("compiled/{hash}.wasm"));
                let mut writer = NativeHost::open(&image.workspace, &image.records, &image.book)
                    .map_err(|error| format!("open host: {error:#}"))?;
                writer
                    .write_bytes(&path, &wasm, "application/wasm")
                    .map_err(|error| format!("write {path}: {error:#}"))?;
                (wasm, path)
            } else {
                let path = requested_path
                    .ok_or_else(|| "call-compiled needs source or path".to_string())?;
                let mut reader = NativeHost::open(&image.workspace, &image.records, &image.book)
                    .map_err(|error| format!("open host: {error:#}"))?;
                let wasm = reader
                    .read_bytes(&path)
                    .map_err(|error| format!("read {path}: {error:#}"))?;
                (wasm, path)
            };
            let hash = blake3::hash(&wasm).to_hex().to_string();
            if !image.modules.contains_key(&hash) {
                let module = Arc::new(
                    Module::new(&image.engine, &wasm).map_err(|error| format!("{error:#}"))?,
                );
                image.modules.insert(hash.clone(), module);
            }
            let module = image
                .modules
                .get(&hash)
                .ok_or_else(|| "compiled module vanished".to_string())?;
            let mut store = Store::new(&image.engine, ());
            let instance = Instance::new(&mut store, module.as_ref(), &[])
                .map_err(|error| format!("instantiate {path}: {error:#}"))?;
            let func = instance
                .get_func(&mut store, &function)
                .ok_or_else(|| format!("compiled module has no function {function}"))?;
            let ty = func.ty(&store);
            let params: Vec<Val> = args.iter().copied().map(Val::I64).collect();
            if params.len() != ty.params().len() {
                return Err(format!(
                    "compiled function expects {} args, got {}",
                    ty.params().len(),
                    params.len()
                ));
            }
            let mut results = vec![Val::I64(0); ty.results().len()];
            func.call(&mut store, &params, &mut results)
                .map_err(|error| format!("call compiled: {error:#}"))?;
            let result = match results.first() {
                Some(Val::I64(value)) => json!(value),
                Some(other) => return Err(format!("unexpected compiled result {other:?}")),
                None => Json::Null,
            };
            Ok(json!({
                "session": session,
                "path": path,
                "hash": hash,
                "function": function,
                "args": args,
                "result": result,
                "compiled": true,
            }))
        })(),
        "compile-function" => (|| -> Result<Json, String> {
            let name = text_field(request, "name").to_string();
            if name.is_empty() {
                return Err("compile-function needs name".to_string());
            }
            let image = images
                .get_mut(&session)
                .ok_or_else(|| format!("unknown session {session}"))?;
            let source = image
                .runtime
                .numeric_source(&name)
                .ok_or_else(|| format!("unknown definition {name}"))?;
            let program = soil_lang::parse(&source).map_err(|error| format!("{error:#}"))?;
            let wasm = soil_lang::compile(&program).map_err(|error| format!("{error:#}"))?;
            let hash = blake3::hash(&wasm).to_hex().to_string();
            let path = format!("compiled/{hash}.wasm");
            if !image.modules.contains_key(&hash) {
                let module = Arc::new(
                    Module::new(&image.engine, &wasm).map_err(|error| format!("{error:#}"))?,
                );
                image.modules.insert(hash.clone(), module);
                let mut writer = NativeHost::open(&image.workspace, &image.records, &image.book)
                    .map_err(|error| format!("open host: {error:#}"))?;
                writer
                    .write_bytes(&path, &wasm, "application/wasm")
                    .map_err(|error| format!("write {path}: {error:#}"))?;
            }
            let module = image
                .modules
                .get(&hash)
                .cloned()
                .ok_or_else(|| "compiled module vanished".to_string())?;
            let bool_result = image
                .runtime
                .definition_form(&name)
                .is_some_and(form_returns_bool);
            image.runtime.install_compiled(
                &name,
                Box::new(WasmFunction {
                    engine: image.engine.clone(),
                    module,
                    function: name.clone(),
                    bool_result,
                }),
            );
            Ok(json!({
                "session": session,
                "name": name,
                "path": path,
                "hash": hash,
                "bytes": wasm.len(),
                "bool_result": bool_result,
                "installed": true,
            }))
        })(),
        "load" => {
            let path = text_field(request, "path").to_string();
            images
                .get_mut(&session)
                .ok_or_else(|| format!("unknown session {session}"))
                .and_then(|image| {
                    image
                        .runtime
                        .load_vfs(&path)
                        .and_then(|value| {
                            image.remember(&path)?;
                            Ok(json!({
                                "session": session,
                                "loaded": path,
                                "result": result_text(&value),
                            }))
                        })
                        .map_err(|error| format!("{error:#}"))
                })
        }
        "eval" => {
            let source = text_field(request, "source").to_string();
            images
                .get_mut(&session)
                .ok_or_else(|| format!("unknown session {session}"))
                .and_then(|image| {
                    image
                        .runtime
                        .load(&source)
                        .map(|value| {
                            json!({
                                "session": session,
                                "result": result_text(&value),
                            })
                        })
                        .map_err(|error| format!("{error:#}"))
                })
        }
        "reload" => images
            .get_mut(&session)
            .ok_or_else(|| format!("unknown session {session}"))
            .and_then(|image| {
                let paths: Vec<String> = image.loaded.keys().cloned().collect();
                let mut reloaded = Vec::new();
                for path in paths {
                    let current = image
                        .runtime
                        .source_hash(&path)
                        .map_err(|error| format!("{error:#}"))?;
                    if image.loaded.get(&path) == Some(&current) {
                        continue;
                    }
                    image
                        .runtime
                        .load_vfs(&path)
                        .map_err(|error| format!("reload {path}: {error:#}"))?;
                    image.loaded.insert(path.clone(), current);
                    reloaded.push(path);
                }
                Ok(json!({"session": session, "reloaded": reloaded}))
            }),
        "status" => images
            .get_mut(&session)
            .ok_or_else(|| format!("unknown session {session}"))
            .map(|image| {
                json!({
                    "session": session,
                    "desktop": image.desktop,
                    "project": image.project,
                    "dirty": image.runtime.dirty_definitions(),
                    "unattached": image.runtime.unattached_definitions(),
                    "compiled": image.runtime.compiled_definitions(),
                    "loaded": image.loaded.keys().cloned().collect::<Vec<_>>(),
                })
            }),
        "flush" => {
            let dry_run = request
                .get("dry_run")
                .and_then(Json::as_bool)
                .unwrap_or(false);
            images
                .get_mut(&session)
                .ok_or_else(|| format!("unknown session {session}"))
                .and_then(|image| {
                    let report = if dry_run {
                        image.runtime.flush_preview()
                    } else {
                        image.runtime.flush_apply()
                    }
                    .map_err(|error| format!("{error:#}"))?;
                    if !dry_run {
                        for file in &report.files {
                            image
                                .remember(&file.path)
                                .map_err(|error| format!("{error:#}"))?;
                        }
                    }
                    Ok(flush_json(report))
                })
        }
        "flush_new" => {
            let path = text_field(request, "path").to_string();
            let names: Vec<String> = request
                .get("names")
                .and_then(Json::as_array)
                .map(|names| {
                    names
                        .iter()
                        .filter_map(Json::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            images
                .get_mut(&session)
                .ok_or_else(|| format!("unknown session {session}"))
                .and_then(|image| {
                    let file = image
                        .runtime
                        .flush_unattached(&path, &names)
                        .map_err(|error| format!("{error:#}"))?;
                    image
                        .remember(&file.path)
                        .map_err(|error| format!("{error:#}"))?;
                    Ok(json!({
                        "session": session,
                        "path": file.path,
                        "definitions": file.definitions,
                        "source": file.source,
                    }))
                })
        }
        other => Err(format!("unknown op {other}")),
    };

    match response {
        Ok(result) => json!({"ok": true, "result": result}),
        Err(error) => json!({"ok": false, "error": error}),
    }
}

fn stdin_loop(prelude: &str) {
    let mut images: HashMap<String, Image> = HashMap::new();
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(line) => line,
            Err(error) => {
                let _ = writeln!(
                    stdout,
                    "{}",
                    json!({"ok": false, "error": error.to_string()})
                );
                continue;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let request: Json = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(error) => {
                let _ = writeln!(
                    stdout,
                    "{}",
                    json!({"ok": false, "error": format!("json: {error}")})
                );
                continue;
            }
        };
        let output = dispatch(&mut images, prelude, &request);
        let _ = writeln!(stdout, "{output}");
        let _ = stdout.flush();
    }
}

#[cfg(unix)]
mod socket {
    use super::{Image, dispatch};
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::Path;
    use std::sync::mpsc;
    use std::thread;

    use serde_json::{Value as Json, json};

    struct Job {
        request: Json,
        reply: mpsc::Sender<Json>,
    }

    fn client(stream: UnixStream, jobs: mpsc::Sender<Job>) {
        let Ok(mut writer) = stream.try_clone() else {
            return;
        };
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            if line.trim().is_empty() {
                continue;
            }
            let request: Json = match serde_json::from_str(line.trim()) {
                Ok(request) => request,
                Err(error) => {
                    let response = json!({"ok": false, "error": format!("json: {error}")});
                    if writeln!(writer, "{response}")
                        .and_then(|()| writer.flush())
                        .is_err()
                    {
                        break;
                    }
                    continue;
                }
            };
            let (reply, answer) = mpsc::channel();
            if jobs.send(Job { request, reply }).is_err() {
                break;
            }
            let Ok(response) = answer.recv() else {
                break;
            };
            if writeln!(writer, "{response}")
                .and_then(|()| writer.flush())
                .is_err()
            {
                break;
            }
        }
    }

    pub fn serve(path: &Path, prelude: &str) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("create {}: {error}", parent.display()))?;
        }
        // A socket left by a dead daemon is not a session: bind over it.
        let _ = std::fs::remove_file(path);
        let listener = UnixListener::bind(path)
            .map_err(|error| format!("bind {}: {error}", path.display()))?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("chmod {}: {error}", path.display()))?;

        let (jobs, work) = mpsc::channel::<Job>();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else {
                    continue;
                };
                let jobs = jobs.clone();
                thread::spawn(move || client(stream, jobs));
            }
        });

        // The image map stays on this thread. Requests from every client are
        // serialized here, which is what makes one image per session safe while
        // several agents are attached.
        let mut images: HashMap<String, Image> = HashMap::new();
        for job in work {
            let response = dispatch(&mut images, prelude, &job.request);
            let _ = job.reply.send(response);
        }
        Ok(())
    }
}

fn usage() {
    eprintln!("usage: soil-image [--socket PATH]");
}

fn main() {
    let prelude = match prelude() {
        Ok(prelude) => prelude,
        Err(error) => {
            eprintln!("soil-image: {error}");
            std::process::exit(1);
        }
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        usage();
        return;
    }
    if let Some(index) = args.iter().position(|arg| arg == "--socket") {
        let Some(path) = args.get(index + 1) else {
            usage();
            std::process::exit(2);
        };
        #[cfg(unix)]
        {
            if let Err(error) = socket::serve(std::path::Path::new(path), &prelude) {
                eprintln!("soil-image: {error}");
                std::process::exit(1);
            }
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            eprintln!("soil-image: --socket is only supported on Unix");
            std::process::exit(1);
        }
        return;
    }
    if !args.is_empty() {
        usage();
        std::process::exit(2);
    }
    stdin_loop(&prelude);
}

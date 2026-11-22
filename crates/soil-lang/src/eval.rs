// SPDX-License-Identifier: MIT OR Apache-2.0
//! A small closed evaluator for Soil Lisp S-expressions.
//!
//! This is deliberately not a general Lisp. It evaluates the forms Soil Lisp emits:
//! `defun`, `let`, `if`, `cond`, `and`/`or`/`not`, a few list/field helpers, and
//! exactly one host boundary: `(host-call interface function args)`.
//!
//! The generated WIT bindings define `defun` stubs over `host-call`; the host
//! owns grants and dispatch.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::time::Instant;

use crate::sexp::{Comment, Form, Token, Value, parse_all, parse_document, tokenize};
use anyhow::{Context, Result, anyhow, bail};

#[derive(Clone)]
struct Function {
    name: String,
    params: Vec<String>,
    body: Vec<Value>,
}

struct Closure {
    params: Vec<String>,
    body: Vec<Value>,
    env: HashMap<String, Value>,
}

struct CallFrame {
    name: String,
    params: Vec<String>,
    body: Vec<Value>,
    env: HashMap<String, Value>,
}

#[derive(Clone)]
struct Origin {
    path: String,
    start: usize,
    end: usize,
    source_hash: String,
    form: Value,
}

pub struct FlushFile {
    pub path: String,
    pub source: String,
    pub definitions: Vec<String>,
}

pub struct FlushReport {
    pub files: Vec<FlushFile>,
    pub conflicts: Vec<String>,
}

enum Tail {
    Value(Value),
    Call(Value, Vec<Value>),
}

pub struct Runtime {
    functions: HashMap<String, Function>,
    macros: HashMap<String, Function>,
    origins: HashMap<String, Origin>,
    forms: HashMap<String, Value>,
    closures: HashMap<u64, Closure>,
    next_closure: u64,
    gensym: u64,
    host: Box<dyn Host>,
    args: Vec<String>,
    started: Instant,
    evals: u64,
    host_calls: u64,
    host_ms: u64,
}

pub trait Host {
    fn granted(&self, interface: &str, function: &str) -> bool;
    fn call(
        &mut self,
        interface: &str,
        function: &str,
        args: &[Value],
    ) -> std::result::Result<Value, String>;

    fn read_source(&mut self, _path: &str) -> std::result::Result<String, String> {
        Err("source reads are not supported by this host".into())
    }

    fn write_source(&mut self, _path: &str, _source: &str) -> std::result::Result<(), String> {
        Err("source writes are not supported by this host".into())
    }
}

/// Dispatch a `host-call` to the first host that grants it.
pub struct HostChain {
    hosts: Vec<Box<dyn Host>>,
}

impl HostChain {
    pub fn new(hosts: Vec<Box<dyn Host>>) -> Self {
        Self { hosts }
    }
}

impl Host for HostChain {
    fn read_source(&mut self, path: &str) -> std::result::Result<String, String> {
        for host in &mut self.hosts {
            match host.read_source(path) {
                Ok(source) => return Ok(source),
                Err(error) if error == "source reads are not supported by this host" => continue,
                Err(error) => return Err(error),
            }
        }
        Err("no host can read image source".into())
    }

    fn write_source(&mut self, path: &str, source: &str) -> std::result::Result<(), String> {
        for host in &mut self.hosts {
            match host.write_source(path, source) {
                Ok(()) => return Ok(()),
                Err(error) if error == "source writes are not supported by this host" => continue,
                Err(error) => return Err(error),
            }
        }
        Err("no host can write image source".into())
    }

    fn granted(&self, interface: &str, function: &str) -> bool {
        self.hosts
            .iter()
            .any(|host| host.granted(interface, function))
    }

    fn call(
        &mut self,
        interface: &str,
        function: &str,
        args: &[Value],
    ) -> std::result::Result<Value, String> {
        for host in &mut self.hosts {
            if host.granted(interface, function) {
                return host.call(interface, function, args);
            }
        }
        Err(format!("no host grants {interface}/{function}"))
    }
}

impl Runtime {
    pub fn new(host: Box<dyn Host>) -> Self {
        Self {
            functions: HashMap::new(),
            macros: HashMap::new(),
            origins: HashMap::new(),
            forms: HashMap::new(),
            closures: HashMap::new(),
            next_closure: 1,
            gensym: 0,
            host,
            args: Vec::new(),
            started: Instant::now(),
            evals: 0,
            host_calls: 0,
            host_ms: 0,
        }
    }

    /// Arguments the host handed this program, excluding the interpreter's own
    /// flags and source files. The `argv` form exposes them to Soil.
    pub fn set_args(&mut self, args: Vec<String>) {
        self.args = args;
    }

    pub fn has_function(&self, name: &str) -> bool {
        self.functions.contains_key(name)
    }

    pub fn load_file(&mut self, path: impl AsRef<Path>) -> Result<Value> {
        let path = path.as_ref();
        let source =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        self.load_source(&source, None)
            .with_context(|| format!("evaluate {}", path.display()))
    }

    pub fn load(&mut self, source: &str) -> Result<Value> {
        self.load_source(source, None)
    }

    /// Load a VFS-relative source path through the host source seam.
    pub fn load_vfs(&mut self, path: &str) -> Result<Value> {
        let source = self
            .host
            .read_source(path)
            .map_err(|error| anyhow!("read {path}: {error}"))?;
        self.load_source(&source, Some(path))
    }

    /// Content hash of a VFS source path as the image currently sees it.
    pub fn source_hash(&mut self, path: &str) -> Result<String> {
        let source = self
            .host
            .read_source(path)
            .map_err(|error| anyhow!("read {path}: {error}"))?;
        Ok(blake3::hash(source.as_bytes()).to_hex().to_string())
    }

    pub fn load_source(&mut self, source: &str, origin: Option<&str>) -> Result<Value> {
        let document = parse_document(source)?;
        let source_hash = blake3::hash(source.as_bytes()).to_hex().to_string();
        let mut last = Value::Nil;

        for form in document.forms {
            if let Some(list) = form.value.as_list() {
                let head = list.first().and_then(Value::as_symbol);
                if head == Some("defun") || head == Some("defmacro") {
                    let (name, function) = parse_defun(list)?;
                    if head == Some("defun") {
                        self.functions.insert(name.clone(), function);
                    } else {
                        self.macros.insert(name.clone(), function);
                    }
                    self.forms.insert(name.clone(), form.value.clone());
                    if let Some(path) = origin {
                        self.origins.insert(
                            name,
                            Origin {
                                path: path.to_string(),
                                start: form.span.start,
                                end: form.span.end,
                                source_hash: source_hash.clone(),
                                form: form.value.clone(),
                            },
                        );
                    }
                    continue;
                }
                if head.is_some_and(|name| name.starts_with("define-")) {
                    continue;
                }
            }

            let mut env = HashMap::new();
            last = self.eval(&form.value, &mut env)?;
        }

        Ok(last)
    }

    /// Forget source provenance without forgetting the definitions.
    ///
    /// Use this after loading the prelude/baseline: those functions are part of
    /// the image, not dirty source edits waiting to be flushed.
    pub fn clear_provenance(&mut self) {
        self.origins.clear();
        self.forms.clear();
    }

    /// Names whose live definition differs from the form loaded from VFS.
    pub fn dirty_definitions(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .origins
            .iter()
            .filter(|(name, origin)| {
                self.forms
                    .get(*name)
                    .is_some_and(|form| form != &origin.form)
            })
            .map(|(name, _)| name.clone())
            .collect();
        names.sort();
        names
    }

    /// Definitions created in the image with no VFS origin.
    pub fn unattached_definitions(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .forms
            .keys()
            .filter(|name| !self.origins.contains_key(*name))
            .cloned()
            .collect();
        names.sort();
        names
    }

    /// Build candidate source edits without writing. Conflicts are paths whose
    /// current VFS content no longer matches the content the image loaded.
    pub fn flush_preview(&mut self) -> Result<FlushReport> {
        let mut by_path: BTreeMap<String, Vec<(String, Origin, Value)>> = BTreeMap::new();
        for (name, origin) in &self.origins {
            let Some(current) = self.forms.get(name) else {
                continue;
            };
            if current == &origin.form {
                continue;
            }
            by_path.entry(origin.path.clone()).or_default().push((
                name.clone(),
                origin.clone(),
                current.clone(),
            ));
        }

        let mut files = Vec::new();
        let mut conflicts = Vec::new();
        for (path, mut edits) in by_path {
            let current = match self.host.read_source(&path) {
                Ok(source) => source,
                Err(error) => {
                    conflicts.push(format!("{path}: {error}"));
                    continue;
                }
            };
            let current_hash = blake3::hash(current.as_bytes()).to_hex().to_string();
            if edits
                .iter()
                .any(|(_, origin, _)| origin.source_hash != current_hash)
            {
                conflicts.push(format!("{path}: source changed since image load"));
                continue;
            }
            edits.sort_by_key(|(_, origin, _)| origin.start);
            let mut source = current;
            for (_, origin, form) in edits.iter().rev() {
                if origin.end > source.len() {
                    conflicts.push(format!("{path}: origin span is outside the source"));
                    break;
                }
                source.replace_range(origin.start..origin.end, &form.to_string());
            }
            files.push(FlushFile {
                path,
                source,
                definitions: edits.into_iter().map(|(name, _, _)| name).collect(),
            });
        }
        Ok(FlushReport { files, conflicts })
    }

    /// Write dirty definitions back through the host source seam.
    pub fn flush_apply(&mut self) -> Result<FlushReport> {
        let report = self.flush_preview()?;
        if !report.conflicts.is_empty() {
            return Ok(report);
        }
        for file in &report.files {
            self.host
                .write_source(&file.path, &file.source)
                .map_err(|error| anyhow!("write {}: {error}", file.path))?;
        }
        for file in &report.files {
            self.load_vfs(&file.path)?;
        }
        Ok(report)
    }

    /// Append unattached REPL definitions to a VFS source path.
    pub fn flush_unattached(&mut self, path: &str, names: &[String]) -> Result<FlushFile> {
        let mut source = self.host.read_source(path).unwrap_or_default();
        let mut definitions = Vec::new();
        for name in names {
            if let Some(form) = self.forms.get(name) {
                if !source.ends_with('\n') && !source.is_empty() {
                    source.push('\n');
                }
                source.push_str(&form.to_string());
                source.push('\n');
                definitions.push(name.clone());
            }
        }
        self.host
            .write_source(path, &source)
            .map_err(|error| anyhow!("write {path}: {error}"))?;
        self.load_vfs(path)?;
        Ok(FlushFile {
            path: path.to_string(),
            source,
            definitions,
        })
    }

    pub fn call(&mut self, name: &str, args: &[Value]) -> Result<Value> {
        let function = self
            .functions
            .get(name)
            .cloned()
            .ok_or_else(|| anyhow!("unknown function `{name}`"))?;
        self.call_function(&function, args.to_vec())
    }

    fn call_function(&mut self, function: &Function, args: Vec<Value>) -> Result<Value> {
        let frame = CallFrame {
            name: function.name.clone(),
            params: function.params.clone(),
            body: function.body.clone(),
            env: HashMap::new(),
        };
        self.run_call(frame, args)
    }

    fn resolve_frame(&mut self, function: &Value) -> Result<CallFrame> {
        if let Some(id) = function_id(function) {
            let closure = self
                .closures
                .get(&id)
                .ok_or_else(|| anyhow!("unknown function object {id}"))?;
            return Ok(CallFrame {
                name: "lambda".to_string(),
                params: closure.params.clone(),
                body: closure.body.clone(),
                env: closure.env.clone(),
            });
        }
        let name = value_text(function);
        let function = self
            .functions
            .get(&name)
            .cloned()
            .ok_or_else(|| anyhow!("unknown function `{name}`"))?;
        Ok(CallFrame {
            name: function.name,
            params: function.params,
            body: function.body,
            env: HashMap::new(),
        })
    }

    /// Evaluate a call in a loop. A tail call returns a new frame and arguments
    /// instead of growing the Rust stack.
    fn run_call(&mut self, mut frame: CallFrame, mut args: Vec<Value>) -> Result<Value> {
        loop {
            if args.len() != frame.params.len() {
                bail!(
                    "{} expects {} arguments, got {}",
                    frame.name,
                    frame.params.len(),
                    args.len()
                );
            }
            let mut env = frame.env.clone();
            for (name, value) in frame.params.iter().zip(args) {
                env.insert(name.clone(), value);
            }
            let mut body = frame.body.clone();
            let last = body.pop();
            for form in &body {
                self.eval(form, &mut env)?;
            }
            let Some(last) = last else {
                return Ok(Value::Nil);
            };
            match self.eval_tail(&last, &mut env)? {
                Tail::Value(value) => return Ok(value),
                Tail::Call(function, arguments) => {
                    frame = self.resolve_frame(&function)?;
                    args = arguments;
                }
            }
        }
    }

    fn eval_tail(&mut self, expression: &Value, env: &mut HashMap<String, Value>) -> Result<Tail> {
        self.evals += 1;
        let Value::List(items) = expression else {
            return Ok(Tail::Value(self.eval(expression, env)?));
        };
        let Some(head) = items.first().and_then(Value::as_symbol) else {
            return Ok(Tail::Value(self.eval(expression, env)?));
        };
        if self.macros.contains_key(head) {
            let expanded = self.expand_macro(head, &items[1..])?;
            return self.eval_tail(&expanded, env);
        }
        match head {
            "if" => {
                let test = self.eval(require(items, 1, "if")?, env)?;
                let branch = if truthy(&test) { 2 } else { 3 };
                match items.get(branch) {
                    Some(value) => self.eval_tail(value, env),
                    None => Ok(Tail::Value(Value::Nil)),
                }
            }
            "cond" => self.eval_tail_cond(&items[1..], env),
            "let" => self.eval_tail_let(&items[1..], env),
            "begin" | "do" => self.eval_tail_body(&items[1..], env),
            "and" => {
                if items.len() <= 1 {
                    return Ok(Tail::Value(Value::Bool(true)));
                }
                for item in &items[1..items.len() - 1] {
                    let value = self.eval(item, env)?;
                    if !truthy(&value) {
                        return Ok(Tail::Value(value));
                    }
                }
                self.eval_tail(items.last().expect("and has a last form"), env)
            }
            "or" => {
                if items.len() <= 1 {
                    return Ok(Tail::Value(Value::Bool(false)));
                }
                for item in &items[1..items.len() - 1] {
                    let value = self.eval(item, env)?;
                    if truthy(&value) {
                        return Ok(Tail::Value(value));
                    }
                }
                self.eval_tail(items.last().expect("or has a last form"), env)
            }
            "funcall" => {
                let function = self.eval(require(items, 1, "funcall")?, env)?;
                let arguments = self.eval_args(&items[2..], env)?;
                Ok(Tail::Call(function, arguments))
            }
            "apply" => {
                let function = self.eval(require(items, 1, "apply")?, env)?;
                let arguments = self.eval(require(items, 2, "apply")?, env)?;
                let arguments = match arguments {
                    Value::Nil => Vec::new(),
                    Value::List(values) => values,
                    _ => bail!("apply expects a list of arguments"),
                };
                Ok(Tail::Call(function, arguments))
            }
            _ if self.functions.contains_key(head) => {
                let arguments = self.eval_args(&items[1..], env)?;
                Ok(Tail::Call(Value::Symbol(head.to_string()), arguments))
            }
            _ => Ok(Tail::Value(self.eval(expression, env)?)),
        }
    }

    fn eval_tail_body(&mut self, body: &[Value], env: &mut HashMap<String, Value>) -> Result<Tail> {
        if body.is_empty() {
            return Ok(Tail::Value(Value::Nil));
        }
        for form in &body[..body.len() - 1] {
            self.eval(form, env)?;
        }
        self.eval_tail(&body[body.len() - 1], env)
    }

    fn eval_tail_cond(
        &mut self,
        clauses: &[Value],
        env: &mut HashMap<String, Value>,
    ) -> Result<Tail> {
        for clause in clauses {
            let list = clause
                .as_list()
                .ok_or_else(|| anyhow!("cond clause is not a list"))?;
            let test = list
                .first()
                .ok_or_else(|| anyhow!("cond clause has no test"))?;
            let matched = match test {
                Value::Symbol(name) if name == "else" => true,
                _ => truthy(&self.eval(test, env)?),
            };
            if matched {
                return self.eval_tail_body(&list[1..], env);
            }
        }
        Ok(Tail::Value(Value::Nil))
    }

    fn eval_tail_let(&mut self, args: &[Value], env: &mut HashMap<String, Value>) -> Result<Tail> {
        let bindings = args
            .first()
            .and_then(Value::as_list)
            .ok_or_else(|| anyhow!("let needs a binding list"))?;
        let mut next = env.clone();
        for binding in bindings {
            let pair = binding
                .as_list()
                .ok_or_else(|| anyhow!("let binding is not a list"))?;
            let name = pair
                .first()
                .and_then(Value::as_symbol)
                .ok_or_else(|| anyhow!("let binding needs a name"))?;
            let value = self.eval(
                pair.get(1)
                    .ok_or_else(|| anyhow!("let binding needs a value"))?,
                &mut next,
            )?;
            next.insert(name.to_string(), value);
        }
        self.eval_tail_body(&args[1..], &mut next)
    }

    fn call_macro(&mut self, function: &Function, args: Vec<Value>) -> Result<Value> {
        if args.len() != function.params.len() {
            bail!(
                "macro {} expects {} arguments, got {}",
                function.name,
                function.params.len(),
                args.len()
            );
        }
        let mut env = HashMap::new();
        for (name, value) in function.params.iter().zip(args) {
            env.insert(name.clone(), value);
        }
        self.eval_body(&function.body, &mut env)
            .with_context(|| format!("in macro `{}`", function.name))
    }

    fn expand_macro(&mut self, name: &str, args: &[Value]) -> Result<Value> {
        let function = self
            .macros
            .get(name)
            .cloned()
            .ok_or_else(|| anyhow!("unknown macro `{name}`"))?;
        self.call_macro(&function, args.to_vec())
    }

    fn macroexpand(&mut self, mut form: Value) -> Result<Value> {
        loop {
            let expanded = match &form {
                Value::List(items) => {
                    let Some(head) = items.first().and_then(Value::as_symbol) else {
                        return Ok(form);
                    };
                    if !self.macros.contains_key(head) {
                        return Ok(form);
                    }
                    self.expand_macro(head, &items[1..])?
                }
                _ => return Ok(form),
            };
            form = expanded;
        }
    }

    fn macroexpand_one(&mut self, form: Value) -> Result<Value> {
        match &form {
            Value::List(items) => {
                let Some(head) = items.first().and_then(Value::as_symbol) else {
                    return Ok(form);
                };
                if !self.macros.contains_key(head) {
                    return Ok(form);
                }
                self.expand_macro(head, &items[1..])
            }
            _ => Ok(form),
        }
    }

    fn call_value(&mut self, function: &Value, args: Vec<Value>) -> Result<Value> {
        let frame = self.resolve_frame(function)?;
        self.run_call(frame, args)
    }

    fn eval_quasiquote(
        &mut self,
        form: &Value,
        depth: usize,
        env: &mut HashMap<String, Value>,
    ) -> Result<Value> {
        let Some(items) = form.as_list() else {
            return Ok(form.clone());
        };
        if items.is_empty() {
            return Ok(Value::Nil);
        }
        let head = items.first().and_then(Value::as_symbol);
        if head == Some("unquote") {
            if depth == 1 {
                return self.eval(require(items, 1, "unquote")?, env);
            }
            return Ok(Value::List(vec![
                Value::Symbol("unquote".to_string()),
                self.eval_quasiquote(require(items, 1, "unquote")?, depth - 1, env)?,
            ]));
        }
        if head == Some("unquote-splicing") {
            if depth == 1 {
                return self.eval(require(items, 1, "unquote-splicing")?, env);
            }
            return Ok(Value::List(vec![
                Value::Symbol("unquote-splicing".to_string()),
                self.eval_quasiquote(require(items, 1, "unquote-splicing")?, depth - 1, env)?,
            ]));
        }
        if head == Some("quasiquote") {
            return Ok(Value::List(vec![
                Value::Symbol("quasiquote".to_string()),
                self.eval_quasiquote(require(items, 1, "quasiquote")?, depth + 1, env)?,
            ]));
        }

        let mut out = Vec::new();
        for item in items {
            let item_list = item.as_list();
            if depth == 1
                && item_list
                    .and_then(|parts| parts.first())
                    .and_then(Value::as_symbol)
                    == Some("unquote-splicing")
            {
                let value = self.eval(require(item_list.unwrap(), 1, "unquote-splicing")?, env)?;
                match value {
                    Value::Nil => {}
                    Value::List(values) => out.extend(values),
                    _ => bail!("unquote-splicing expects a list"),
                }
            } else {
                out.push(self.eval_quasiquote(item, depth, env)?);
            }
        }
        Ok(Value::List(out))
    }

    fn eval_body(&mut self, body: &[Value], env: &mut HashMap<String, Value>) -> Result<Value> {
        let mut last = Value::Nil;
        for expression in body {
            last = self.eval(expression, env)?;
        }
        Ok(last)
    }

    fn eval(&mut self, expression: &Value, env: &mut HashMap<String, Value>) -> Result<Value> {
        self.evals += 1;
        match expression {
            Value::Nil
            | Value::Bool(_)
            | Value::Int(_)
            | Value::Float(_)
            | Value::String(_)
            | Value::Keyword(_) => Ok(expression.clone()),
            Value::Symbol(name) => env
                .get(name)
                .cloned()
                .ok_or_else(|| anyhow!("unbound symbol `{name}`")),
            Value::List(items) => {
                let head = items
                    .first()
                    .and_then(Value::as_symbol)
                    .ok_or_else(|| anyhow!("empty or invalid call `{expression}`"))?;

                if self.macros.contains_key(head) {
                    let expanded = self.expand_macro(head, &items[1..])?;
                    return self.eval(&expanded, env);
                }

                match head {
                    "quote" => Ok(items
                        .get(1)
                        .cloned()
                        .ok_or_else(|| anyhow!("quote needs one argument"))?),
                    "quasiquote" => {
                        let form = require(items, 1, "quasiquote")?;
                        self.eval_quasiquote(form, 1, env)
                    }
                    "unquote" | "unquote-splicing" => {
                        bail!("{head} outside quasiquote")
                    }
                    "defmacro" => {
                        let (name, function) = parse_defun(items)?;
                        self.macros.insert(name, function);
                        Ok(Value::Nil)
                    }
                    "lambda" => {
                        let params = items
                            .get(1)
                            .and_then(Value::as_list)
                            .ok_or_else(|| anyhow!("lambda needs a parameter list"))?
                            .iter()
                            .map(|value| {
                                value
                                    .as_symbol()
                                    .map(str::to_string)
                                    .ok_or_else(|| anyhow!("lambda parameter is not a symbol"))
                            })
                            .collect::<Result<Vec<_>>>()?;
                        let body = items[2..].to_vec();
                        if body.is_empty() {
                            bail!("lambda needs a body");
                        }
                        let id = self.next_closure;
                        self.next_closure += 1;
                        self.closures.insert(
                            id,
                            Closure {
                                params,
                                body,
                                env: env.clone(),
                            },
                        );
                        Ok(function_object(id))
                    }
                    "function" => {
                        let target = require(items, 1, "function")?;
                        match target {
                            Value::Symbol(name) => Ok(Value::Symbol(name.clone())),
                            Value::List(parts)
                                if parts.first().and_then(Value::as_symbol) == Some("lambda") =>
                            {
                                self.eval(target, env)
                            }
                            _ => bail!("function expects a symbol or lambda"),
                        }
                    }
                    "funcall" => {
                        let function = self.eval(require(items, 1, "funcall")?, env)?;
                        let args = self.eval_args(&items[2..], env)?;
                        self.call_value(&function, args)
                    }
                    "macroexpand" | "macroexpand-1" => {
                        let form = self.eval(require(items, 1, head)?, env)?;
                        if head == "macroexpand-1" {
                            self.macroexpand_one(form)
                        } else {
                            self.macroexpand(form)
                        }
                    }
                    "gensym" => {
                        let prefix = items
                            .get(1)
                            .map(value_text)
                            .unwrap_or_else(|| "g".to_string());
                        self.gensym += 1;
                        Ok(Value::Symbol(format!("{prefix}{}", self.gensym)))
                    }
                    "if" => {
                        let test = self.eval(require(items, 1, "if")?, env)?;
                        let branch = if truthy(&test) { 2 } else { 3 };
                        match items.get(branch) {
                            Some(value) => self.eval(value, env),
                            None => Ok(Value::Nil),
                        }
                    }
                    "cond" => self.eval_cond(&items[1..], env),
                    "let" => self.eval_let(&items[1..], env),
                    "and" => {
                        let mut last = Value::Bool(true);
                        for item in &items[1..] {
                            last = self.eval(item, env)?;
                            if !truthy(&last) {
                                return Ok(last);
                            }
                        }
                        Ok(last)
                    }
                    "or" => {
                        for item in &items[1..] {
                            let value = self.eval(item, env)?;
                            if truthy(&value) {
                                return Ok(value);
                            }
                        }
                        Ok(Value::Bool(false))
                    }
                    "not" => Ok(Value::Bool(!truthy(
                        &self.eval(require(items, 1, "not")?, env)?,
                    ))),
                    "begin" | "do" => self.eval_body(&items[1..], env),
                    "read" => {
                        let source = self.eval(require(items, 1, "read")?, env)?;
                        let source = source
                            .as_str()
                            .ok_or_else(|| anyhow!("read expects a string"))?;
                        Ok(Value::List(parse_all(source)?))
                    }
                    "read-document" => {
                        let source = self.eval(require(items, 1, "read-document")?, env)?;
                        let source = source
                            .as_str()
                            .ok_or_else(|| anyhow!("read-document expects a string"))?;
                        let document = parse_document(source)?;
                        let tokens = tokenize(source)?;
                        document_value(document, tokens)
                            .map_err(|error| anyhow!("read-document: {error}"))
                    }
                    "eval" => {
                        let form = self.eval(require(items, 1, "eval")?, env)?;
                        self.eval(&form, env)
                    }
                    "host-call" => self.eval_host_call(items, env),
                    "keyword" => {
                        let value = self.eval(require(items, 1, "keyword")?, env)?;
                        let text = value
                            .as_str()
                            .ok_or_else(|| anyhow!("keyword expects a string or symbol"))?;
                        Ok(Value::Keyword(text.to_string()))
                    }
                    "json-object" => Ok(json_object()),
                    "runtime-stats" => Ok(self.stats_value()),
                    "runtime-reset" => {
                        self.reset_stats();
                        Ok(Value::Nil)
                    }
                    "list" => Ok(Value::List(self.eval_args(&items[1..], env)?)),
                    "argv" => Ok(Value::List(
                        self.args.iter().cloned().map(Value::String).collect(),
                    )),
                    "field" | "get" => {
                        let record = self.eval(require(items, 1, "field")?, env)?;
                        let name = self.eval(require(items, 2, "field")?, env)?;
                        Ok(field_value(&record, &name))
                    }
                    "length" => {
                        let value = self.eval(require(items, 1, "length")?, env)?;
                        Ok(match &value {
                            Value::List(values) => Value::Int(values.len() as i64),
                            Value::String(value) => Value::Int(value.chars().count() as i64),
                            _ => bail!("length expects a list or string"),
                        })
                    }
                    "str" => Ok(Value::String(
                        self.eval_args(&items[1..], env)?
                            .iter()
                            .map(display_text)
                            .collect::<Vec<_>>()
                            .join(""),
                    )),
                    "exit" => {
                        let code = self.eval(require(items, 1, "exit")?, env)?;
                        let code = code
                            .as_f64()
                            .ok_or_else(|| anyhow!("exit expects an integer"))?
                            as i32;
                        std::process::exit(code);
                    }
                    "eprintln" => {
                        let values = self.eval_args(&items[1..], env)?;
                        eprintln!(
                            "{}",
                            values
                                .iter()
                                .map(display_text)
                                .collect::<Vec<_>>()
                                .join(" ")
                        );
                        Ok(Value::Nil)
                    }
                    "println" => {
                        let values = self.eval_args(&items[1..], env)?;
                        println!(
                            "{}",
                            values
                                .iter()
                                .map(display_text)
                                .collect::<Vec<_>>()
                                .join(" ")
                        );
                        Ok(Value::Nil)
                    }
                    "ok?" => Ok(Value::Bool(is_tag(
                        &self.eval(require(items, 1, "ok?")?, env)?,
                        "ok",
                    ))),
                    "err?" => Ok(Value::Bool(is_tag(
                        &self.eval(require(items, 1, "err?")?, env)?,
                        "err",
                    ))),
                    "value" => {
                        let value = self.eval(require(items, 1, "value")?, env)?;
                        Ok(match value.as_list() {
                            Some([_, value]) => value.clone(),
                            _ => bail!("value expects a two-element tagged list"),
                        })
                    }
                    "nil?" => Ok(Value::Bool(
                        match self.eval(require(items, 1, "nil?")?, env)? {
                            Value::Nil => true,
                            Value::List(items) if items.is_empty() => true,
                            _ => false,
                        },
                    )),
                    "first" => {
                        let value = self.eval(require(items, 1, "first")?, env)?;
                        Ok(value
                            .as_list()
                            .and_then(|values| values.first())
                            .cloned()
                            .unwrap_or(Value::Nil))
                    }
                    "rest" => {
                        let value = self.eval(require(items, 1, "rest")?, env)?;
                        Ok(match value.as_list() {
                            Some([_, values @ ..]) if !values.is_empty() => {
                                Value::List(values.to_vec())
                            }
                            _ => Value::Nil,
                        })
                    }
                    "cons" => {
                        let head = self.eval(require(items, 1, "cons")?, env)?;
                        let tail = self.eval(require(items, 2, "cons")?, env)?;
                        match tail {
                            Value::Nil => Ok(Value::List(vec![head])),
                            Value::List(mut values) => {
                                values.insert(0, head);
                                Ok(Value::List(values))
                            }
                            _ => bail!("cons expects a list or nil as its second argument"),
                        }
                    }
                    "append" => {
                        let left = self.eval(require(items, 1, "append")?, env)?;
                        let right = self.eval(require(items, 2, "append")?, env)?;
                        let mut values = match left {
                            Value::Nil => Vec::new(),
                            Value::List(values) => values,
                            _ => bail!("append expects lists"),
                        };
                        match right {
                            Value::Nil => {}
                            Value::List(items) => values.extend(items),
                            _ => bail!("append expects lists"),
                        }
                        Ok(Value::List(values))
                    }
                    "apply" => {
                        let function = self.eval(require(items, 1, "apply")?, env)?;
                        let arguments = self.eval(require(items, 2, "apply")?, env)?;
                        let arguments = match arguments {
                            Value::Nil => Vec::new(),
                            Value::List(values) => values,
                            _ => bail!("apply expects a list of arguments"),
                        };
                        self.call_value(&function, arguments)
                    }
                    "symbol?" => Ok(Value::Bool(matches!(
                        self.eval(require(items, 1, "symbol?")?, env)?,
                        Value::Symbol(_)
                    ))),
                    "atom" => {
                        let value = self.eval(require(items, 1, "atom")?, env)?;
                        Ok(if matches!(value, Value::List(_)) {
                            Value::Nil
                        } else {
                            Value::Symbol("t".to_string())
                        })
                    }
                    "eq" => {
                        let left = self.eval(require(items, 1, "eq")?, env)?;
                        let right = self.eval(require(items, 2, "eq")?, env)?;
                        Ok(if left == right {
                            Value::Symbol("t".to_string())
                        } else {
                            Value::Nil
                        })
                    }
                    "car" => {
                        let value = self.eval(require(items, 1, "car")?, env)?;
                        Ok(match value.as_list() {
                            Some([first, ..]) => first.clone(),
                            _ => Value::Nil,
                        })
                    }
                    "cdr" => {
                        let value = self.eval(require(items, 1, "cdr")?, env)?;
                        Ok(match value.as_list() {
                            Some([_, rest @ ..]) if !rest.is_empty() => Value::List(rest.to_vec()),
                            _ => Value::Nil,
                        })
                    }
                    "string-contains?" => {
                        let haystack = self.eval(require(items, 1, "string-contains?")?, env)?;
                        let needle = self.eval(require(items, 2, "string-contains?")?, env)?;
                        let Some(haystack) = haystack.as_str() else {
                            bail!("string-contains? expects a string");
                        };
                        let Some(needle) = needle.as_str() else {
                            bail!("string-contains? expects a string");
                        };
                        Ok(Value::Bool(haystack.contains(needle)))
                    }
                    "string-starts-with?" => {
                        let text = self.eval(require(items, 1, "string-starts-with?")?, env)?;
                        let prefix =
                            self.eval(require(items, 2, "string-starts-with?")?, env)?;
                        let Some(text) = text.as_str() else {
                            bail!("string-starts-with? expects a string");
                        };
                        let Some(prefix) = prefix.as_str() else {
                            bail!("string-starts-with? expects a prefix string");
                        };
                        Ok(Value::Bool(text.starts_with(prefix)))
                    }
                    "string-replace" => {
                        let text = self.eval(require(items, 1, "string-replace")?, env)?;
                        let from = self.eval(require(items, 2, "string-replace")?, env)?;
                        let to = self.eval(require(items, 3, "string-replace")?, env)?;
                        let Some(text) = text.as_str() else {
                            bail!("string-replace expects a string");
                        };
                        let Some(from) = from.as_str() else {
                            bail!("string-replace expects a search string");
                        };
                        let Some(to) = to.as_str() else {
                            bail!("string-replace expects a replacement string");
                        };
                        Ok(Value::String(text.replace(from, to)))
                    }
                    "bytes->string" => {
                        let value = self.eval(require(items, 1, "bytes->string")?, env)?;
                        let Value::List(bytes) = value else {
                            bail!("bytes->string expects a list of bytes");
                        };
                        let bytes = bytes
                            .iter()
                            .map(|byte| match byte {
                                Value::Int(byte) if (0..=255).contains(byte) => Ok(*byte as u8),
                                _ => bail!("bytes->string expects bytes in 0..=255"),
                            })
                            .collect::<Result<Vec<_>>>()?;
                        Ok(Value::String(String::from_utf8_lossy(&bytes).into_owned()))
                    }
                    "string->bytes" => {
                        let value = self.eval(require(items, 1, "string->bytes")?, env)?;
                        let text = value
                            .as_str()
                            .ok_or_else(|| anyhow!("string->bytes expects a string"))?;
                        Ok(Value::List(
                            text.bytes().map(|byte| Value::Int(byte as i64)).collect(),
                        ))
                    }
                    "string-from-bytes" => {
                        let value = self.eval(require(items, 1, "string-from-bytes")?, env)?;
                        let Value::List(bytes) = value else {
                            bail!("string-from-bytes expects a list of bytes");
                        };
                        let bytes = bytes
                            .iter()
                            .map(|byte| match byte {
                                Value::Int(byte) if (0..=255).contains(byte) => Ok(*byte as u8),
                                _ => bail!("string-from-bytes expects bytes in 0..=255"),
                            })
                            .collect::<Result<Vec<_>>>()?;
                        Ok(Value::String(String::from_utf8_lossy(&bytes).into_owned()))
                    }
                    "getenv" => {
                        let value = self.eval(require(items, 1, "getenv")?, env)?;
                        let name = value
                            .as_str()
                            .ok_or_else(|| anyhow!("getenv expects a string"))?;
                        Ok(match std::env::var(name) {
                            Ok(value) => Value::String(value),
                            Err(_) => Value::Nil,
                        })
                    }
                    "string-chars" => {
                        let text = self.eval(require(items, 1, "string-chars")?, env)?;
                        let Some(text) = text.as_str() else {
                            bail!("string-chars expects a string");
                        };
                        Ok(Value::List(
                            text.chars()
                                .map(|character| Value::String(character.to_string()))
                                .collect(),
                        ))
                    }
                    "string-join" => {
                        let values = self.eval(require(items, 1, "string-join")?, env)?;
                        let separator = self.eval(require(items, 2, "string-join")?, env)?;
                        let Some(separator) = separator.as_str() else {
                            bail!("string-join expects a separator string");
                        };
                        let values = match values {
                            Value::Nil => Vec::new(),
                            Value::List(values) => values,
                            _ => bail!("string-join expects a list"),
                        };
                        Ok(Value::String(
                            values
                                .iter()
                                .map(value_text)
                                .collect::<Vec<_>>()
                                .join(separator),
                        ))
                    }
                    "json-decode" => {
                        let value = self.eval(require(items, 1, "json-decode")?, env)?;
                        let bytes = value_bytes(&value)?;
                        let json: serde_json::Value = serde_json::from_slice(&bytes)
                            .with_context(|| "invalid JSON in json-decode")?;
                        Ok(json_to_value(&json))
                    }
                    "json-encode" => {
                        let value = self.eval(require(items, 1, "json-encode")?, env)?;
                        Ok(Value::String(
                            serde_json::to_string(&value_to_json(&value))
                                .with_context(|| "json-encode failed")?,
                        ))
                    }
                    "+" | "-" | "*" | "/" => self.eval_arithmetic(head, &items[1..], env),
                    "pow" => {
                        let base = self.eval(require(items, 1, "pow")?, env)?;
                        let exponent = self.eval(require(items, 2, "pow")?, env)?;
                        let base = base
                            .as_f64()
                            .ok_or_else(|| anyhow!("pow expects numbers"))?;
                        let exponent = exponent
                            .as_f64()
                            .ok_or_else(|| anyhow!("pow expects numbers"))?;
                        Ok(Value::Float(base.powf(exponent)))
                    }
                    "slugify" => {
                        let value = self.eval(require(items, 1, "slugify")?, env)?;
                        let text = value
                            .as_str()
                            .ok_or_else(|| anyhow!("slugify expects a string"))?;
                        Ok(Value::String(slugify(text)))
                    }
                    "date-string" => {
                        let value = self.eval(require(items, 1, "date-string")?, env)?;
                        let epoch = value
                            .as_f64()
                            .ok_or_else(|| anyhow!("date-string expects epoch seconds"))?;
                        Ok(Value::String(date_string(epoch as i64)))
                    }
                    "int" => {
                        let value = self.eval(require(items, 1, "int")?, env)?;
                        let value = value
                            .as_f64()
                            .ok_or_else(|| anyhow!("int expects a number"))?;
                        Ok(Value::Int(value as i64))
                    }
                    "round" => {
                        let value = self.eval(require(items, 1, "round")?, env)?;
                        let value = value
                            .as_f64()
                            .ok_or_else(|| anyhow!("round expects a number"))?;
                        Ok(Value::Float(value.round()))
                    }
                    "floor" => {
                        let value = self.eval(require(items, 1, "floor")?, env)?;
                        let value = value
                            .as_f64()
                            .ok_or_else(|| anyhow!("floor expects a number"))?;
                        Ok(Value::Float(value.floor()))
                    }
                    "=" | ">" | "<" | ">=" | "<=" => self.eval_comparison(head, &items[1..], env),
                    name if self.functions.contains_key(name) => {
                        let function = self.functions.get(name).cloned().unwrap();
                        let args = self.eval_args(&items[1..], env)?;
                        self.call_function(&function, args)
                    }
                    name => bail!("unknown form or function `{name}`"),
                }
            }
        }
    }

    fn eval_args(
        &mut self,
        expressions: &[Value],
        env: &mut HashMap<String, Value>,
    ) -> Result<Vec<Value>> {
        expressions
            .iter()
            .map(|expression| self.eval(expression, env))
            .collect()
    }

    fn eval_cond(&mut self, clauses: &[Value], env: &mut HashMap<String, Value>) -> Result<Value> {
        for clause in clauses {
            let list = clause
                .as_list()
                .ok_or_else(|| anyhow!("cond clause is not a list"))?;
            let test = list
                .first()
                .ok_or_else(|| anyhow!("cond clause has no test"))?;
            let matched = match test {
                Value::Symbol(name) if name == "else" => true,
                _ => truthy(&self.eval(test, env)?),
            };
            if matched {
                return self.eval_body(&list[1..], env);
            }
        }
        Ok(Value::Nil)
    }

    fn eval_let(&mut self, args: &[Value], env: &mut HashMap<String, Value>) -> Result<Value> {
        let bindings = args
            .first()
            .and_then(Value::as_list)
            .ok_or_else(|| anyhow!("let needs a binding list"))?;
        let mut next = env.clone();

        for binding in bindings {
            let pair = binding
                .as_list()
                .ok_or_else(|| anyhow!("let binding is not a list"))?;
            let name = pair
                .first()
                .and_then(Value::as_symbol)
                .ok_or_else(|| anyhow!("let binding needs a name"))?;
            let value = self.eval(
                pair.get(1)
                    .ok_or_else(|| anyhow!("let binding needs a value"))?,
                &mut next,
            )?;
            next.insert(name.to_string(), value);
        }

        self.eval_body(&args[1..], &mut next)
    }

    fn eval_host_call(
        &mut self,
        items: &[Value],
        env: &mut HashMap<String, Value>,
    ) -> Result<Value> {
        let interface = self.eval(require(items, 1, "host-call")?, env)?;
        let function = self.eval(require(items, 2, "host-call")?, env)?;
        let arguments = match items.get(3) {
            Some(expression) => self.eval(expression, env)?,
            None => Value::Nil,
        };

        let interface = value_text(&interface);
        let function = value_text(&function);
        let args = match arguments {
            Value::Nil => Vec::new(),
            Value::List(values) => values,
            _ => bail!("host-call arguments must be a list or nil"),
        };

        if !self.host.granted(&interface, &function) {
            bail!("denied: {interface}/{function} is not granted to this caller");
        }

        self.host_calls += 1;
        let started = Instant::now();
        let result = self
            .host
            .call(&interface, &function, &args)
            .map_err(|error| anyhow!("host error: {error}"));
        self.host_ms += started.elapsed().as_millis() as u64;
        result
    }

    fn reset_stats(&mut self) {
        self.started = Instant::now();
        self.evals = 0;
        self.host_calls = 0;
        self.host_ms = 0;
    }

    fn stats_value(&self) -> Value {
        Value::List(vec![
            pair("evals", Value::Int(self.evals as i64)),
            pair("host_calls", Value::Int(self.host_calls as i64)),
            pair("host_ms", Value::Int(self.host_ms as i64)),
            pair(
                "elapsed_ms",
                Value::Int(self.started.elapsed().as_millis() as i64),
            ),
        ])
    }

    fn eval_arithmetic(
        &mut self,
        operator: &str,
        expressions: &[Value],
        env: &mut HashMap<String, Value>,
    ) -> Result<Value> {
        let values = self.eval_args(expressions, env)?;
        let numbers = values
            .iter()
            .map(|value| {
                value
                    .as_f64()
                    .ok_or_else(|| anyhow!("{operator} expects numbers"))
            })
            .collect::<Result<Vec<_>>>()?;

        if operator == "/" {
            if numbers.len() < 2 {
                bail!("/ needs at least two arguments");
            }
            let mut result = numbers[0];
            for number in &numbers[1..] {
                if *number == 0.0 {
                    bail!("division by zero");
                }
                result /= number;
            }
            return Ok(Value::Float(result));
        }

        let mut result = numbers.first().copied().unwrap_or(0.0);
        for number in numbers.iter().skip(1) {
            match operator {
                "+" => result += *number,
                "-" => result -= *number,
                "*" => result *= *number,
                _ => unreachable!(),
            }
        }

        let all_ints = values.iter().all(|value| matches!(value, Value::Int(_)));
        if all_ints {
            Ok(Value::Int(result as i64))
        } else {
            Ok(Value::Float(result))
        }
    }

    fn eval_comparison(
        &mut self,
        operator: &str,
        expressions: &[Value],
        env: &mut HashMap<String, Value>,
    ) -> Result<Value> {
        if expressions.len() != 2 {
            bail!("{operator} needs two arguments");
        }
        let left = self.eval(&expressions[0], env)?;
        let right = self.eval(&expressions[1], env)?;

        let result = match (left.as_f64(), right.as_f64()) {
            (Some(left), Some(right)) => match operator {
                "=" => left == right,
                ">" => left > right,
                "<" => left < right,
                ">=" => left >= right,
                "<=" => left <= right,
                _ => unreachable!(),
            },
            _ => match operator {
                "=" => left == right,
                _ => bail!("{operator} expects numbers"),
            },
        };

        Ok(Value::Bool(result))
    }
}

fn value_bytes(value: &Value) -> Result<Vec<u8>> {
    match value {
        Value::List(items) => items
            .iter()
            .map(|item| match item {
                Value::Int(byte) if (0..=255).contains(byte) => Ok(*byte as u8),
                _ => bail!("body must be a list of bytes"),
            })
            .collect(),
        Value::String(text) => Ok(text.as_bytes().to_vec()),
        _ => bail!("body must be a list of bytes or a string"),
    }
}

fn slugify(text: &str) -> String {
    let mut slug = String::new();
    let mut last_dash = true;
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            slug.push('-');
            last_dash = true;
        }
    }
    slug.trim_matches('-').to_string()
}

/// Civil date from Unix seconds, independent of a date crate. Howard Hinnant's
/// days-from-civil inverse keeps `burndown` points stable in native and wasm.
fn date_string(epoch: i64) -> String {
    let days = epoch.div_euclid(86_400);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// An empty JSON object cannot be distinguished from an empty array in the
/// list-only value model. A one-symbol marker keeps `{}` round-tripping through
/// `json-decode` / `json-encode`; it is not a user-visible record.
const JSON_OBJECT_MARKER: &str = "__json_object__";

fn json_object() -> Value {
    Value::List(vec![Value::Symbol(JSON_OBJECT_MARKER.to_string())])
}

fn is_json_object(items: &[Value]) -> bool {
    matches!(items, [Value::Symbol(name)] if name == JSON_OBJECT_MARKER)
}


/// Turn a parsed document into the record shape the linter and doc tools read.
///
/// Forms retain their source byte span and leading comments. The raw comment
/// text is preserved so `;;;` blocks remain Markdown rather than being
/// reflowed by the reader.
fn document_value(document: crate::sexp::Document, tokens: Vec<Token>) -> Result<Value> {
    Ok(Value::List(vec![
        pair("source", Value::String(document.source)),
        pair(
            "forms",
            Value::List(document.forms.into_iter().map(form_value).collect()),
        ),
        pair(
            "trailing-comments",
            Value::List(document.trailing_comments.into_iter().map(comment_value).collect()),
        ),
        pair(
            "tokens",
            Value::List(tokens.into_iter().map(token_value).collect()),
        ),
    ]))
}

fn token_value(token: Token) -> Value {
    Value::List(vec![
        pair("kind", Value::String(token.kind.as_str().into())),
        pair("text", Value::String(token.text)),
        pair("start", Value::Int(token.start as i64)),
        pair("end", Value::Int(token.end as i64)),
    ])
}

fn form_value(form: Form) -> Value {
    Value::List(vec![
        pair("id", Value::String(form.id)),
        pair("start", Value::Int(form.span.start as i64)),
        pair("end", Value::Int(form.span.end as i64)),
        pair("value", form.value),
        pair(
            "comments",
            Value::List(form.comments.into_iter().map(comment_value).collect()),
        ),
    ])
}

fn comment_value(comment: Comment) -> Value {
    Value::List(vec![
        pair("text", Value::String(comment.text)),
        pair("start", Value::Int(comment.span.start as i64)),
        pair("end", Value::Int(comment.span.end as i64)),
    ])
}

/// Decode a JSON value into the record/list shape Soil uses.
///
/// JSON objects become lists of `(keyword value)` pairs, matching
/// `json-decode` and the UI form encoding.
pub fn json_to_value(json: &serde_json::Value) -> Value {
    match json {
        serde_json::Value::Null => Value::Nil,
        serde_json::Value::Bool(value) => Value::Bool(*value),
        serde_json::Value::Number(number) => number
            .as_i64()
            .map(Value::Int)
            .or_else(|| number.as_f64().map(Value::Float))
            .unwrap_or(Value::Nil),
        serde_json::Value::String(text) => Value::String(text.clone()),
        serde_json::Value::Array(items) => Value::List(items.iter().map(json_to_value).collect()),
        serde_json::Value::Object(object) if object.is_empty() => json_object(),
        serde_json::Value::Object(object) => Value::List(
            object
                .iter()
                .map(|(key, value)| pair(key, json_to_value(value)))
                .collect(),
        ),
    }
}

/// Encode a Soil value as JSON using the same shape `json-encode` uses.
///
/// A list of `(keyword value)` pairs is an object; every other list is an
/// array. This is the prototype encoding for host-side UI trees and tools.
pub fn value_to_json(value: &Value) -> serde_json::Value {
    match value {
        Value::Nil => serde_json::Value::Null,
        Value::Bool(value) => serde_json::Value::Bool(*value),
        Value::Int(value) => serde_json::Value::Number((*value).into()),
        Value::Float(value) => serde_json::Number::from_f64(*value)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        Value::String(text) | Value::Symbol(text) | Value::Keyword(text) => {
            serde_json::Value::String(text.clone())
        }
        Value::List(items) if is_json_object(items) => {
            serde_json::Value::Object(Default::default())
        }
        Value::List(items) => {
            let is_object = items.iter().all(|item| {
                item.as_list().is_some_and(|pair| {
                    pair.len() == 2 && pair.first().and_then(Value::as_keyword).is_some()
                })
            });
            if is_object && !items.is_empty() {
                let mut object = serde_json::Map::new();
                for item in items {
                    let pair = item.as_list().unwrap();
                    object.insert(
                        pair[0].as_keyword().unwrap().to_string(),
                        value_to_json(&pair[1]),
                    );
                }
                serde_json::Value::Object(object)
            } else {
                serde_json::Value::Array(items.iter().map(value_to_json).collect())
            }
        }
    }
}

fn require<'a>(items: &'a [Value], index: usize, form: &str) -> Result<&'a Value> {
    items
        .get(index)
        .ok_or_else(|| anyhow!("{form} needs argument {index}"))
}

fn parse_defun(list: &[Value]) -> Result<(String, Function)> {
    let name = list
        .get(1)
        .and_then(Value::as_symbol)
        .ok_or_else(|| anyhow!("defun needs a name"))?
        .to_string();
    let params = list
        .get(2)
        .and_then(Value::as_list)
        .ok_or_else(|| anyhow!("defun needs a parameter list"))?
        .iter()
        .map(|value| {
            value
                .as_symbol()
                .map(str::to_string)
                .ok_or_else(|| anyhow!("defun parameter is not a symbol"))
        })
        .collect::<Result<Vec<_>>>()?;
    let body = list[3..].to_vec();
    if body.is_empty() {
        bail!("defun needs a body");
    }
    Ok((name.clone(), Function { name, params, body }))
}

fn truthy(value: &Value) -> bool {
    !matches!(value, Value::Nil | Value::Bool(false))
}

fn is_tag(value: &Value, tag: &str) -> bool {
    value
        .as_list()
        .and_then(|items| items.first())
        .and_then(Value::as_symbol)
        == Some(tag)
}

fn function_object(id: u64) -> Value {
    Value::List(vec![
        Value::Symbol("#<function>".to_string()),
        Value::Int(id as i64),
    ])
}

fn function_id(value: &Value) -> Option<u64> {
    let items = value.as_list()?;
    if items.first().and_then(Value::as_symbol) != Some("#<function>") {
        return None;
    }
    match items.get(1) {
        Some(Value::Int(id)) => u64::try_from(*id).ok(),
        _ => None,
    }
}

fn value_text(value: &Value) -> String {
    match value {
        Value::String(text) | Value::Symbol(text) | Value::Keyword(text) => text.clone(),
        other => other.to_string(),
    }
}

fn display_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn field_value(record: &Value, name: &Value) -> Value {
    let Some(items) = record.as_list() else {
        return Value::Nil;
    };
    let key = name.as_keyword().or_else(|| name.as_symbol());
    let Some(key) = key else {
        return Value::Nil;
    };
    for item in items {
        let Some(pair) = item.as_list() else { continue };
        let field = pair
            .first()
            .and_then(|value| value.as_keyword().or_else(|| value.as_symbol()));
        if field == Some(key) {
            return pair.get(1).cloned().unwrap_or(Value::Nil);
        }
    }
    Value::Nil
}

/// A deterministic in-memory records host for the first runtime slice.
pub struct FakeHost {
    records: HashMap<String, Vec<Value>>,
    allow_all: bool,
    grants: Vec<(String, String)>,
}

impl Default for FakeHost {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeHost {
    pub fn new() -> Self {
        Self {
            records: HashMap::new(),
            allow_all: true,
            grants: Vec::new(),
        }
    }

    /// Add one record to the in-memory host. Test/demo data only.
    pub fn insert(&mut self, collection: &str, id: &str, body: &[u8]) {
        self.records
            .entry(collection.to_string())
            .or_default()
            .push(record_value(collection, id, body));
    }

    pub fn deny_all(mut self) -> Self {
        self.allow_all = false;
        self.grants.clear();
        self
    }

    pub fn grant(mut self, interface: &str, function: &str) -> Self {
        self.allow_all = false;
        self.grants
            .push((interface.to_string(), function.to_string()));
        self
    }

    pub fn records(&self) -> &HashMap<String, Vec<Value>> {
        &self.records
    }
}

impl Host for FakeHost {
    fn granted(&self, interface: &str, function: &str) -> bool {
        self.allow_all
            || self
                .grants
                .iter()
                .any(|(i, f)| i == interface && f == function)
    }

    fn call(
        &mut self,
        interface: &str,
        function: &str,
        args: &[Value],
    ) -> std::result::Result<Value, String> {
        if interface != "soil:records" {
            return Err(format!("unknown interface `{interface}`"));
        }

        match function {
            "scan" | "changes" => {
                let collection = args
                    .first()
                    .map(value_text)
                    .ok_or_else(|| "scan needs a collection".to_string())?;
                let limit = args.get(2).and_then(Value::as_f64).unwrap_or(50.0) as usize;
                let entries = self
                    .records
                    .get(&collection)
                    .map(|records| records.iter().take(limit).cloned().collect())
                    .unwrap_or_default();
                Ok(ok(page_value(entries)))
            }
            "get" => {
                let key = args.first().ok_or_else(|| "get needs a key".to_string())?;
                let (collection, id) = parse_key(key)?;
                let found = self
                    .records
                    .get(&collection)
                    .and_then(|records| {
                        records.iter().find(|record| {
                            record
                                .as_list()
                                .map(|items| {
                                    field_value(
                                        &Value::List(items.to_vec()),
                                        &Value::Keyword("key".to_string()),
                                    )
                                    .as_list()
                                    .map(|key| {
                                        field_value(
                                            &Value::List(key.to_vec()),
                                            &Value::Keyword("id".to_string()),
                                        )
                                        .as_str()
                                            == Some(&id)
                                    })
                                    .unwrap_or(false)
                                })
                                .unwrap_or(false)
                        })
                    })
                    .cloned();
                Ok(match found {
                    Some(record) => ok(record),
                    None => err("not-found"),
                })
            }
            "upsert" => {
                let entry = args
                    .first()
                    .cloned()
                    .ok_or_else(|| "upsert needs an entry".to_string())?;
                let key = field_value(&entry, &Value::Keyword("key".to_string()));
                let (collection, id) = parse_key(&key)?;
                let mut records = self.records.remove(&collection).unwrap_or_default();
                records.retain(|record| {
                    let key = field_value(record, &Value::Keyword("key".to_string()));
                    parse_key(&key)
                        .map(|(_, current)| current != id)
                        .unwrap_or(true)
                });
                records.push(entry.clone());
                self.records.insert(collection, records);
                Ok(ok(entry))
            }
            "delete" => {
                let key = args
                    .first()
                    .ok_or_else(|| "delete needs a key".to_string())?;
                let (collection, id) = parse_key(key)?;
                if let Some(records) = self.records.get_mut(&collection) {
                    records.retain(|record| {
                        let key = field_value(record, &Value::Keyword("key".to_string()));
                        parse_key(&key)
                            .map(|(_, current)| current != id)
                            .unwrap_or(true)
                    });
                }
                Ok(ok(Value::Nil))
            }
            "scan-packed" | "cas" => Ok(err("unsupported")),
            other => Err(format!("unknown soil:records function `{other}`")),
        }
    }
}

fn parse_key(value: &Value) -> Result<(String, String), String> {
    let collection = field_value(value, &Value::Keyword("collection".to_string()))
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| "key needs a collection".to_string())?;
    let id = field_value(value, &Value::Keyword("id".to_string()))
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| "key needs an id".to_string())?;
    Ok((collection, id))
}

fn pair(name: &str, value: Value) -> Value {
    Value::List(vec![Value::Keyword(name.to_string()), value])
}

fn key_value(collection: &str, id: &str) -> Value {
    Value::List(vec![
        pair("collection", Value::String(collection.to_string())),
        pair("id", Value::String(id.to_string())),
    ])
}

fn record_value(collection: &str, id: &str, body: &[u8]) -> Value {
    Value::List(vec![
        pair("key", key_value(collection, id)),
        pair("schema", Value::String("signal.sample.v1".to_string())),
        pair("revision", Value::Int(1)),
        pair("updated-at-ms", Value::Int(0)),
        pair("origin", Value::String("fake".to_string())),
        pair("deleted", Value::Bool(false)),
        pair(
            "body",
            Value::List(body.iter().map(|byte| Value::Int(*byte as i64)).collect()),
        ),
    ])
}

fn page_value(entries: Vec<Value>) -> Value {
    Value::List(vec![
        pair("entries", Value::List(entries)),
        pair("next", Value::Nil),
    ])
}

fn ok(value: Value) -> Value {
    Value::List(vec![Value::Symbol("ok".to_string()), value])
}

fn err(code: &str) -> Value {
    Value::List(vec![
        Value::Symbol("err".to_string()),
        Value::Symbol(code.to_string()),
    ])
}

#[cfg(test)]
mod document_tests {
    use super::*;

    #[test]
    fn read_document_keeps_spans_and_doc_comments() {
        let mut runtime = Runtime::new(Box::new(FakeHost::default()));
        let value = runtime
            .load(
                "(read-document \";;; A markdown doc\\n;;;\\n;;; - detail\\n(defun foo () 1)\\n\")",
            )
            .expect("read-document");
        let forms = field_value(&value, &Value::Keyword("forms".into()));
        let forms = forms.as_list().expect("forms");
        assert_eq!(forms.len(), 1);
        let form = &forms[0];
        assert_eq!(
            field_value(form, &Value::Keyword("value".into())).as_list().unwrap()[1]
                .as_symbol(),
            Some("foo")
        );
        let comments = field_value(form, &Value::Keyword("comments".into()));
        let comments = comments.as_list().expect("comments");
        assert_eq!(comments.len(), 3);
        assert_eq!(
            field_value(&comments[0], &Value::Keyword("text".into())).as_str(),
            Some(";;; A markdown doc")
        );
        assert!(
            field_value(form, &Value::Keyword("start".into()))
                .as_f64()
                .unwrap()
                > 0.0
        );
        let tokens = field_value(&value, &Value::Keyword("tokens".into()));
        let tokens = tokens.as_list().expect("tokens");
        assert!(tokens.iter().any(|token| {
            field_value(token, &Value::Keyword("kind".into())).as_str() == Some("comment")
        }));
    }
}

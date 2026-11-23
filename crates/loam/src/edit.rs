//! Host-owned Spore edit sessions.
//!
//! A signed Spore is immutable. `loam edit` copies its source into a personal
//! session, checkpoints that work tree, saves a full-file overlay patch, and
//! applies the patch under the personal patch root. The base package is never
//! written to and no capability or WIT is added here.
//!
//! Every command writes one JSON object to stdout. Diagnostics go to stderr.
//! The Soil adapter in `std/loam/edit.soil` calls this through `session/exec`
//! and decodes that object.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write as _,
    path::{Component, Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const SESSION_FILE: &str = "session.json";
const LOCK: &str = "spore.lock";
const MANIFEST: &str = "spore.soil";

#[derive(Debug, Clone)]
pub struct EditError {
    pub code: &'static str,
    pub message: String,
}

impl EditError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for EditError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for EditError {}

type Result<T> = std::result::Result<T, EditError>;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Checkpoint {
    seq: u64,
    created: u64,
    changed: Vec<String>,
    files: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Patch {
    id: String,
    created: u64,
    changed: Vec<String>,
    path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BuildState {
    command: String,
    status: String,
    exit_code: Option<i32>,
    created: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Session {
    session: String,
    spore: String,
    version: String,
    base_digest: String,
    package: PathBuf,
    work: PathBuf,
    created: u64,
    status: String,
    /// Digests of the source tree at open time. `save` compares against this,
    /// not against the last checkpoint, so a patch always describes the base.
    base_files: BTreeMap<String, String>,
    #[serde(default)]
    build_command: Option<String>,
    #[serde(default)]
    checkpoint: Option<Checkpoint>,
    #[serde(default)]
    checkpoints: Vec<Checkpoint>,
    #[serde(default)]
    patches: Vec<Patch>,
    #[serde(default)]
    applied_patches: Vec<String>,
    #[serde(default)]
    build: Option<BuildState>,
}

#[derive(Debug, Clone)]
struct ManifestFields {
    id: String,
    version: String,
    source: Option<String>,
    distribution: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Sexp {
    Atom(String),
    String(String),
    List(Vec<Sexp>),
}

pub fn run(args: &[String]) -> i32 {
    match dispatch(args) {
        Ok(value) => {
            if let Err(error) = print_json(&value) {
                eprintln!("loam edit: {error}");
                return 1;
            }
            0
        }
        Err(error) => {
            let value = json!({
                "ok": false,
                "error": error.code,
                "message": error.message,
            });
            if let Err(write_error) = print_json(&value) {
                eprintln!("loam edit: {write_error}");
            }
            eprintln!("loam edit: {}", error.message);
            1
        }
    }
}

fn dispatch(args: &[String]) -> Result<Value> {
    let Some(op) = args.first().map(String::as_str) else {
        return Err(EditError::new("usage", usage()));
    };
    let mut roots = Roots::from_env();
    let mut positional = Vec::new();
    let mut message = None;
    let mut build_command = None;
    let mut force = false;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--json" => i += 1,
            "--edit-root" => {
                roots.edit = PathBuf::from(
                    args.get(i + 1)
                        .ok_or_else(|| EditError::new("usage", "--edit-root DIR"))?,
                );
                i += 2;
            }
            "--patch-root" => {
                roots.patch = PathBuf::from(
                    args.get(i + 1)
                        .ok_or_else(|| EditError::new("usage", "--patch-root DIR"))?,
                );
                i += 2;
            }
            "--message" => {
                message = Some(
                    args.get(i + 1)
                        .ok_or_else(|| EditError::new("usage", "--message TEXT"))?
                        .clone(),
                );
                i += 2;
            }
            "--build-command" => {
                build_command = Some(
                    args.get(i + 1)
                        .ok_or_else(|| EditError::new("usage", "--build-command PROGRAM"))?
                        .clone(),
                );
                i += 2;
            }
            "--force" => {
                force = true;
                i += 1;
            }
            value if value.starts_with('-') => {
                return Err(EditError::new("usage", format!("unknown option {value}")));
            }
            value => {
                positional.push(value.to_string());
                i += 1;
            }
        }
    }
    match op {
        "open" => open(
            &roots,
            positional.first().map(String::as_str),
            build_command,
        ),
        "read" => {
            let session = require_session(&roots, positional.first())?;
            let path = positional
                .get(1)
                .ok_or_else(|| EditError::new("usage", "read needs a relative path"))?;
            read(&roots, &session, path)
        }
        "list" => {
            let session = require_session(&roots, positional.first())?;
            list(&roots, &session)
        }
        "checkpoint" => {
            let session = require_session(&roots, positional.first())?;
            checkpoint(&roots, &session, message.as_deref())
        }
        "build" => {
            let session = require_session(&roots, positional.first())?;
            build(&roots, &session, build_command.as_deref())
        }
        "save" => {
            let session = require_session(&roots, positional.first())?;
            save(&roots, &session, message.as_deref())
        }
        "apply" => {
            let session = require_session(&roots, positional.first())?;
            apply(&roots, &session)
        }
        "discard" => {
            let session = require_session(&roots, positional.first())?;
            discard(&roots, &session, force)
        }
        "help" | "--help" | "-h" => Ok(json!({"ok": true, "usage": usage()})),
        other => Err(EditError::new(
            "usage",
            format!("unknown edit command {other}\n{}", usage()),
        )),
    }
}

fn usage() -> &'static str {
    "usage: loam edit <open|read|list|checkpoint|build|save|apply|discard> ... --json"
}

fn require_session(roots: &Roots, value: Option<&String>) -> Result<String> {
    let value = value.ok_or_else(|| EditError::new("usage", "an edit session is required"))?;
    session_dir(roots, value)?;
    Ok(value.clone())
}

#[derive(Debug, Clone)]
struct Roots {
    edit: PathBuf,
    patch: PathBuf,
}

impl Roots {
    fn from_env() -> Self {
        let data = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share"))
            })
            .unwrap_or_else(|| PathBuf::from(".local/share"));
        Self {
            edit: std::env::var_os("LOAM_EDIT_ROOT")
                .map(PathBuf::from)
                .unwrap_or_else(|| data.join("loam/edit")),
            patch: std::env::var_os("LOAM_PATCH_ROOT")
                .map(PathBuf::from)
                .unwrap_or_else(|| data.join("loam/patches")),
        }
    }
}

fn open(roots: &Roots, package: Option<&str>, build_command: Option<String>) -> Result<Value> {
    let package = package
        .map(PathBuf::from)
        .ok_or_else(|| EditError::new("usage", "open needs a package directory"))?;
    let package = fs::canonicalize(&package).map_err(|error| {
        EditError::new("package-missing", format!("{}: {error}", package.display()))
    })?;
    let manifest_path = package.join(MANIFEST);
    let lock_path = package.join(LOCK);
    if !manifest_path.is_file() {
        return Err(EditError::new(
            "package-invalid",
            format!("{} is missing", manifest_path.display()),
        ));
    }
    if !lock_path.is_file() {
        return Err(EditError::new(
            "package-invalid",
            format!("{} is missing", lock_path.display()),
        ));
    }
    let manifest_text = fs::read_to_string(&manifest_path).map_err(|error| {
        EditError::new(
            "package-invalid",
            format!("{}: {error}", manifest_path.display()),
        )
    })?;
    let fields = parse_manifest(&manifest_text)?;
    if fields.distribution.as_deref() == Some("binary") {
        return Err(EditError::new(
            "source-unavailable",
            "package distribution is binary-only",
        ));
    }
    let Some(source) = fields.source.as_deref() else {
        return Err(EditError::new(
            "source-unavailable",
            "package declares no source directory",
        ));
    };
    let source = safe_relative(source)?;
    let source_path = package.join(&source);
    if !source_path.is_dir() {
        return Err(EditError::new(
            "source-unavailable",
            format!("{} is not a directory", source_path.display()),
        ));
    }
    let (lock_id, lock_version, base_digest) = parse_lock(&lock_path)?;
    if lock_id != fields.id || lock_version != fields.version {
        return Err(EditError::new(
            "lock-mismatch",
            "spore.soil and spore.lock disagree",
        ));
    }
    let session = format!("{}/{}", fields.id, base_digest);
    validate_component(&fields.id)?;
    validate_digest(&base_digest)?;
    let directory = roots.edit.join(&fields.id).join(&base_digest);
    let work = directory.join("work");
    let session_path = directory.join(SESSION_FILE);
    if session_path.is_file() {
        let mut existing = read_session(&directory)?;
        validate_base(&existing)?;
        if !existing.work.is_dir() {
            copy_tree(&source_path, &existing.work)?;
        }
        existing.status = "open".into();
        write_session(&directory, &existing)?;
        return Ok(session_json(&existing, true));
    }
    fs::create_dir_all(&directory).map_err(io_error)?;
    copy_tree(&source_path, &work)?;
    let base_files = tree_digests(&work)?;
    let now = now_secs();
    let session = Session {
        session,
        spore: fields.id,
        version: fields.version,
        base_digest,
        package,
        work,
        created: now,
        status: "open".into(),
        base_files,
        build_command,
        checkpoint: None,
        checkpoints: Vec::new(),
        patches: Vec::new(),
        applied_patches: Vec::new(),
        build: None,
    };
    write_session(&directory, &session)?;
    Ok(session_json(&session, false))
}

fn read(roots: &Roots, session_id: &str, relative: &str) -> Result<Value> {
    let directory = session_dir(roots, session_id)?;
    let session = read_session(&directory)?;
    validate_base(&session)?;
    let (relative, full) = work_file(&session.work, relative)?;
    let bytes = fs::read(&full).map_err(io_error)?;
    if bytes.len() > 16 * 1024 * 1024 {
        return Err(EditError::new("over-budget", "file is larger than 16 MiB"));
    }
    let text = String::from_utf8(bytes).map_err(|_| {
        EditError::new(
            "invalid-utf8",
            format!("{} is not UTF-8 text", relative.display()),
        )
    })?;
    let digest = hex(&Sha256::digest(text.as_bytes()));
    Ok(json!({
        "ok": true,
        "session": session.session,
        "path": relative.to_string_lossy().replace('\\', "/"),
        "text": text,
        "digest": digest,
        "bytes": text.len(),
    }))
}

fn list(roots: &Roots, session_id: &str) -> Result<Value> {
    let directory = session_dir(roots, session_id)?;
    let session = read_session(&directory)?;
    validate_base(&session)?;
    let files = tree_digests(&session.work)?;
    let files: Vec<Value> = files
        .into_iter()
        .map(|(path, digest)| {
            let size = fs::metadata(session.work.join(&path))
                .map(|metadata| metadata.len())
                .unwrap_or(0);
            json!({"path": path, "digest": digest, "bytes": size})
        })
        .collect();
    Ok(json!({
        "ok": true,
        "session": session.session,
        "files": files,
    }))
}

fn checkpoint(roots: &Roots, session_id: &str, _message: Option<&str>) -> Result<Value> {
    let directory = session_dir(roots, session_id)?;
    let mut session = read_session(&directory)?;
    validate_base(&session)?;
    let files = tree_digests(&session.work)?;
    let previous = session
        .checkpoints
        .last()
        .map(|checkpoint| &checkpoint.files)
        .unwrap_or(&session.base_files);
    let changed = changed_paths(previous, &files);
    if changed.is_empty() {
        return Ok(json!({
            "ok": true,
            "session": session.session,
            "checkpoint": session.checkpoints.last().map(|checkpoint| checkpoint.seq),
            "changed": false,
            "paths": [],
        }));
    }
    let seq = session
        .checkpoints
        .last()
        .map_or(1, |checkpoint| checkpoint.seq + 1);
    let target = directory.join("checkpoints").join(format!("{seq:04}"));
    copy_tree(&session.work, &target)?;
    let checkpoint = Checkpoint {
        seq,
        created: now_secs(),
        changed: changed.clone(),
        files,
    };
    session.checkpoints.push(checkpoint.clone());
    session.checkpoint = Some(checkpoint);
    session.status = "checkpointed".into();
    write_session(&directory, &session)?;
    Ok(json!({
        "ok": true,
        "session": session.session,
        "checkpoint": seq,
        "changed": true,
        "paths": changed,
    }))
}

fn build(roots: &Roots, session_id: &str, override_command: Option<&str>) -> Result<Value> {
    let directory = session_dir(roots, session_id)?;
    let mut session = read_session(&directory)?;
    validate_base(&session)?;
    let Some(command) = override_command.or(session.build_command.as_deref()) else {
        session.build = Some(BuildState {
            command: String::new(),
            status: "unsupported".into(),
            exit_code: None,
            created: now_secs(),
        });
        session.status = "build-unsupported".into();
        write_session(&directory, &session)?;
        return Err(EditError::new(
            "unsupported",
            "no builder configured; pass --build-command PROGRAM",
        ));
    };
    let build_dir = directory.join("build");
    if build_dir.exists() {
        fs::remove_dir_all(&build_dir).map_err(io_error)?;
    }
    fs::create_dir_all(&build_dir).map_err(io_error)?;
    let output = Command::new(command)
        .current_dir(&session.work)
        .env("LOAM_EDIT_WORK", &session.work)
        .env("LOAM_EDIT_BUILD", &build_dir)
        .output()
        .map_err(|error| EditError::new("build-failed", format!("{command}: {error}")))?;
    fs::write(build_dir.join("stdout.log"), &output.stdout).map_err(io_error)?;
    fs::write(build_dir.join("stderr.log"), &output.stderr).map_err(io_error)?;
    let code = output.status.code();
    let state = BuildState {
        command: command.to_string(),
        status: if output.status.success() {
            "ok"
        } else {
            "failed"
        }
        .into(),
        exit_code: code,
        created: now_secs(),
    };
    fs::write(
        build_dir.join("status.json"),
        serde_json::to_vec_pretty(&state).map_err(json_error)?,
    )
    .map_err(io_error)?;
    session.build = Some(state);
    if output.status.success() {
        session.status = "built".into();
        write_session(&directory, &session)?;
        Ok(json!({"ok": true, "session": session.session, "built": true, "build": build_dir}))
    } else {
        session.status = "build-failed".into();
        write_session(&directory, &session)?;
        Err(EditError::new(
            "build-failed",
            format!(
                "builder exited with {code:?}; logs are in {}",
                build_dir.display()
            ),
        ))
    }
}

fn save(roots: &Roots, session_id: &str, _message: Option<&str>) -> Result<Value> {
    let directory = session_dir(roots, session_id)?;
    let mut session = read_session(&directory)?;
    validate_base(&session)?;
    if session.status == "build-failed" {
        return Err(EditError::new(
            "build-failed",
            "the last build failed; fix it or rebuild before saving",
        ));
    }
    let files = tree_digests(&session.work)?;
    let changed = changed_paths(&session.base_files, &files);
    if changed.is_empty() {
        return Ok(json!({
            "ok": true,
            "session": session.session,
            "changed": false,
            "patch": Value::Null,
        }));
    }
    if let Some(previous) = session.patches.last()
        && previous.changed == changed
        && patch_matches(&previous.path, &session.work, &changed, &files)?
    {
        return Ok(json!({
            "ok": true,
            "session": session.session,
            "changed": false,
            "patch": previous.id,
            "patch_path": previous.path,
        }));
    }
    for path in &changed {
        if path == "wit" || path.starts_with("wit/") {
            return Err(EditError::new(
                "wit-change",
                "WIT is not patchable; fork the Spore",
            ));
        }
        if path == "module.wasm" {
            return Err(EditError::new(
                "module-change",
                "the module is not patchable in this slice",
            ));
        }
    }
    let now = now_secs();
    let mut hasher = Sha256::new();
    for path in &changed {
        hasher.update(path.as_bytes());
        hasher.update(b"\0");
        hasher.update(files[path].as_bytes());
        hasher.update(b"\0");
    }
    let id = format!("patch-{now}-{}", &hex(&hasher.finalize())[..12]);
    let patch_dir = directory.join("patches").join(&id);
    write_patch(&patch_dir, &session, &id, now, &changed, &files)?;
    session.patches.push(Patch {
        id: id.clone(),
        created: now,
        changed: changed.clone(),
        path: patch_dir.clone(),
    });
    session.status = "saved".into();
    write_session(&directory, &session)?;
    Ok(json!({
        "ok": true,
        "session": session.session,
        "changed": true,
        "patch": id,
        "patch_path": patch_dir,
        "paths": changed,
    }))
}

fn apply(roots: &Roots, session_id: &str) -> Result<Value> {
    let directory = session_dir(roots, session_id)?;
    let mut session = read_session(&directory)?;
    validate_base(&session)?;
    let mut applied = Vec::new();
    for patch in session.patches.clone() {
        if session.applied_patches.contains(&patch.id) {
            continue;
        }
        let target = roots
            .patch
            .join(&session.spore)
            .join(&session.base_digest)
            .join(&patch.id);
        if target.exists() {
            fs::remove_dir_all(&target).map_err(io_error)?;
        }
        copy_tree(&patch.path, &target)?;
        session.applied_patches.push(patch.id.clone());
        applied.push(patch.id);
    }
    session.status = "applied".into();
    write_session(&directory, &session)?;
    Ok(json!({
        "ok": true,
        "session": session.session,
        "applied": applied,
        "patch_root": roots.patch.join(&session.spore).join(&session.base_digest),
        "restart_required": false,
    }))
}

fn discard(roots: &Roots, session_id: &str, force: bool) -> Result<Value> {
    let directory = session_dir(roots, session_id)?;
    let mut session = read_session(&directory)?;
    if force {
        fs::remove_dir_all(&directory).map_err(io_error)?;
        return Ok(json!({"ok": true, "session": session_id, "removed": true, "force": true}));
    }
    let work = session.work.clone();
    if work.exists() {
        fs::remove_dir_all(&work).map_err(io_error)?;
    }
    let build = directory.join("build");
    if build.exists() {
        fs::remove_dir_all(&build).map_err(io_error)?;
    }
    session.status = "discarded".into();
    write_session(&directory, &session)?;
    Ok(json!({
        "ok": true,
        "session": session_id,
        "removed": true,
        "force": false,
        "kept": ["checkpoints", "patches", "session.json"],
    }))
}

fn patch_matches(
    patch_dir: &Path,
    work: &Path,
    changed: &[String],
    files: &BTreeMap<String, String>,
) -> Result<bool> {
    for path in changed {
        let overlay = patch_dir.join("overlay").join(path);
        if !overlay.is_file() {
            return Ok(false);
        }
        let digest = hex(&Sha256::digest(fs::read(&overlay).map_err(io_error)?));
        if Some(&digest) != files.get(path) || !work.join(path).is_file() {
            return Ok(false);
        }
    }
    Ok(true)
}

fn write_patch(
    patch_dir: &Path,
    session: &Session,
    id: &str,
    created: u64,
    changed: &[String],
    files: &BTreeMap<String, String>,
) -> Result<()> {
    fs::create_dir_all(patch_dir.join("overlay")).map_err(io_error)?;
    let mut manifest = String::from("(patch\n  (schema 1)\n  (kind \"patch\")\n");
    manifest.push_str(&format!("  (id {})\n", quote(id)));
    manifest.push_str(&format!("  (base {})\n", quote(&session.base_digest)));
    manifest.push_str(&format!("  (spore {})\n", quote(&session.spore)));
    manifest.push_str(&format!("  (created {created})\n"));
    manifest.push_str("  (files\n");
    for path in changed {
        manifest.push_str(&format!("    ({} {})\n", quote(path), quote(&files[path])));
        let target = patch_dir.join("overlay").join(path);
        copy_file(&session.work.join(path), &target)?;
    }
    manifest.push_str("  ))\n");
    fs::write(patch_dir.join("patch.soil"), manifest).map_err(io_error)?;
    // Signing hook: a later slice may write signatures/<keyid>.sig here.
    Ok(())
}

fn read_session(directory: &Path) -> Result<Session> {
    let path = directory.join(SESSION_FILE);
    let bytes = fs::read(&path).map_err(|error| {
        EditError::new("session-missing", format!("{}: {error}", path.display()))
    })?;
    let session: Session = serde_json::from_slice(&bytes).map_err(|error| {
        EditError::new("session-invalid", format!("{}: {error}", path.display()))
    })?;
    Ok(session)
}

fn write_session(directory: &Path, session: &Session) -> Result<()> {
    fs::create_dir_all(directory).map_err(io_error)?;
    let bytes = serde_json::to_vec_pretty(session).map_err(json_error)?;
    fs::write(directory.join(SESSION_FILE), bytes).map_err(io_error)
}

fn validate_base(session: &Session) -> Result<()> {
    let (id, version, digest) = parse_lock(&session.package.join(LOCK))?;
    if id != session.spore || version != session.version {
        return Err(EditError::new(
            "base-changed",
            "the base package identity changed",
        ));
    }
    if digest != session.base_digest {
        return Err(EditError::new("base-changed", "the base digest changed"));
    }
    Ok(())
}

fn session_dir(roots: &Roots, session: &str) -> Result<PathBuf> {
    let (spore, digest) = session.split_once('/').ok_or_else(|| {
        EditError::new(
            "session-invalid",
            "session must be <spore-id>/<base-digest>",
        )
    })?;
    if spore.contains('/') {
        return Err(EditError::new(
            "session-invalid",
            "session has too many path parts",
        ));
    }
    validate_component(spore)?;
    validate_digest(digest)?;
    let directory = roots.edit.join(spore).join(digest);
    if !directory.join(SESSION_FILE).is_file() {
        return Err(EditError::new(
            "session-missing",
            format!("{} does not exist", directory.display()),
        ));
    }
    Ok(directory)
}

fn validate_component(value: &str) -> Result<()> {
    if value.is_empty() || value == "." || value == ".." || value.contains(['/', '\\', '\0']) {
        return Err(EditError::new(
            "unsafe-path",
            format!("unsafe id {value:?}"),
        ));
    }
    Ok(())
}

fn validate_digest(value: &str) -> Result<()> {
    if value.len() != 64 || !value.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(EditError::new(
            "lock-mismatch",
            "base digest is not a sha256 hex digest",
        ));
    }
    Ok(())
}

fn parse_manifest(source: &str) -> Result<ManifestFields> {
    let forms = parse_sexps(source)?;
    let spore = forms
        .iter()
        .find_map(|form| match form {
            Sexp::List(items)
                if matches!(items.first(), Some(Sexp::Atom(name)) if name == "spore") =>
            {
                Some(items)
            }
            _ => None,
        })
        .ok_or_else(|| EditError::new("package-invalid", "spore.soil has no (spore ...) form"))?;
    let mut id = None;
    let mut version = None;
    let mut source_dir = None;
    let mut distribution = None;
    for field in spore.iter().skip(1) {
        let Sexp::List(pair) = field else {
            continue;
        };
        let (Some(Sexp::Atom(key)), Some(value)) = (pair.first(), pair.get(1)) else {
            continue;
        };
        let value = match value {
            Sexp::String(value) => Some(value.clone()),
            _ => None,
        };
        match key.as_str() {
            "id" => id = value,
            "version" => version = value,
            "source" => source_dir = value,
            "distribution" => distribution = value,
            _ => {}
        }
    }
    Ok(ManifestFields {
        id: id.ok_or_else(|| EditError::new("package-invalid", "spore.soil has no id"))?,
        version: version
            .ok_or_else(|| EditError::new("package-invalid", "spore.soil has no version"))?,
        source: source_dir,
        distribution,
    })
}

fn parse_sexps(source: &str) -> Result<Vec<Sexp>> {
    let mut stack = vec![Vec::new()];
    let mut chars = source.char_indices().peekable();
    while let Some((_, ch)) = chars.next() {
        match ch {
            ';' => {
                for (_, next) in chars.by_ref() {
                    if next == '\n' {
                        break;
                    }
                }
            }
            '(' => stack.push(Vec::new()),
            ')' => {
                let list = stack
                    .pop()
                    .ok_or_else(|| EditError::new("package-invalid", "unbalanced )"))?;
                stack
                    .last_mut()
                    .ok_or_else(|| EditError::new("package-invalid", "unbalanced )"))?
                    .push(Sexp::List(list));
            }
            '"' => {
                let mut value = String::new();
                loop {
                    match chars.next().map(|(_, ch)| ch) {
                        Some('"') => break,
                        Some('\\') => match chars.next().map(|(_, ch)| ch) {
                            Some('n') => value.push('\n'),
                            Some('t') => value.push('\t'),
                            Some(ch) => value.push(ch),
                            None => {
                                return Err(EditError::new(
                                    "package-invalid",
                                    "unterminated string",
                                ));
                            }
                        },
                        Some(ch) => value.push(ch),
                        None => {
                            return Err(EditError::new("package-invalid", "unterminated string"));
                        }
                    }
                }
                stack
                    .last_mut()
                    .ok_or_else(|| EditError::new("package-invalid", "string outside form"))?
                    .push(Sexp::String(value));
            }
            ch if ch.is_whitespace() => {}
            ch => {
                let mut atom = String::from(ch);
                while let Some((_, next)) = chars.peek().copied() {
                    if next.is_whitespace() || matches!(next, '(' | ')' | ';' | '"') {
                        break;
                    }
                    atom.push(next);
                    chars.next();
                }
                stack
                    .last_mut()
                    .ok_or_else(|| EditError::new("package-invalid", "atom outside form"))?
                    .push(Sexp::Atom(atom));
            }
        }
    }
    if stack.len() != 1 {
        return Err(EditError::new("package-invalid", "unbalanced ("));
    }
    Ok(stack.pop().unwrap_or_default())
}

fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn parse_lock(path: &Path) -> Result<(String, String, String)> {
    let text = fs::read_to_string(path).map_err(|error| {
        EditError::new("package-invalid", format!("{}: {error}", path.display()))
    })?;
    let mut id = None;
    let mut version = None;
    let mut digest = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((key, rest)) = line.split_once(' ') else {
            continue;
        };
        match key {
            "id" => id = Some(rest.trim().to_string()),
            "version" => version = Some(rest.trim().to_string()),
            "digest" => digest = Some(rest.trim().to_string()),
            _ => {}
        }
    }
    let digest =
        digest.ok_or_else(|| EditError::new("lock-mismatch", "spore.lock has no digest"))?;
    validate_digest(&digest)?;
    Ok((
        id.ok_or_else(|| EditError::new("lock-mismatch", "spore.lock has no id"))?,
        version.ok_or_else(|| EditError::new("lock-mismatch", "spore.lock has no version"))?,
        digest,
    ))
}

fn safe_relative(value: &str) -> Result<PathBuf> {
    if value.is_empty() {
        return Err(EditError::new("unsafe-path", "empty path"));
    }
    let path = Path::new(value);
    if path.is_absolute() {
        return Err(EditError::new(
            "unsafe-path",
            format!("absolute path {value:?}"),
        ));
    }
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(EditError::new(
                    "unsafe-path",
                    format!("unsafe path {value:?}"),
                ));
            }
        }
    }
    if out.as_os_str().is_empty() {
        return Err(EditError::new("unsafe-path", "empty path"));
    }
    Ok(out)
}

fn work_file(work: &Path, relative: &str) -> Result<(PathBuf, PathBuf)> {
    let relative = safe_relative(relative)?;
    let root = fs::canonicalize(work).map_err(|error| {
        EditError::new("session-invalid", format!("{}: {error}", work.display()))
    })?;
    let full = work.join(&relative);
    let metadata = fs::symlink_metadata(&full).map_err(|_| {
        EditError::new(
            "not-found",
            format!("{} is not in the session", relative.display()),
        )
    })?;
    if metadata.file_type().is_symlink() {
        return Err(EditError::new(
            "unsafe-path",
            format!("{} is a symlink", relative.display()),
        ));
    }
    if !metadata.is_file() {
        return Err(EditError::new(
            "not-file",
            format!("{} is not a regular file", relative.display()),
        ));
    }
    let canonical = fs::canonicalize(&full).map_err(io_error)?;
    if !canonical.starts_with(&root) {
        return Err(EditError::new(
            "unsafe-path",
            format!("{} escapes the work tree", relative.display()),
        ));
    }
    Ok((relative, canonical))
}

fn copy_tree(source: &Path, target: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(source).map_err(io_error)?;
    if metadata.file_type().is_symlink() {
        return Err(EditError::new(
            "unsafe-path",
            format!("{} is a symlink", source.display()),
        ));
    }
    if metadata.is_dir() {
        fs::create_dir_all(target).map_err(io_error)?;
        for entry in fs::read_dir(source).map_err(io_error)? {
            let entry = entry.map_err(io_error)?;
            let name = entry.file_name();
            if name == ".git" {
                continue;
            }
            copy_tree(&entry.path(), &target.join(name))?;
        }
    } else if metadata.is_file() {
        copy_file(source, target)?;
    } else {
        return Err(EditError::new(
            "unsafe-path",
            format!("{} is not a regular file", source.display()),
        ));
    }
    Ok(())
}

fn copy_file(source: &Path, target: &Path) -> Result<()> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(io_error)?;
    }
    fs::copy(source, target).map_err(io_error)?;
    Ok(())
}

fn tree_digests(root: &Path) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    collect_digests(root, root, &mut out)?;
    Ok(out)
}

fn collect_digests(root: &Path, path: &Path, out: &mut BTreeMap<String, String>) -> Result<()> {
    for entry in fs::read_dir(path).map_err(io_error)? {
        let entry = entry.map_err(io_error)?;
        let full = entry.path();
        let metadata = fs::symlink_metadata(&full).map_err(io_error)?;
        if metadata.file_type().is_symlink() {
            return Err(EditError::new(
                "unsafe-path",
                format!("{} is a symlink", full.display()),
            ));
        }
        if metadata.is_dir() {
            collect_digests(root, &full, out)?;
        } else if metadata.is_file() {
            let relative = full
                .strip_prefix(root)
                .map_err(|_| EditError::new("unsafe-path", "file escaped the work tree"))?
                .to_string_lossy()
                .replace('\\', "/");
            let digest = hex(&Sha256::digest(fs::read(&full).map_err(io_error)?));
            out.insert(relative, digest);
        }
    }
    Ok(())
}

fn changed_paths(
    previous: &BTreeMap<String, String>,
    current: &BTreeMap<String, String>,
) -> Vec<String> {
    let keys: BTreeSet<&String> = previous.keys().chain(current.keys()).collect();
    keys.into_iter()
        .filter(|key| previous.get(*key) != current.get(*key))
        .map(ToString::to_string)
        .collect()
}

fn session_json(session: &Session, reopened: bool) -> Value {
    json!({
        "ok": true,
        "session": session.session,
        "spore": session.spore,
        "version": session.version,
        "base_digest": session.base_digest,
        "package": session.package,
        "work": session.work,
        "created": session.created,
        "status": session.status,
        "reopened": reopened,
    })
}

fn print_json(value: &Value) -> std::io::Result<()> {
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    serde_json::to_writer(&mut stdout, value)?;
    stdout.write_all(b"\n")?;
    stdout.flush()
}

fn io_error(error: std::io::Error) -> EditError {
    EditError::new("io", error.to_string())
}

fn json_error(error: serde_json::Error) -> EditError {
    EditError::new("io", error.to_string())
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package(root: &Path, distribution: &str, with_source: bool) -> PathBuf {
        let package = root.join("marbles");
        fs::create_dir_all(&package).unwrap();
        fs::write(
            package.join(MANIFEST),
            format!(
                "(spore\n  (schema 1)\n  (kind \"spore\")\n  (id \"acme:marbles\")\n  (version \"0.1.0\")\n  (distribution \"{distribution}\")\n{}\n  (module \"module.wasm\"))\n",
                if with_source { "  (source \"source/\")\n" } else { "" }
            ),
        )
        .unwrap();
        fs::write(
            package.join(LOCK),
            "spore-lock 1\nkind spore\nid acme:marbles\nversion 0.1.0\ndigest aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n",
        )
        .unwrap();
        if with_source {
            fs::create_dir_all(package.join("source")).unwrap();
            fs::write(package.join("source/mb.soil"), "(defun main () 0)\n").unwrap();
        }
        package
    }

    fn roots(root: &Path) -> Roots {
        Roots {
            edit: root.join("edit"),
            patch: root.join("patches"),
        }
    }

    #[test]
    fn open_checkpoint_save_and_apply() {
        let temp = tempfile::tempdir().unwrap();
        let package = package(temp.path(), "both", true);
        let roots = roots(temp.path());
        let opened = open(&roots, package.to_str(), None).unwrap();
        let session = opened["session"].as_str().unwrap().to_string();
        fs::write(
            roots
                .edit
                .join("acme:marbles")
                .join("a".repeat(64))
                .join("work/mb.soil"),
            "(defun main () 1)\n",
        )
        .unwrap();
        let changed = checkpoint(&roots, &session, None).unwrap();
        assert_eq!(changed["changed"], true);
        let noop = checkpoint(&roots, &session, None).unwrap();
        assert_eq!(noop["changed"], false);
        let saved = save(&roots, &session, None).unwrap();
        assert_eq!(saved["changed"], true);
        let applied = apply(&roots, &session).unwrap();
        assert_eq!(applied["applied"].as_array().unwrap().len(), 1);
        assert!(
            roots
                .patch
                .join("acme:marbles")
                .join("a".repeat(64))
                .exists()
        );
        assert!(package.join("source/mb.soil").is_file());
    }

    #[test]
    fn binary_only_is_source_unavailable() {
        let temp = tempfile::tempdir().unwrap();
        let package = package(temp.path(), "binary", false);
        let error = open(&roots(temp.path()), package.to_str(), None).unwrap_err();
        assert_eq!(error.code, "source-unavailable");
    }

    #[test]
    fn read_and_list_work_files() {
        let temp = tempfile::tempdir().unwrap();
        let package = package(temp.path(), "both", true);
        let roots = roots(temp.path());
        let opened = open(&roots, package.to_str(), None).unwrap();
        let session = opened["session"].as_str().unwrap().to_string();
        let opened_file = read(&roots, &session, "mb.soil").unwrap();
        assert!(opened_file["text"].as_str().unwrap().contains("main"));
        let listed = list(&roots, &session).unwrap();
        assert!(
            listed["files"]
                .as_array()
                .unwrap()
                .iter()
                .any(|file| file["path"] == "mb.soil")
        );
        assert_eq!(
            read(&roots, &session, "../spore.lock").unwrap_err().code,
            "unsafe-path"
        );
        assert_eq!(
            read(&roots, &session, "/etc/passwd").unwrap_err().code,
            "unsafe-path"
        );
        assert_eq!(
            read(&roots, &session, "missing.soil").unwrap_err().code,
            "not-found"
        );
    }

    #[cfg(unix)]
    #[test]
    fn read_refuses_a_work_symlink() {
        let temp = tempfile::tempdir().unwrap();
        let package = package(temp.path(), "both", true);
        let roots = roots(temp.path());
        let opened = open(&roots, package.to_str(), None).unwrap();
        let session = opened["session"].as_str().unwrap().to_string();
        let work = roots
            .edit
            .join("acme:marbles")
            .join("a".repeat(64))
            .join("work");
        std::os::unix::fs::symlink("mb.soil", work.join("link.soil")).unwrap();
        assert_eq!(
            read(&roots, &session, "link.soil").unwrap_err().code,
            "unsafe-path"
        );
    }

    #[test]
    fn absolute_and_parent_paths_are_refused() {
        assert_eq!(safe_relative("../escape").unwrap_err().code, "unsafe-path");
        assert_eq!(
            safe_relative("/tmp/escape").unwrap_err().code,
            "unsafe-path"
        );
        assert_eq!(
            safe_relative("a/../../escape").unwrap_err().code,
            "unsafe-path"
        );
    }

    #[test]
    fn save_refuses_wit_changes() {
        let temp = tempfile::tempdir().unwrap();
        let package = package(temp.path(), "both", true);
        let roots = roots(temp.path());
        let opened = open(&roots, package.to_str(), None).unwrap();
        let session = opened["session"].as_str().unwrap().to_string();
        let work = roots
            .edit
            .join("acme:marbles")
            .join("a".repeat(64))
            .join("work");
        fs::create_dir_all(work.join("wit")).unwrap();
        fs::write(work.join("wit/marbles.wit"), "(package changed)").unwrap();
        let error = save(&roots, &session, None).unwrap_err();
        assert_eq!(error.code, "wit-change");
    }

    #[test]
    fn save_does_not_duplicate_an_unchanged_patch() {
        let temp = tempfile::tempdir().unwrap();
        let package = package(temp.path(), "both", true);
        let roots = roots(temp.path());
        let opened = open(&roots, package.to_str(), None).unwrap();
        let session = opened["session"].as_str().unwrap().to_string();
        let work = roots
            .edit
            .join("acme:marbles")
            .join("a".repeat(64))
            .join("work");
        fs::write(work.join("mb.soil"), "(defun main () 2)\n").unwrap();
        let first = save(&roots, &session, None).unwrap();
        let second = save(&roots, &session, None).unwrap();
        assert_eq!(first["changed"], true);
        assert_eq!(second["changed"], false);
        assert_eq!(first["patch"], second["patch"]);
    }

    #[test]
    fn source_symlink_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let package = package(temp.path(), "both", true);
        fs::remove_file(package.join("source/mb.soil")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("mb.soil", package.join("source/link.soil")).unwrap();
        let error = open(&roots(temp.path()), package.to_str(), None).unwrap_err();
        assert_eq!(error.code, "unsafe-path");
    }
}

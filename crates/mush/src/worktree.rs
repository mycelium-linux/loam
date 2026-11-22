//! Worktree and tree records for Mush.
//!
//! The VFS stays the file store. This module records the collaborative
//! objects above it: a tree is a content-addressed manifest of a VFS subtree,
//! a worktree is a named ref into that tree, and a thread owns worktrees. The
//! records live in `soil:records` so they can move to a synced book later
//! without changing the VFS schema.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

use ::soil::records_host::{RecordsBookHost, records};
use anyhow::{Context, Result, anyhow, bail};
use fungos_vfs::{ApplyOutcome, ContentId, Expect, Vfs, pull_over_ssh};
use rand::Rng;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

const THREAD_SCHEMA: &str = "fungos.thread/v1";
const TURN_SCHEMA: &str = "fungos.turn/v1";
const WORKTREE_SCHEMA: &str = "fungos.worktree/v1";
const TREE_SCHEMA: &str = "fungos.tree/v1";
const REF_SCHEMA: &str = "fungos.ref/v1";
const CHECKOUT_SCHEMA: &str = "fungos.checkout/v1";
const WORKTREE_STATE_SCHEMA: &str = "fungos.worktree-state/v1";
const PROJECT_SCHEMA: &str = "fungos.project/v1";
const WORKSPACE_SCHEMA: &str = "fungos.workspace/v1";

/// The only collection in the device-local book.
const LOCAL_COLLECTION: &str = "local";
const COMMENT_SCHEMA: &str = "fungos.comment/v1";

const SCHEMAS: &[(&str, &str)] = &[
    ("thread", THREAD_SCHEMA),
    ("turn", TURN_SCHEMA),
    ("worktree", WORKTREE_SCHEMA),
    ("tree", TREE_SCHEMA),
    ("ref", REF_SCHEMA),
    ("checkout", CHECKOUT_SCHEMA),
    ("worktree-state", WORKTREE_STATE_SCHEMA),
    ("project", PROJECT_SCHEMA),
    ("workspace", WORKSPACE_SCHEMA),
    ("comment", COMMENT_SCHEMA),
];

#[derive(Debug, Serialize, Deserialize)]
struct Thread {
    id: String,
    title: String,
    parent: Option<String>,
    created_at_ms: u64,
}

/// Reserved for the agent/turn record; worktree v0 creates threads only.
#[allow(dead_code)]
#[derive(Debug, Serialize, Deserialize)]
struct Turn {
    thread: String,
    sequence: u64,
    state: String,
    producer: Option<String>,
    created_at_ms: u64,
}

/// A worktree as every device agrees it is.
///
/// Deliberately no path, no base/head tree and no timestamp of the last local
/// look: those describe one machine's view, and putting them in a synced book
/// means two devices trade lies about each other's filesystem.
#[derive(Debug, Serialize, Deserialize)]
struct Worktree {
    id: String,
    name: String,
    thread: String,
    created_at_ms: u64,
}

/// What *this* device currently has for a worktree.
///
/// Lives in the device-local collection. A synced book must never carry it —
/// the path, and which tree this machine last materialised, are the difference
/// between two checkouts of one worktree.
#[derive(Debug, Serialize, Deserialize)]
struct WorktreeState {
    worktree: String,
    path: String,
    base_tree: String,
    head_tree: String,
    updated_at_ms: u64,
}

/// Where a worktree stands on this device.
#[derive(Debug, Serialize)]
struct WorktreeStatus {
    id: String,
    path: String,
    /// The tree this device last materialised.
    local_head: String,
    /// The tree the synced book points at, which may be newer.
    ref_head: Option<String>,
    /// Uncommitted ops in this store's log.
    pending_ops: usize,
}

/// The worktree record as written before the split, when device state shared
/// the synced collection. Read only by the migration.
#[derive(Debug, Deserialize)]
struct LegacyWorktree {
    id: String,
    name: String,
    thread: String,
    path: Option<String>,
    base_tree: Option<String>,
    head_tree: Option<String>,
    created_at_ms: u64,
    updated_at_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TreeEntry {
    path: String,
    content_id: String,
    size: u64,
    media_type: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct Tree {
    id: String,
    entries: Vec<TreeEntry>,
    created_at_ms: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct Ref {
    name: String,
    worktree: String,
    tree: String,
    updated_at_ms: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct Checkout {
    id: String,
    worktree: String,
    device: String,
    path: String,
    tree: String,
    created_at_ms: u64,
}

/// One repository in a project. The source is where this device keeps the
/// checkout the project was defined from; `base` is the branch the session
/// worktree forks from.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProjectRepo {
    name: String,
    source: String,
    base: String,
}

/// A project is a named set of repositories, not a repository.
///
/// The durable identity is the project id plus its repo names and sources. The
/// session worktrees and their branches are local materializations, recorded in
/// [`Workspace`], so two devices can check out the same project without
/// pretending they have the same paths.
#[derive(Debug, Serialize, Deserialize)]
struct Project {
    id: String,
    title: String,
    desktop: String,
    repos: Vec<ProjectRepo>,
    created_at_ms: u64,
}

/// One repository's worktree inside a session mount root.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct WorkspaceRepo {
    name: String,
    path: String,
    branch: String,
    worktree: String,
}

/// This device's materialization of a project for one session.
///
/// The root is `sessions/<session>/<project>` under the Desktop VFS root. A
/// workspace is not a durable top-level object: it is the session's mount root,
/// and it can be removed and recreated from the project plus the book.
#[derive(Debug, Serialize, Deserialize)]
struct Workspace {
    project: String,
    session: String,
    root: String,
    repos: Vec<WorkspaceRepo>,
    created_at_ms: u64,
}

#[derive(Debug, Serialize)]
struct RepoOutcome {
    name: String,
    branch: String,
    commit: String,
    status: String,
}

/// Reserved for the resource-anchored comment record.
#[allow(dead_code)]
#[derive(Debug, Serialize, Deserialize)]
struct Comment {
    resource: String,
    anchor: String,
    body: String,
    author: String,
    created_at_ms: u64,
}

struct State {
    /// The filesystem root this VFS names. Paths in the book are relative to
    /// it, so a materialized worktree is `<root>/worktrees/<name>` on every
    /// device that mounts that root.
    root: PathBuf,
    vfs: Vfs,
    /// The synced book: thread, worktree identity, tree, ref, comment.
    records: RecordsBookHost,
    collection: String,
    /// The device-local book: checkout, mount owner, this machine's path and
    /// base/head for a worktree.
    local: RecordsBookHost,
    local_collection: String,
}

impl State {
    fn open(
        root: &Path,
        state_root: &Path,
        book: &str,
        collection: &str,
        store: Option<&Path>,
    ) -> Result<Self> {
        // Content lives in a store that is shared by every root on this host
        // unless one is named. A caller that names its own (a test, a sandbox, a
        // service with its own data directory) cannot be affected by, or leak
        // into, anybody else's writes.
        let vfs = Vfs::open_with(root, fungos_vfs::NodeRole::Full, store)
            .with_context(|| format!("open VFS {}", root.display()))?;
        let records = RecordsBookHost::open(state_root, book, collection)
            .with_context(|| format!("open records {book}/{collection}"))?;
        // A separate book, not merely a separate collection: the book host
        // hands back whatever it holds stamped with the collection it was asked
        // for, so a second collection inside the same book is not a boundary —
        // it reads the synced records and a sync of the main book would carry
        // the device ones. Device state gets its own book, which is a real
        // boundary: nothing that publishes `book` can see it.
        let local_book = format!("{book}.local");
        let local_collection = LOCAL_COLLECTION.to_string();
        let local = RecordsBookHost::open(state_root, &local_book, &local_collection)
            .with_context(|| format!("open records {local_book}/{local_collection}"))?;
        let mut state = Self {
            root: root.to_path_buf(),
            vfs,
            records,
            collection: collection.to_string(),
            local,
            local_collection,
        };
        state.migrate_device_state()?;
        Ok(state)
    }

    /// Move device state out of the synced collection for stores written before
    /// the split.
    ///
    /// Idempotent: after a record has been rewritten it carries no path, so a
    /// second open is a no-op. Nothing is deleted from the synced collection
    /// until its local half has been written.
    fn migrate_device_state(&mut self) -> Result<()> {
        let entries = self.records.scan_all(10_000)?;
        let mut pending = Vec::new();
        for entry in entries {
            if entry.deleted || !entry.key.id.starts_with("worktree/") {
                continue;
            }
            let Ok(legacy) = serde_json::from_slice::<LegacyWorktree>(&entry.body) else {
                continue;
            };
            // Only records written before the split carry a path.
            if legacy.path.is_none() {
                continue;
            }
            pending.push(legacy);
        }

        for legacy in pending {
            let head = legacy
                .head_tree
                .clone()
                .or_else(|| legacy.base_tree.clone())
                .unwrap_or_default();
            let path = legacy.path.clone().unwrap_or_default();
            let state = WorktreeState {
                worktree: legacy.id.clone(),
                path,
                base_tree: legacy.base_tree.clone().unwrap_or_else(|| head.clone()),
                head_tree: head,
                updated_at_ms: legacy.updated_at_ms.unwrap_or(legacy.created_at_ms),
            };
            self.put_local(
                worktree_state_record(&legacy.id),
                WORKTREE_STATE_SCHEMA,
                &state,
            )?;
            self.put(
                worktree_record(&legacy.id),
                WORKTREE_SCHEMA,
                &Worktree {
                    id: legacy.id.clone(),
                    name: legacy.name,
                    thread: legacy.thread,
                    created_at_ms: legacy.created_at_ms,
                },
            )?;
        }
        Ok(())
    }

    fn put<T: Serialize>(&mut self, id: String, schema: &str, value: &T) -> Result<()> {
        let body = serde_json::to_vec(value)?;
        let entry = records::Entry {
            key: records::Key {
                collection: self.collection.clone(),
                id: id.clone(),
            },
            schema: schema.to_string(),
            revision: 0,
            updated_at_ms: 0,
            origin: String::new(),
            deleted: false,
            body,
        };
        <RecordsBookHost as records::Host>::upsert(&mut self.records, entry)
            .map_err(|error| anyhow!("write record {id}: {error:?}"))?;
        Ok(())
    }

    /// Write a device-local record. Never the synced book.
    fn put_local<T: Serialize>(&mut self, id: String, schema: &str, value: &T) -> Result<()> {
        let body = serde_json::to_vec(value)?;
        let entry = records::Entry {
            key: records::Key {
                collection: self.local_collection.clone(),
                id: id.clone(),
            },
            schema: schema.to_string(),
            revision: 0,
            updated_at_ms: 0,
            origin: String::new(),
            deleted: false,
            body,
        };
        <RecordsBookHost as records::Host>::upsert(&mut self.local, entry)
            .map_err(|error| anyhow!("write local record {id}: {error:?}"))?;
        Ok(())
    }

    fn get_local<T: DeserializeOwned>(&mut self, id: &str) -> Result<Option<T>> {
        let Some(entry) = self.local.get_entry(id)? else {
            return Ok(None);
        };
        if entry.deleted {
            return Ok(None);
        }
        serde_json::from_slice(&entry.body)
            .with_context(|| format!("decode local record {id}"))
            .map(Some)
    }

    fn get<T: DeserializeOwned>(&mut self, id: &str) -> Result<Option<T>> {
        let Some(entry) = self.records.get_entry(id)? else {
            return Ok(None);
        };
        if entry.deleted {
            return Ok(None);
        }
        serde_json::from_slice(&entry.body)
            .with_context(|| format!("decode record {id}"))
            .map(Some)
    }

    fn scan<T: DeserializeOwned>(&mut self, prefix: &str) -> Result<Vec<T>> {
        let entries = self.records.scan_all(10_000)?;
        let mut values = Vec::new();
        for entry in entries {
            // `scan_all` walks the whole book, so the collection has to be
            // checked here or the local and synced books read each other and
            // the split is cosmetic.
            if entry.deleted
                || entry.key.collection != self.collection
                || !entry.key.id.starts_with(prefix)
            {
                continue;
            }
            values.push(
                serde_json::from_slice(&entry.body)
                    .with_context(|| format!("decode record {}", entry.key.id))?,
            );
        }
        Ok(values)
    }

    #[cfg(test)]
    fn scan_local<T: DeserializeOwned>(&mut self, prefix: &str) -> Result<Vec<T>> {
        let entries = self.local.scan_all(10_000)?;
        let mut values = Vec::new();
        for entry in entries {
            if entry.deleted
                || entry.key.collection != self.local_collection
                || !entry.key.id.starts_with(prefix)
            {
                continue;
            }
            values.push(
                serde_json::from_slice(&entry.body)
                    .with_context(|| format!("decode local record {}", entry.key.id))?,
            );
        }
        Ok(values)
    }

    fn create_worktree(
        &mut self,
        path: &str,
        thread: &str,
        id: Option<&str>,
        name: Option<&str>,
    ) -> Result<Worktree> {
        validate_thread_id(thread)?;
        let path = normalize_path(path)?;
        let tree = snapshot_tree(&self.vfs, &path)?;
        if tree.entries.is_empty() && !path.is_empty() {
            bail!("no files under {path}");
        }
        let id = match id {
            Some(id) => {
                validate_worktree_id(id)?;
                id.to_string()
            }
            None => random_id("wt"),
        };
        if self.get::<Worktree>(&worktree_record(&id))?.is_some() {
            bail!("worktree {id} already exists");
        }

        let thread_record = thread_record(thread);
        if self.get::<Thread>(&thread_record)?.is_none() {
            self.put(
                thread_record,
                THREAD_SCHEMA,
                &Thread {
                    id: thread.to_string(),
                    title: thread.to_string(),
                    parent: None,
                    created_at_ms: now_ms(),
                },
            )?;
        }

        let name = name.map(str::to_string).unwrap_or_else(|| {
            path.rsplit('/')
                .next()
                .filter(|value| !value.is_empty())
                .unwrap_or("root")
                .to_string()
        });
        let now = now_ms();
        let worktree = Worktree {
            id: id.clone(),
            name,
            thread: thread.to_string(),
            created_at_ms: now,
        };
        self.put(tree_record(&tree.id), TREE_SCHEMA, &tree)?;
        self.put(worktree_record(&id), WORKTREE_SCHEMA, &worktree)?;
        self.put_local(
            worktree_state_record(&id),
            WORKTREE_STATE_SCHEMA,
            &WorktreeState {
                worktree: id.clone(),
                path,
                base_tree: tree.id.clone(),
                head_tree: tree.id.clone(),
                updated_at_ms: now,
            },
        )?;
        self.set_ref(&id, "head", &tree.id)?;
        Ok(worktree)
    }

    fn snapshot_worktree(&mut self, id: &str, path: Option<&str>) -> Result<Worktree> {
        let worktree = self.load_worktree(id)?;
        let mut state = self.load_worktree_state(&worktree.id)?;
        let path = match path {
            Some(path) => normalize_path(path)?,
            None => state.path.clone(),
        };
        // Read the directory, not only the index: an agent writes files with
        // `open(2)` and never calls `vfs.put`, so a snapshot that trusts the
        // index records the tree the agent *started* with. Ingest is
        // content-addressed, so a file the store already has costs a hash and
        // nothing else. A root with no directory at that path is an index-only
        // tree (a test, or a sync node that never materializes): there is
        // nothing to read, and the index is the whole truth there.
        if self.root.join(&path).is_dir() {
            self.ingest(&self.root.join(&path), &path)?;
        }
        let tree = snapshot_tree(&self.vfs, &path)?;
        self.put(tree_record(&tree.id), TREE_SCHEMA, &tree)?;
        state.path = path;
        state.head_tree = tree.id.clone();
        state.updated_at_ms = now_ms();
        self.put_local(
            worktree_state_record(&worktree.id),
            WORKTREE_STATE_SCHEMA,
            &state,
        )?;
        self.set_ref(&worktree.id, "head", &state.head_tree)?;
        Ok(worktree)
    }

    fn load_worktree(&mut self, id: &str) -> Result<Worktree> {
        let id = id.strip_prefix("worktree/").unwrap_or(id);
        self.get(&worktree_record(id))?
            .with_context(|| format!("worktree {id} not found"))
    }

    /// Make sure this device has `<root>/worktrees/<name>`, and that the book
    /// knows it as a worktree.
    ///
    /// This is the one call an agent harness needs: "give this session its own
    /// tree, or tell me where mine is". Idempotent on purpose — a restart, a
    /// second session on the same tree, and a device that already materialized
    /// it all take the same path, and the printed directory is the answer.
    ///
    /// A copy is a reflink where the filesystem has one and a real copy where it
    /// does not: `mush project add` already makes the same trade.
    fn ensure_worktree(&mut self, name: &str, thread: &str, from: Option<&str>) -> Result<PathBuf> {
        validate_worktree_id(name)?;
        let relative = normalize_path(&format!("worktrees/{name}"))?;
        let dir = self.root.join(&relative);
        let known = self.get::<Worktree>(&worktree_record(name))?;

        if known.is_none() {
            let source = match from {
                Some(path) => PathBuf::from(path),
                None => self.root.join("projects").join(name),
            };
            if !dir.exists() {
                if !source.is_dir() {
                    bail!("no project at {}; pass --from PATH", source.display());
                }
                if let Some(parent) = dir.parent() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("create {}", parent.display()))?;
                }
                crate::projects::cow_copy(&source, &dir)?;
            }
            // The VFS is an index over content, not a filesystem: a copy that
            // only lands on disk is invisible to `snapshot` and `checkout`
            // (which is every device that is not this one). Record what is in
            // the directory, so the tree and the directory agree.
            let files = self.ingest(&dir, &relative)?;
            if files == 0 {
                bail!("nothing to record under {}", dir.display());
            }
            self.create_worktree(&relative, thread, Some(name), Some(name))?;
        } else if !dir.exists() {
            // The book has it; this device never materialized it. Checkout needs
            // this device's head, which only a pull fills in — say that instead
            // of copying the base tree over whatever the book has since said.
            bail!(
                "worktree {name} is known but not materialized on this device; \
                 run `mush worktree pull {name} --from HOST --remote-root PATH` first"
            );
        }
        Ok(dir)
    }

    /// Put a directory's files into the VFS under `target`.
    ///
    /// Content-addressed, so a file the store already has at this path is left
    /// alone instead of gaining a new version on every call. This is what makes
    /// `ensure` idempotent on a machine where the mount is present and the copy
    /// went through it: the second pass writes nothing.
    fn ingest(&mut self, source: &Path, target: &str) -> Result<usize> {
        let mut files = 0;
        let mut stack = vec![source.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in
                std::fs::read_dir(&dir).with_context(|| format!("read {}", dir.display()))?
            {
                let entry = entry?;
                let path = entry.path();
                let kind = entry.file_type()?;
                if kind.is_dir() {
                    stack.push(path);
                    continue;
                }
                if !kind.is_file() {
                    continue;
                }
                let relative = path
                    .strip_prefix(source)
                    .map(|rest| rest.to_string_lossy().replace('\\', "/"))
                    .unwrap_or_default();
                let vpath = format!("{target}/{relative}");
                let bytes =
                    std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
                let id = ContentId::from_bytes(&bytes);
                if let Some(existing) = self.vfs.file(&vpath)?
                    && existing.content_id == id
                {
                    files += 1;
                    continue;
                }
                self.vfs
                    .put(&vpath, &bytes, media_type(&path), Expect::Any)
                    .with_context(|| format!("record {vpath}"))?;
                files += 1;
            }
        }
        Ok(files)
    }

    fn define_project(
        &mut self,
        desktop: &str,
        id: &str,
        title: &str,
        sources: Vec<String>,
    ) -> Result<Project> {
        validate_worktree_id(id)?;
        if sources.is_empty() {
            bail!("a project needs at least one --repo");
        }
        let mut repos = Vec::new();
        let mut names = BTreeSet::new();
        for source in sources {
            let source = crate::config::expand_home(&source);
            let root = crate::project::checkout_root(&source)
                .with_context(|| format!("{} is not a git checkout", source.display()))?;
            let name = root
                .file_name()
                .and_then(|name| name.to_str())
                .filter(|name| !name.is_empty())
                .ok_or_else(|| anyhow!("cannot name repository {}", root.display()))?
                .to_string();
            if !names.insert(name.clone()) {
                bail!("duplicate repository name {name:?}");
            }
            let facts = crate::project::facts(&root);
            let base = facts.branch.unwrap_or_else(|| "HEAD".to_string());
            repos.push(ProjectRepo {
                name,
                source: root.display().to_string(),
                base,
            });
        }
        let project = Project {
            id: id.to_string(),
            title: title.to_string(),
            desktop: desktop.to_string(),
            repos,
            created_at_ms: now_ms(),
        };
        self.put(project_record(id), PROJECT_SCHEMA, &project)?;
        Ok(project)
    }

    /// Add one repository to an existing project.
    ///
    /// Add-only on purpose: an agent may request that a project include another
    /// checkout, but removing a repo is a project edit a human should make with
    /// the project manifest in front of them. Re-adding the same name and
    /// source is idempotent.
    fn add_project_repo(&mut self, id: &str, source: &str) -> Result<Project> {
        let mut project = self
            .get::<Project>(&project_record(id))?
            .with_context(|| format!("project {id} not found"))?;
        let source = crate::config::expand_home(source);
        let root = crate::project::checkout_root(&source)
            .with_context(|| format!("{} is not a git checkout", source.display()))?;
        let name = root
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .ok_or_else(|| anyhow!("cannot name repository {}", root.display()))?
            .to_string();
        if let Some(existing) = project.repos.iter().find(|repo| repo.name == name) {
            if Path::new(&existing.source) == root {
                return Ok(project);
            }
            bail!("project {id} already has a repo named {name:?}");
        }
        let facts = crate::project::facts(&root);
        project.repos.push(ProjectRepo {
            name,
            source: root.display().to_string(),
            base: facts.branch.unwrap_or_else(|| "HEAD".to_string()),
        });
        self.put(project_record(id), PROJECT_SCHEMA, &project)?;
        Ok(project)
    }

    fn checkout_project(&mut self, id: &str, session: &str) -> Result<Workspace> {
        validate_session_id(session)?;
        let project = self
            .get::<Project>(&project_record(id))?
            .with_context(|| format!("project {id} not found"))?;
        let root = normalize_path(&format!("sessions/{session}/{id}"))?;
        std::fs::create_dir_all(self.root.join(&root))
            .with_context(|| format!("create {}", self.root.join(&root).display()))?;

        let previous = self.get_local::<Workspace>(&workspace_record(session, id))?;
        let mut repos = Vec::new();
        for repo in &project.repos {
            let target_relative = normalize_path(&format!("{root}/{}", repo.name))?;
            let target = self.root.join(&target_relative);
            let prior = previous
                .as_ref()
                .and_then(|workspace| workspace.repos.iter().find(|row| row.name == repo.name));
            let branch = prior
                .map(|row| row.branch.clone())
                .unwrap_or_else(|| project_branch(&project.id, session, &repo.name));
            let existed = target.join(".git").exists();
            if !existed {
                if target.exists() {
                    bail!("{} exists but is not a git worktree", target.display());
                }
                add_git_worktree(Path::new(&repo.source), &target, &branch, &repo.base)?;
            }

            let worktree = match prior {
                Some(row) if existed && Path::new(&row.path) == Path::new(&target_relative) => {
                    row.worktree.clone()
                }
                _ => {
                    // The host git worktree is what agents edit. The VFS tree and
                    // worktree record are the portable snapshot of that directory,
                    // so ingest it before recording it.
                    let files = self.ingest(&target, &target_relative)?;
                    if files == 0 {
                        bail!("nothing to record under {}", target.display());
                    }
                    self.create_worktree(
                        &target_relative,
                        session,
                        None,
                        Some(&format!("{id}/{session}/{}", repo.name)),
                    )?
                    .id
                }
            };
            repos.push(WorkspaceRepo {
                name: repo.name.clone(),
                path: target_relative,
                branch,
                worktree,
            });
        }

        let workspace = Workspace {
            project: project.id.clone(),
            session: session.to_string(),
            root,
            repos,
            created_at_ms: now_ms(),
        };
        self.put_local(
            workspace_record(session, &project.id),
            WORKSPACE_SCHEMA,
            &workspace,
        )?;
        Ok(workspace)
    }

    fn load_workspace(&mut self, id: &str, session: &str) -> Result<Workspace> {
        self.get_local(&workspace_record(session, id))?
            .with_context(|| format!("project {id} has no workspace for session {session}"))
    }

    fn commit_project(
        &mut self,
        id: &str,
        session: &str,
        message: &str,
    ) -> Result<Vec<RepoOutcome>> {
        let workspace = self.load_workspace(id, session)?;
        let mut outcomes = Vec::new();
        for repo in &workspace.repos {
            let path = self.root.join(&repo.path);
            run_git(&path, &["add", "-A"])?;
            let dirty = !git_stdout(&path, &["status", "--porcelain"])?
                .trim()
                .is_empty();
            if !dirty {
                outcomes.push(RepoOutcome {
                    name: repo.name.clone(),
                    branch: repo.branch.clone(),
                    commit: git_stdout(&path, &["rev-parse", "HEAD"])?
                        .trim()
                        .to_string(),
                    status: "unchanged".to_string(),
                });
                continue;
            }
            run_git(&path, &["commit", "-m", message])?;
            let commit = git_stdout(&path, &["rev-parse", "HEAD"])?
                .trim()
                .to_string();
            self.snapshot_worktree(&repo.worktree, Some(&repo.path))?;
            outcomes.push(RepoOutcome {
                name: repo.name.clone(),
                branch: repo.branch.clone(),
                commit,
                status: "committed".to_string(),
            });
        }
        Ok(outcomes)
    }

    fn push_project(&mut self, id: &str, session: &str, remote: &str) -> Result<Vec<RepoOutcome>> {
        let workspace = self.load_workspace(id, session)?;
        let mut outcomes = Vec::new();
        for repo in &workspace.repos {
            let path = self.root.join(&repo.path);
            let output = Command::new("git")
                .arg("-C")
                .arg(&path)
                .args(["push", "-u", remote, &repo.branch])
                .output()
                .with_context(|| format!("run git push in {}", path.display()))?;
            if !output.status.success() {
                bail!(
                    "git push {} {}: {}",
                    remote,
                    repo.branch,
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
            outcomes.push(RepoOutcome {
                name: repo.name.clone(),
                branch: repo.branch.clone(),
                commit: git_stdout(&path, &["rev-parse", "HEAD"])?
                    .trim()
                    .to_string(),
                status: "pushed".to_string(),
            });
        }
        Ok(outcomes)
    }

    fn status(&mut self, id: &str) -> Result<WorktreeStatus> {
        let worktree = self.load_worktree(id)?;
        let state = self.load_worktree_state(&worktree.id)?;
        let ref_head = self
            .get::<Ref>(&ref_record(&worktree.id, "head"))?
            .map(|reference| reference.tree);
        let pending_ops = self.vfs.pending(1000)?.len();
        Ok(WorktreeStatus {
            id: worktree.id,
            path: state.path,
            local_head: state.head_tree,
            ref_head,
            pending_ops,
        })
    }

    fn load_worktree_state(&mut self, id: &str) -> Result<WorktreeState> {
        self.get_local(&worktree_state_record(id))?
            .with_context(|| {
                format!(
                    "worktree {id} has no state on this device — mount or snapshot it here first"
                )
            })
    }

    fn load_tree(&mut self, id: &str) -> Result<Tree> {
        let id = id.strip_prefix("tree/").unwrap_or(id);
        self.get(&tree_record(id))?
            .with_context(|| format!("tree {id} not found"))
    }

    fn set_ref(&mut self, worktree: &str, name: &str, tree: &str) -> Result<()> {
        self.put(
            ref_record(worktree, name),
            REF_SCHEMA,
            &Ref {
                name: name.to_string(),
                worktree: worktree.to_string(),
                tree: tree.to_string(),
                updated_at_ms: now_ms(),
            },
        )
    }

    fn checkout(
        &mut self,
        tree: &Tree,
        target: &str,
        worktree: Option<&Worktree>,
    ) -> Result<usize> {
        let target = normalize_path(target)?;
        let mut count = 0;
        for entry in &tree.entries {
            let content_id = ContentId::parse(&entry.content_id)
                .map_err(|error| anyhow!("invalid content id {}: {error:?}", entry.content_id))?;
            let bytes = self.read_content(&content_id)?;
            let path = if target.is_empty() {
                entry.path.clone()
            } else {
                format!("{target}/{}", entry.path)
            };
            self.vfs
                .put(&path, &bytes, &entry.media_type, Expect::Any)?;
            count += 1;
        }
        if let Some(worktree) = worktree {
            let checkout = Checkout {
                id: random_id("co"),
                worktree: worktree.id.clone(),
                device: device_id(),
                path: target,
                tree: tree.id.clone(),
                created_at_ms: now_ms(),
            };
            self.put_local(checkout_record(&checkout.id), CHECKOUT_SCHEMA, &checkout)?;
        }
        Ok(count)
    }

    /// Read an immutable blob from the VFS content store.
    ///
    /// The bytes live in whichever store the VFS was opened against: the
    /// machine's shared store by default, a per-root `.fungos/objects`
    /// directory on a root an older build wrote. Asking the store is the only
    /// layout-independent answer — reading the old path directly is how this
    /// broke when the default moved.
    fn read_content(&self, id: &ContentId) -> Result<Vec<u8>> {
        let bytes = self
            .vfs
            .content(id)
            .with_context(|| format!("read content {id}"))?;
        let actual = ContentId::from_bytes(&bytes);
        if &actual != id {
            bail!("content mismatch for {id}: blob hashes to {actual}");
        }
        Ok(bytes)
    }
}

/// `mush project define|checkout|status|commit|push`
///
/// This is the hierarchy the Desktop document describes: a Project is a named
/// set of repositories, a session gets a mount root under `sessions/<session>/`,
/// and each repository in that root is a real git worktree plus a portable VFS
/// worktree record. The image attaches to the mount root, not to the Desktop.
pub(crate) fn run_project(config: &crate::config::Config, mut args: Vec<String>) -> Result<()> {
    let Some(subcommand) = args.first().cloned() else {
        return project_help();
    };
    args.remove(0);
    match subcommand.as_str() {
        "help" | "--help" | "-h" => return project_help(),
        "define" => {
            let desktop_id = crate::desktop::selected(config, &mut args)
                .ok_or_else(|| anyhow!("no Desktop selected; pass --desktop ID"))?;
            let desktop = crate::desktop::load(&desktop_id)?;
            let title = take_option(&mut args, "--title").unwrap_or_default();
            let mut sources = Vec::new();
            while let Some(source) = take_option(&mut args, "--repo") {
                sources.push(source);
            }
            ensure_no_options(&args)?;
            ensure_arity(&args, 1)?;
            let id = &args[0];
            let title = if title.trim().is_empty() {
                id.clone()
            } else {
                title
            };
            let mut state = open_project_state(&desktop)?;
            let project = state.define_project(&desktop.id, id, &title, sources)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "ok": true,
                    "project": project.id,
                    "desktop": project.desktop,
                    "repos": project.repos.iter().map(|repo| serde_json::json!({
                        "name": repo.name,
                        "source": repo.source,
                        "base": repo.base,
                    })).collect::<Vec<_>>(),
                }))?
            );
            Ok(())
        }
        "add-repo" => {
            let desktop_id = crate::desktop::selected(config, &mut args)
                .ok_or_else(|| anyhow!("no Desktop selected; pass --desktop ID"))?;
            let desktop = crate::desktop::load(&desktop_id)?;
            let source = take_option(&mut args, "--repo").ok_or_else(|| {
                anyhow!("usage: mush project add-repo <project> --repo PATH --desktop ID")
            })?;
            ensure_no_options(&args)?;
            ensure_arity(&args, 1)?;
            let mut state = open_project_state(&desktop)?;
            let project = state.add_project_repo(&args[0], &source)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "ok": true,
                    "project": project.id,
                    "repos": project.repos.iter().map(|repo| serde_json::json!({
                        "name": repo.name,
                        "source": repo.source,
                        "base": repo.base,
                    })).collect::<Vec<_>>(),
                }))?
            );
            Ok(())
        }
        "list" => {
            let desktop_id = crate::desktop::selected(config, &mut args)
                .ok_or_else(|| anyhow!("no Desktop selected; pass --desktop ID"))?;
            let desktop = crate::desktop::load(&desktop_id)?;
            ensure_no_options(&args)?;
            ensure_arity(&args, 0)?;
            let mut state = open_project_state(&desktop)?;
            let mut projects = state.scan::<Project>("project/")?;
            projects.sort_by(|a, b| a.id.cmp(&b.id));
            for project in projects {
                println!(
                    "{}\t{}\t{} repos\t{}",
                    project.id,
                    project.title,
                    project.repos.len(),
                    project
                        .repos
                        .iter()
                        .map(|repo| repo.name.as_str())
                        .collect::<Vec<_>>()
                        .join(",")
                );
            }
            Ok(())
        }
        "show" => {
            let desktop_id = crate::desktop::selected(config, &mut args)
                .ok_or_else(|| anyhow!("no Desktop selected; pass --desktop ID"))?;
            let desktop = crate::desktop::load(&desktop_id)?;
            ensure_no_options(&args)?;
            ensure_arity(&args, 1)?;
            let mut state = open_project_state(&desktop)?;
            let project = state
                .get::<Project>(&project_record(&args[0]))?
                .with_context(|| format!("project {} not found", args[0]))?;
            println!("{}", serde_json::to_string_pretty(&project)?);
            Ok(())
        }
        "checkout" => {
            let desktop_id = crate::desktop::selected(config, &mut args)
                .ok_or_else(|| anyhow!("no Desktop selected; pass --desktop ID"))?;
            let desktop = crate::desktop::load(&desktop_id)?;
            let session = take_session(&mut args).ok_or_else(|| {
                anyhow!("usage: mush project checkout <project> --session <session>")
            })?;
            ensure_no_options(&args)?;
            ensure_arity(&args, 1)?;
            let mut state = open_project_state(&desktop)?;
            let workspace = state.checkout_project(&args[0], &session)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "ok": true,
                    "project": workspace.project,
                    "session": workspace.session,
                    "root": state.root.join(&workspace.root).display().to_string(),
                    "repos": workspace.repos.iter().map(|repo| serde_json::json!({
                        "name": repo.name,
                        "path": repo.path,
                        "branch": repo.branch,
                        "worktree": repo.worktree,
                    })).collect::<Vec<_>>(),
                }))?
            );
            Ok(())
        }
        "status" => {
            let desktop_id = crate::desktop::selected(config, &mut args)
                .ok_or_else(|| anyhow!("no Desktop selected; pass --desktop ID"))?;
            let desktop = crate::desktop::load(&desktop_id)?;
            let session = take_session(&mut args).ok_or_else(|| {
                anyhow!("usage: mush project status <project> --session <session>")
            })?;
            ensure_no_options(&args)?;
            ensure_arity(&args, 1)?;
            let mut state = open_project_state(&desktop)?;
            let workspace = state.load_workspace(&args[0], &session)?;
            let mut rows = Vec::new();
            for repo in &workspace.repos {
                let path = state.root.join(&repo.path);
                let branch = git_stdout(&path, &["rev-parse", "--abbrev-ref", "HEAD"])?
                    .trim()
                    .to_string();
                let commit = git_stdout(&path, &["rev-parse", "HEAD"])?
                    .trim()
                    .to_string();
                let dirty = !git_stdout(&path, &["status", "--porcelain"])?
                    .trim()
                    .is_empty();
                rows.push(serde_json::json!({
                    "name": repo.name,
                    "path": repo.path,
                    "branch": branch,
                    "commit": commit,
                    "dirty": dirty,
                    "worktree": repo.worktree,
                }));
            }
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "ok": true,
                    "project": workspace.project,
                    "session": workspace.session,
                    "root": workspace.root,
                    "repos": rows,
                }))?
            );
            Ok(())
        }
        "commit" => {
            let desktop_id = crate::desktop::selected(config, &mut args)
                .ok_or_else(|| anyhow!("no Desktop selected; pass --desktop ID"))?;
            let desktop = crate::desktop::load(&desktop_id)?;
            let session = take_session(&mut args).ok_or_else(|| {
                anyhow!("usage: mush project commit <project> --session <session> --message MSG")
            })?;
            let message = take_option(&mut args, "--message")
                .or_else(|| take_option(&mut args, "-m"))
                .ok_or_else(|| {
                    anyhow!(
                        "usage: mush project commit <project> --session <session> --message MSG"
                    )
                })?;
            ensure_no_options(&args)?;
            ensure_arity(&args, 1)?;
            let mut state = open_project_state(&desktop)?;
            let outcomes = state.commit_project(&args[0], &session, &message)?;
            println!("{}", serde_json::to_string_pretty(&outcomes)?);
            Ok(())
        }
        "push" => {
            let desktop_id = crate::desktop::selected(config, &mut args)
                .ok_or_else(|| anyhow!("no Desktop selected; pass --desktop ID"))?;
            let desktop = crate::desktop::load(&desktop_id)?;
            let session = take_session(&mut args).ok_or_else(|| {
                anyhow!("usage: mush project push <project> --session <session> [--remote origin]")
            })?;
            let remote = take_option(&mut args, "--remote").unwrap_or_else(|| "origin".to_string());
            ensure_no_options(&args)?;
            ensure_arity(&args, 1)?;
            let mut state = open_project_state(&desktop)?;
            let outcomes = state.push_project(&args[0], &session, &remote)?;
            println!("{}", serde_json::to_string_pretty(&outcomes)?);
            Ok(())
        }
        other => bail!(
            "unknown project subcommand {other:?}; try define, checkout, status, commit, push"
        ),
    }
}

fn open_project_state(desktop: &crate::desktop::Desktop) -> Result<State> {
    let root = desktop.root.clone();
    let state_root = std::env::var("MUSH_STATE_ROOT")
        .map(|value| crate::config::expand_home(&value))
        .unwrap_or_else(|_| root.join(".fungos/records"));
    let book = std::env::var("MUSH_WORKTREE_BOOK").unwrap_or_else(|_| "mush".into());
    let collection =
        std::env::var("MUSH_WORKTREE_COLLECTION").unwrap_or_else(|_| "worktree".into());
    let store = std::env::var("MUSH_VFS_STORE")
        .ok()
        .map(|value| crate::config::expand_home(&value));
    State::open(&root, &state_root, &book, &collection, store.as_deref())
}

fn project_help() -> Result<()> {
    println!(
        "mush project define <id> --desktop <id> --repo PATH [--repo PATH ...] [--title TITLE]\n\
         mush project add-repo <id> --repo PATH --desktop <id>\n\
         mush project list --desktop <id>\n\
         mush project show <id> --desktop <id>\n\
         mush project checkout <id> --session <session> --desktop <id>\n\
         mush project status <id> --session <session> --desktop <id>\n\
         mush project commit <id> --session <session> --message MSG --desktop <id>\n\
         mush project push <id> --session <session> [--remote origin] --desktop <id>"
    );
    Ok(())
}

pub(crate) fn run(
    root: &Path,
    command: &str,
    mut args: Vec<String>,
    state_root: PathBuf,
    book: String,
    collection: String,
    store: Option<&Path>,
) -> Result<()> {
    let mut state = State::open(root, &state_root, &book, &collection, store)?;
    match command {
        "worktree" => run_worktree(&mut state, root, &mut args),
        "tree" => run_tree(&mut state, &mut args),
        other => bail!("unknown command {other:?}"),
    }
}

fn run_worktree(state: &mut State, _root: &Path, args: &mut Vec<String>) -> Result<()> {
    let Some(subcommand) = args.first().cloned() else {
        return worktree_help();
    };
    args.remove(0);
    match subcommand.as_str() {
        "ensure" => {
            let thread = take_option(args, "--thread").ok_or_else(|| {
                anyhow!("usage: mush worktree ensure <name> --thread <thread> [--from PATH]")
            })?;
            let from = take_option(args, "--from");
            ensure_no_options(args)?;
            ensure_arity(args, 1)?;
            let path = state.ensure_worktree(&args[0], &thread, from.as_deref())?;
            // The path, one line: a caller that cannot see this device's layout
            // (an agent harness, a script) learns where its files are from here.
            println!("{}", path.display());
            Ok(())
        }
        "create" => {
            let thread = take_option(args, "--thread")
                .ok_or_else(|| anyhow!("usage: mush worktree create <path> --thread <thread>"))?;
            let id = take_option(args, "--id");
            let name = take_option(args, "--name");
            ensure_no_options(args)?;
            let path = args
                .first()
                .cloned()
                .ok_or_else(|| anyhow!("usage: mush worktree create <path> --thread <thread>"))?;
            ensure_arity(args, 1)?;
            let worktree = state.create_worktree(&path, &thread, id.as_deref(), name.as_deref())?;
            let local = state.load_worktree_state(&worktree.id)?;
            println!(
                "{}\tthread={}\thead={}",
                worktree.id, worktree.thread, local.head_tree
            );
            Ok(())
        }
        "list" => {
            let thread = take_option(args, "--thread");
            ensure_no_options(args)?;
            ensure_arity(args, 0)?;
            let mut worktrees = state.scan::<Worktree>("worktree/")?;
            worktrees.sort_by(|a, b| a.id.cmp(&b.id));
            for worktree in worktrees {
                if thread
                    .as_deref()
                    .is_some_and(|thread| thread != worktree.thread)
                {
                    continue;
                }
                // A worktree known to the book but never mounted here shows as
                // such rather than borrowing another device's path.
                let (path, head) =
                    match state.get_local::<WorktreeState>(&worktree_state_record(&worktree.id))? {
                        Some(local) => (local.path, local.head_tree),
                        None => ("-".to_string(), "-".to_string()),
                    };
                println!(
                    "{}\t{}\t{}\t{}\t{}",
                    worktree.id, worktree.thread, path, head, worktree.name
                );
            }
            Ok(())
        }
        "show" => {
            ensure_no_options(args)?;
            ensure_arity(args, 1)?;
            let worktree = state.load_worktree(&args[0])?;
            println!("{}", serde_json::to_string_pretty(&worktree)?);
            Ok(())
        }
        "snapshot" => {
            ensure_no_options(args)?;
            if args.is_empty() || args.len() > 2 {
                bail!("usage: mush worktree snapshot <id> [path]");
            }
            let worktree = state.snapshot_worktree(&args[0], args.get(1).map(String::as_str))?;
            let local = state.load_worktree_state(&worktree.id)?;
            println!("{}\thead={}", worktree.id, local.head_tree);
            Ok(())
        }
        "checkout" => {
            ensure_no_options(args)?;
            ensure_arity(args, 2)?;
            let worktree = state.load_worktree(&args[0])?;
            let local = state.load_worktree_state(&worktree.id)?;
            let tree = state.load_tree(&local.head_tree)?;
            let count = state.checkout(&tree, &args[1], Some(&worktree))?;
            println!(
                "checked out {count} files from {} into {}",
                tree.id, args[1]
            );
            Ok(())
        }
        "diff" => {
            ensure_no_options(args)?;
            ensure_arity(args, 2)?;
            let left = state.load_worktree(&args[0])?;
            let right = state.load_worktree(&args[1])?;
            let left_head = state.load_worktree_state(&left.id)?.head_tree;
            let right_head = state.load_worktree_state(&right.id)?.head_tree;
            let left_tree = state.load_tree(&left_head)?;
            let right_tree = state.load_tree(&right_head)?;
            print_diff(&left_tree, &right_tree);
            Ok(())
        }
        "status" => {
            ensure_no_options(args)?;
            ensure_arity(args, 1)?;
            let status = state.status(&args[0])?;
            println!("{}", serde_json::to_string_pretty(&status)?);
            if status.ref_head.as_deref() != Some(status.local_head.as_str()) {
                // Not an error: the book may simply be ahead of this device.
                eprintln!(
                    "note: the book points at {} — this device has {}",
                    status.ref_head.as_deref().unwrap_or("nothing"),
                    status.local_head
                );
            }
            Ok(())
        }
        "pull" => {
            let from = take_option(args, "--from").ok_or_else(|| {
                anyhow!("usage: mush worktree pull <id> --from <host> --remote-root <path>")
            })?;
            let remote_root = take_option(args, "--remote-root").ok_or_else(|| {
                anyhow!("usage: mush worktree pull <id> --from <host> --remote-root <path>")
            })?;
            let remote_bin =
                take_option(args, "--remote-bin").unwrap_or_else(|| "fungos-vfs".to_string());
            ensure_no_options(args)?;
            ensure_arity(args, 1)?;
            // Fail before moving bytes if this device has never mounted the
            // worktree: there would be nothing to reconcile against.
            state.load_worktree_state(&args[0])?;
            let (outcomes, _transfer) =
                pull_over_ssh(&mut state.vfs, &from, &remote_root, &remote_bin, 500)?;
            let mut applied = 0;
            let mut duplicate = 0;
            let mut diverged = 0;
            for outcome in &outcomes {
                match outcome {
                    // A whole-state replacement landed, which is as much an
                    // applied transfer as a delta is.
                    ApplyOutcome::Applied | ApplyOutcome::Snapshot => applied += 1,
                    ApplyOutcome::Duplicate => duplicate += 1,
                    ApplyOutcome::Diverged => diverged += 1,
                }
            }
            println!(
                "{}",
                serde_json::json!({
                    "applied": applied,
                    "duplicate": duplicate,
                    "diverged": diverged,
                })
            );
            if diverged > 0 {
                // Fast-forward only in v0: a divergent op is never applied on
                // top of local work, it is surfaced.
                bail!("{diverged} op(s) diverged; this worktree needs reconciling");
            }
            Ok(())
        }
        "schema" => {
            ensure_no_options(args)?;
            ensure_arity(args, 0)?;
            print_schemas()
        }
        "help" | "--help" | "-h" => worktree_help(),
        other => bail!("unknown worktree command {other:?}"),
    }
}

fn run_tree(state: &mut State, args: &mut Vec<String>) -> Result<()> {
    let Some(subcommand) = args.first().cloned() else {
        return tree_help();
    };
    args.remove(0);
    match subcommand.as_str() {
        "snapshot" => {
            ensure_no_options(args)?;
            if args.len() > 1 {
                bail!("usage: mush tree snapshot [path]");
            }
            let path = args.first().cloned().unwrap_or_default();
            let tree = snapshot_tree(&state.vfs, &path)?;
            state.put(tree_record(&tree.id), TREE_SCHEMA, &tree)?;
            println!("{}\t{} files", tree.id, tree.entries.len());
            Ok(())
        }
        "show" => {
            ensure_no_options(args)?;
            ensure_arity(args, 1)?;
            let tree = state.load_tree(&args[0])?;
            println!("{}", serde_json::to_string_pretty(&tree)?);
            Ok(())
        }
        "list" => {
            ensure_no_options(args)?;
            ensure_arity(args, 0)?;
            let mut trees = state.scan::<Tree>("tree/")?;
            trees.sort_by(|a, b| a.id.cmp(&b.id));
            for tree in trees {
                println!(
                    "{}\t{} files\t{}",
                    tree.id,
                    tree.entries.len(),
                    tree.created_at_ms
                );
            }
            Ok(())
        }
        "diff" => {
            ensure_no_options(args)?;
            ensure_arity(args, 2)?;
            let left = state.load_tree(&args[0])?;
            let right = state.load_tree(&args[1])?;
            print_diff(&left, &right);
            Ok(())
        }
        "checkout" => {
            ensure_no_options(args)?;
            ensure_arity(args, 2)?;
            let tree = state.load_tree(&args[0])?;
            let count = state.checkout(&tree, &args[1], None)?;
            println!(
                "checked out {count} files from {} into {}",
                tree.id, args[1]
            );
            Ok(())
        }
        "schema" => {
            ensure_no_options(args)?;
            ensure_arity(args, 0)?;
            print_schemas()
        }
        "help" | "--help" | "-h" => tree_help(),
        other => bail!("unknown tree command {other:?}"),
    }
}

fn snapshot_tree(vfs: &Vfs, path: &str) -> Result<Tree> {
    let root = normalize_path(path)?;
    if !root.is_empty() && vfs.file(&root)?.is_some() {
        bail!("tree snapshot expects a directory, {root} is a file");
    }
    let prefix = if root.is_empty() {
        String::new()
    } else {
        format!("{root}/")
    };
    let mut entries = Vec::new();
    for entry in vfs.list(&root)? {
        let path = if root.is_empty() {
            entry.path.clone()
        } else {
            match entry.path.strip_prefix(&prefix) {
                Some(path) if !path.is_empty() => path.to_string(),
                _ => continue,
            }
        };
        entries.push(TreeEntry {
            path,
            content_id: entry.content_id.to_string(),
            size: entry.size,
            media_type: entry.media_type,
        });
    }
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    let id = ContentId::from_bytes(&serde_json::to_vec(&entries)?).to_string();
    Ok(Tree {
        id,
        entries,
        created_at_ms: now_ms(),
    })
}

fn print_diff(left: &Tree, right: &Tree) {
    for line in diff_lines(left, right) {
        println!("{line}");
    }
}

fn diff_lines(left: &Tree, right: &Tree) -> Vec<String> {
    let left = left
        .entries
        .iter()
        .map(|entry| (entry.path.as_str(), entry.content_id.as_str()))
        .collect::<BTreeMap<_, _>>();
    let right = right
        .entries
        .iter()
        .map(|entry| (entry.path.as_str(), entry.content_id.as_str()))
        .collect::<BTreeMap<_, _>>();
    let paths = left
        .keys()
        .chain(right.keys())
        .copied()
        .collect::<BTreeSet<_>>();
    let mut lines = Vec::new();
    for path in paths {
        match (left.get(path), right.get(path)) {
            (None, Some(_)) => lines.push(format!("+ {path}")),
            (Some(_), None) => lines.push(format!("- {path}")),
            (Some(left), Some(right)) if left != right => lines.push(format!("~ {path}")),
            _ => {}
        }
    }
    lines
}

fn print_schemas() -> Result<()> {
    let schemas = SCHEMAS
        .iter()
        .map(|(kind, schema)| (*kind, *schema))
        .collect::<BTreeMap<_, _>>();
    println!("{}", serde_json::to_string_pretty(&schemas)?);
    Ok(())
}

fn worktree_help() -> Result<()> {
    println!(
        "mush worktree ensure <name> --thread <thread> [--from PATH]\n\
         mush worktree create <path> --thread <thread> [--id ID] [--name NAME]\n\
         mush worktree list [--thread <thread>]\n\
         mush worktree show <id>\n\
         mush worktree snapshot <id> [path]\n\
         mush worktree checkout <id> <target>\n\
        mush worktree status <id>\n\
        mush worktree pull <id> --from <host> --remote-root <path>\n\
         mush worktree diff <a> <b>\n\
         mush worktree schema"
    );
    Ok(())
}

fn tree_help() -> Result<()> {
    println!(
        "mush tree snapshot [path]\n\
         mush tree show <id>\n\
         mush tree list\n\
         mush tree diff <a> <b>\n\
         mush tree checkout <id> <target>\n\
         mush tree schema"
    );
    Ok(())
}

fn thread_record(thread: &str) -> String {
    format!("thread/{thread}")
}

fn worktree_record(id: &str) -> String {
    format!("worktree/{id}")
}

/// Device-local companion to `worktree_record`, in the local collection.
fn worktree_state_record(id: &str) -> String {
    format!("worktree-state/{id}")
}

fn tree_record(id: &str) -> String {
    format!("tree/{id}")
}

fn ref_record(worktree: &str, name: &str) -> String {
    format!("ref/{worktree}/{name}")
}

fn checkout_record(id: &str) -> String {
    format!("checkout/{id}")
}

fn project_record(id: &str) -> String {
    format!("project/{id}")
}

fn workspace_record(session: &str, project: &str) -> String {
    format!("workspace/{session}/{project}")
}

/// A media type for a file the store is recording.
///
/// Only the handful the fleet actually reads: everything else is bytes, and
/// guessing a type from an unknown extension is how a renderer gets told to
/// draw a font as text.
fn media_type(path: &Path) -> &'static str {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("md") => "text/markdown",
        Some("rs") => "text/x-rust",
        Some("toml") => "text/x-toml",
        Some("json") => "application/json",
        Some("txt") => "text/plain",
        Some("html") => "text/html",
        Some("css") => "text/css",
        Some("js") => "text/javascript",
        Some("png") => "image/png",
        Some("svg") => "image/svg+xml",
        _ => "application/octet-stream",
    }
}

/// The branch a session worktree uses for one repository.
///
/// One branch per repo per project per session, so two sessions never share a
/// mutable ref. The name is sanitized because git refs forbid whitespace and
/// a handful of punctuation that session/project ids may contain.
fn project_branch(project: &str, session: &str, repo: &str) -> String {
    let raw = format!("soil/{project}/{session}/{repo}");
    let mut branch = String::with_capacity(raw.len());
    let mut last_slash = false;
    for ch in raw.chars() {
        let ch = if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '/') {
            ch
        } else {
            '-'
        };
        if ch == '/' {
            if last_slash {
                continue;
            }
            last_slash = true;
        } else {
            last_slash = false;
        }
        branch.push(ch);
    }
    while branch.contains("..") {
        branch = branch.replace("..", ".");
    }
    let branch = branch.trim_matches('/').trim_end_matches('.').to_string();
    branch.chars().take(240).collect()
}

/// Add a real git worktree for one project repo.
///
/// If the branch already exists (a retry, or another device pushed it), check
/// it out by name instead of trying to create it again. A source checkout that
/// is dirty is not carried over: the worktree starts at the committed base,
/// which is the only state git can reproduce.
fn add_git_worktree(source: &Path, target: &Path, branch: &str, base: &str) -> Result<()> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let reference = format!("refs/heads/{branch}");
    let exists = Command::new("git")
        .arg("-C")
        .arg(source)
        .args(["show-ref", "--verify", "--quiet", &reference])
        .status()
        .with_context(|| format!("run git show-ref in {}", source.display()))?
        .success();
    let mut command = Command::new("git");
    command.arg("-C").arg(source).args(["worktree", "add"]);
    if exists {
        command.arg(target).arg(branch);
    } else {
        command.args(["-b", branch]).arg(target).arg(base);
    }
    let output = command
        .output()
        .with_context(|| format!("run git worktree add in {}", source.display()))?;
    if !output.status.success() {
        bail!(
            "git worktree add {}: {}",
            target.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn run_git(path: &Path, args: &[&str]) -> Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .output()
        .with_context(|| format!("run git -C {} {:?}", path.display(), args))?;
    if !output.status.success() {
        bail!(
            "git -C {} {:?}: {}",
            path.display(),
            args,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn git_stdout(path: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .output()
        .with_context(|| format!("run git -C {} {:?}", path.display(), args))?;
    if !output.status.success() {
        bail!(
            "git -C {} {:?}: {}",
            path.display(),
            args,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn normalize_path(path: &str) -> Result<String> {
    if path.contains('\\') {
        bail!("invalid VFS path {path:?}");
    }
    let mut parts = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => bail!("VFS path cannot contain .."),
            part => parts.push(part),
        }
    }
    Ok(parts.join("/"))
}

fn validate_session_id(id: &str) -> Result<()> {
    validate_thread_id(id).map_err(|_| anyhow!("invalid session id {id:?}"))
}

fn validate_thread_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 128
        || id.contains("..")
        || id.contains('\\')
        || id.chars().any(char::is_whitespace)
    {
        bail!("invalid thread id {id:?}");
    }
    Ok(())
}

fn validate_worktree_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 64
        || !id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        bail!("invalid worktree id {id:?}");
    }
    Ok(())
}

fn take_option(args: &mut Vec<String>, name: &str) -> Option<String> {
    let index = args.iter().position(|arg| arg == name)?;
    if index + 1 >= args.len() {
        return None;
    }
    let value = args.remove(index + 1);
    args.remove(index);
    Some(value)
}

/// Session is the model spelling; `--thread` remains as a compatibility alias
/// for scripts and older harnesses that still emit it.
fn take_session(args: &mut Vec<String>) -> Option<String> {
    take_option(args, "--session").or_else(|| take_option(args, "--thread"))
}

fn ensure_no_options(args: &[String]) -> Result<()> {
    if let Some(option) = args.iter().find(|arg| arg.starts_with("--")) {
        bail!("unknown option {option}");
    }
    Ok(())
}

fn ensure_arity(args: &[String], expected: usize) -> Result<()> {
    if args.len() != expected {
        bail!("expected {expected} argument(s), got {}", args.len());
    }
    Ok(())
}

fn random_id(prefix: &str) -> String {
    format!("{prefix}_{:016x}", rand::rng().random::<u64>())
}

fn device_id() -> String {
    std::env::var("MUSH_DEVICE")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "local".into())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> (tempfile::TempDir, State) {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("store");
        let state = State::open(
            &dir.path().join("vfs"),
            &dir.path().join("records"),
            "mush-test",
            "worktree",
            Some(&store),
        )
        .unwrap();
        (dir, state)
    }

    fn git_repo(path: &Path) {
        std::fs::create_dir_all(path).unwrap();
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "test@example.com"],
            vec!["config", "user.name", "Test"],
        ] {
            let status = Command::new("git")
                .arg("-C")
                .arg(path)
                .args(&args)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?}");
        }
        std::fs::write(path.join("README.md"), "initial\n").unwrap();
        for args in [vec!["add", "."], vec!["commit", "-q", "-m", "initial"]] {
            let status = Command::new("git")
                .arg("-C")
                .arg(path)
                .args(&args)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?}");
        }
    }

    #[test]
    fn a_project_checkout_makes_one_git_worktree_per_repo() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("store");
        let mut state = State::open(
            &dir.path().join("vfs"),
            &dir.path().join("records"),
            "mush-test",
            "worktree",
            Some(&store),
        )
        .unwrap();
        let one = dir.path().join("src/one");
        let two = dir.path().join("src/two");
        git_repo(&one);
        git_repo(&two);

        let project = state
            .define_project(
                "acme",
                "suite",
                "Suite",
                vec![one.display().to_string(), two.display().to_string()],
            )
            .unwrap();
        assert_eq!(project.repos.len(), 2);

        let workspace = state.checkout_project("suite", "session-1").unwrap();
        assert_eq!(workspace.root, "sessions/session-1/suite");
        assert_eq!(workspace.repos.len(), 2);
        for repo in &workspace.repos {
            let path = state.root.join(&repo.path);
            assert!(path.join(".git").is_file(), "{}", path.display());
            assert_eq!(
                git_stdout(&path, &["rev-parse", "--abbrev-ref", "HEAD"])
                    .unwrap()
                    .trim(),
                repo.branch
            );
            assert!(state.load_worktree(&repo.worktree).unwrap().thread == "session-1");
        }

        // Re-running checkout is a resume, not a second set of worktrees.
        let again = state.checkout_project("suite", "session-1").unwrap();
        assert_eq!(
            again
                .repos
                .iter()
                .map(|repo| &repo.worktree)
                .collect::<Vec<_>>(),
            workspace
                .repos
                .iter()
                .map(|repo| &repo.worktree)
                .collect::<Vec<_>>()
        );

        // add-repo is durable and the next checkout materializes it.
        let three = dir.path().join("src/three");
        git_repo(&three);
        state
            .add_project_repo("suite", &three.display().to_string())
            .unwrap();
        let grown = state.checkout_project("suite", "session-1").unwrap();
        assert_eq!(grown.repos.len(), 3);
        assert!(state.root.join(&grown.repos[2].path).join(".git").is_file());

        std::fs::write(
            state.root.join(&workspace.repos[0].path).join("README.md"),
            "changed\n",
        )
        .unwrap();
        let outcomes = state
            .commit_project("suite", "session-1", "one change")
            .unwrap();
        assert_eq!(outcomes[0].status, "committed");
        assert_eq!(outcomes[1].status, "unchanged");
        assert_eq!(
            git_stdout(
                &state.root.join(&workspace.repos[0].path),
                &["status", "--porcelain"]
            )
            .unwrap()
            .trim(),
            ""
        );
    }

    /// A store written before the split keeps its worktree, and the device
    /// state it used to carry moves to the local book instead of being dropped.
    #[test]
    fn the_migration_splits_a_pre_split_worktree_record() {
        let dir = tempfile::tempdir().unwrap();
        let (vfs_root, records_root) = (dir.path().join("vfs"), dir.path().join("records"));
        let store = dir.path().join("store");
        {
            let mut state = State::open(
                &vfs_root,
                &records_root,
                "mush-test",
                "worktree",
                Some(&store),
            )
            .unwrap();
            // The shape written before the split: portable identity and device
            // state in one record.
            let legacy = serde_json::json!({
                "id": "wt_old",
                "name": "old",
                "thread": "thread-1",
                "path": "projects/old",
                "base_tree": "tree-base",
                "head_tree": "tree-head",
                "created_at_ms": 5,
                "updated_at_ms": 9,
            });
            state
                .put(worktree_record("wt_old"), WORKTREE_SCHEMA, &legacy)
                .unwrap();
        }

        // Reopening migrates.
        let mut state = State::open(
            &vfs_root,
            &records_root,
            "mush-test",
            "worktree",
            Some(&store),
        )
        .unwrap();
        let local = state.load_worktree_state("wt_old").unwrap();
        assert_eq!(local.path, "projects/old");
        assert_eq!(local.head_tree, "tree-head");
        assert_eq!(local.base_tree, "tree-base");

        let portable = state.load_worktree("wt_old").unwrap();
        assert_eq!(portable.id, "wt_old");
        assert_eq!(portable.thread, "thread-1");
        let body = serde_json::to_string(&portable).unwrap();
        assert!(!body.contains("projects/old"), "{body}");

        // Idempotent: a second open does not duplicate or lose anything.
        let mut again = State::open(
            &vfs_root,
            &records_root,
            "mush-test",
            "worktree",
            Some(&store),
        )
        .unwrap();
        assert_eq!(
            again
                .scan_local::<WorktreeState>("worktree-state/")
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn ensure_gives_a_session_its_own_copy_and_says_where_it_is() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("vfs");
        let store = dir.path().join("store");
        std::fs::create_dir_all(root.join("projects/demo/src")).unwrap();
        std::fs::write(root.join("projects/demo/src/main.rs"), b"fn main() {}").unwrap();
        let mut state = State::open(
            &root,
            &dir.path().join("records"),
            "mush-test",
            "worktree",
            Some(&store),
        )
        .unwrap();

        let from = root.join("projects/demo");
        let from = from.to_string_lossy().to_string();
        let first = state
            .ensure_worktree("lane", "thread-1", Some(&from))
            .unwrap();
        assert_eq!(first, root.join("worktrees/lane"));
        assert!(first.join("src/main.rs").is_file(), "the copy is on disk");
        assert!(state.load_worktree("lane").is_ok(), "the book knows it");

        // The agent edits its own tree; the project it came from is untouched.
        std::fs::write(first.join("src/main.rs"), b"fn main() { changed }").unwrap();
        assert_eq!(
            std::fs::read(root.join("projects/demo/src/main.rs")).unwrap(),
            b"fn main() {}"
        );

        // Idempotent: a restart or a second session takes the same tree, not a
        // second copy, so work in progress is not thrown away.
        let again = state
            .ensure_worktree("lane", "thread-1", Some(&from))
            .unwrap();
        assert_eq!(again, first);
        assert_eq!(
            std::fs::read(again.join("src/main.rs")).unwrap(),
            b"fn main() { changed }"
        );
    }

    #[test]
    fn a_snapshot_captures_a_file_the_agent_wrote_to_disk() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("vfs");
        let store = dir.path().join("store");
        std::fs::create_dir_all(root.join("projects/demo")).unwrap();
        std::fs::write(root.join("projects/demo/README.md"), b"hello").unwrap();
        let mut state = State::open(
            &root,
            &dir.path().join("records"),
            "mush-test",
            "worktree",
            Some(&store),
        )
        .unwrap();
        let from = root.join("projects/demo").to_string_lossy().to_string();
        let tree = state
            .ensure_worktree("lane", "thread-1", Some(&from))
            .unwrap();
        let before = state.load_worktree_state("lane").unwrap().head_tree;

        // What an agent does: write with open(2). No `vfs.put`, no index entry.
        std::fs::write(tree.join("notes.md"), b"reviewed by agent").unwrap();
        state.snapshot_worktree("lane", None).unwrap();

        let after = state.load_worktree_state("lane").unwrap().head_tree;
        assert_ne!(before, after, "the snapshot saw the agent's file");
        let recorded = state
            .vfs
            .file("worktrees/lane/notes.md")
            .unwrap()
            .expect("the file is in the tree");
        assert_eq!(recorded.size, b"reviewed by agent".len() as u64);
    }

    #[test]
    fn ensure_without_a_project_and_without_a_source_says_what_is_missing() {
        let (_dir, mut state) = state();
        let error = state
            .ensure_worktree("nope", "thread-1", None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("no project at"), "{error}");
    }

    #[test]
    fn snapshots_are_content_addressed_and_round_trip() {
        let (_dir, mut state) = state();
        state
            .vfs
            .put(
                "projects/demo/README.md",
                b"hello",
                "text/markdown",
                Expect::Any,
            )
            .unwrap();
        state
            .vfs
            .put(
                "projects/demo/src/main.rs",
                b"fn main() {}",
                "text/x-rust",
                Expect::Any,
            )
            .unwrap();

        let tree = snapshot_tree(&state.vfs, "projects/demo").unwrap();
        assert_eq!(tree.entries.len(), 2);
        assert!(tree.id.starts_with("blake3:"));
        assert_eq!(tree.entries[0].path, "README.md");
        assert_eq!(tree.entries[1].path, "src/main.rs");

        state
            .put(tree_record(&tree.id), TREE_SCHEMA, &tree)
            .unwrap();
        let loaded = state.load_tree(&tree.id).unwrap();
        assert_eq!(loaded.id, tree.id);
        assert_eq!(loaded.entries.len(), 2);
    }

    #[test]
    fn worktree_snapshot_diff_and_checkout() {
        let (_dir, mut state) = state();
        state
            .vfs
            .put(
                "projects/demo/README.md",
                b"hello",
                "text/markdown",
                Expect::Any,
            )
            .unwrap();
        state
            .vfs
            .put(
                "projects/demo/src/main.rs",
                b"fn main() {}",
                "text/x-rust",
                Expect::Any,
            )
            .unwrap();

        let worktree = state
            .create_worktree("projects/demo", "thread-1", Some("wt_test"), Some("demo"))
            .unwrap();
        assert_eq!(worktree.thread, "thread-1");
        assert!(state.get::<Thread>("thread/thread-1").unwrap().is_some());
        let base_head = state.load_worktree_state("wt_test").unwrap().head_tree;
        let base = state.load_tree(&base_head).unwrap();

        state
            .vfs
            .put(
                "projects/demo/src/main.rs",
                b"fn main() { changed }",
                "text/x-rust",
                Expect::Any,
            )
            .unwrap();
        state
            .vfs
            .put(
                "projects/demo/src/lib.rs",
                b"pub fn lib() {}",
                "text/x-rust",
                Expect::Any,
            )
            .unwrap();
        let updated = state.snapshot_worktree("wt_test", None).unwrap();
        assert_eq!(updated.id, "wt_test");
        let head_tree = state.load_worktree_state("wt_test").unwrap().head_tree;
        let head = state.load_tree(&head_tree).unwrap();
        let diff = diff_lines(&base, &head);
        assert!(diff.contains(&"~ src/main.rs".to_string()), "{diff:?}");
        assert!(diff.contains(&"+ src/lib.rs".to_string()), "{diff:?}");

        let count = state
            .checkout(&base, "checkouts/demo", Some(&worktree))
            .unwrap();
        assert_eq!(count, 2);
        assert_eq!(state.vfs.get("checkouts/demo/README.md").unwrap(), b"hello");
        assert_eq!(
            state.vfs.get("checkouts/demo/src/main.rs").unwrap(),
            b"fn main() {}"
        );
        assert_eq!(state.scan_local::<Checkout>("checkout/").unwrap().len(), 1);
        // The split: the synced book carries identity, this device carries the
        // path and the trees it materialised.
        assert!(state.scan::<Checkout>("checkout/").unwrap().is_empty());
        assert_eq!(
            state
                .scan::<WorktreeState>("worktree-state/")
                .unwrap()
                .len(),
            0
        );
        assert_eq!(
            state
                .scan_local::<WorktreeState>("worktree-state/")
                .unwrap()
                .len(),
            1
        );
        let portable = serde_json::to_string(&state.load_worktree("wt_test").unwrap()).unwrap();
        assert!(!portable.contains("path"), "{portable}");
        assert!(!portable.contains("head_tree"), "{portable}");
    }
}

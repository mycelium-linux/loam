# Global search

`soil:search@0.6.0` is a host-composed discovery surface over the sources a
host has already granted:

```text
files        -> soil:index / soil:vfs
records      -> soil:records books
Marbles      -> host-granted issue snapshot
principals   -> host-attested identity set
repositories -> explicit local Git roots
spores       -> explicit local Spore manifest roots
peers        -> explicit `mycelium peers --json` snapshot
services     -> explicit Mycelium/Unibus service catalog snapshot
applications -> host-local installed-application inventory
actions      -> reserved for a host action registry
```

Applications are the one source that is **host-local by construction**. The
inventory is whatever is installed on this machine, it differs per host, and it
is cheap to rebuild from the bundle index — so it is never synced, never enters
records, and never gossips. A hit carries the bundle identifier as `id`, the
display name as `title`, and an icon as an asset reference the renderer resolves;
the Spore still cannot see a path.

It is not authority. A hit is a locator plus display metadata. The caller still
fetches the file through `soil:vfs` or the entry through `soil:records`.

## Surface

```wit
search(query)   -> page
state()         -> search-state
refresh()       -> search-state
```

```wit
query {
  text,
  kinds,       // file / record-entry / repository / spore / peer / service /
              // action / task / principal
  mode,        // substring / fuzzy / full-text / semantic
  offset,
  limit,
}
```

```wit
hit {
  kind,
  id,          // path, book/entity, or action id
  title,
  subtitle,
  snippet,
  score,
  source,
  updated-at-ms,
  references, // bounded graph references attached to this hit
}
```


## Typed mentions

`soil:search/mentions` is the `@` autocomplete resolver. It returns typed
candidates with a stable locator token:

```wit
suggest(text, kinds, limit) -> list<candidate>
```

```wit
candidate {
  kind,        // person / account / company / project / task / opportunity /
              // activity / call / thread / message / document / sheet / slide /
              // file / principal
  token,       // record:<book>#<entity> or file:<path>
  label,
  subtitle,
  schema,      // record schema when the candidate came from records
  format,      // md / pdf / docx / xlsx / pptx / ...
  score,
  visibility,  // team / private / workspace
}
```

Records are mapped by schema:

```text
fungos.crm/account@1       -> account
fungos.crm/company@1       -> company
fungos.crm/contact@1       -> person
fungos.crm/project@1       -> project
fungos.crm/opportunity@1   -> opportunity
fungos.crm/activity@1      -> activity
fungos.crm/call@1          -> call
fungos.mail/thread@1       -> thread
fungos.mail/message@1      -> message
fungos.doc/document        -> document
fungos.doc/document@1      -> document
fungos.docs/document@1     -> document
fungos.note/note@1         -> document
fungos.notes/note@1        -> document
fungos.sheet/sheet@1       -> sheet
fungos.sheets/sheet@1      -> sheet
fungos.slide/slide@1       -> slide
fungos.slides/slide@1      -> slide
```

Marbles issues are not records. The host adapter supplies them as `task`
candidates with a `marbles://<store>/<company>/<project>/<issue>` canonical ref.
Principals are supplied by the host as `principal:<actor>` candidates; they are
not records either.

Unknown schemas are not mentionable. Privacy follows the books the host opened:
a team book yields projections, a private book yields private records.

VFS entries are classified by extension:

```text
document   md markdown mdx txt rst adoc org tex doc docx odt rtf epub html
document   pdf                 (format = pdf)
sheet      xls xlsx xlsm ods csv tsv numbers
slide      ppt pptx pptm odp key
file       everything else
```

All VFS mentions use `visibility = workspace`. `format` carries the extension so
a UI can choose the right icon without parsing the path.

The inspection server can load host-granted sources:

```sh
soil-search-web \
  --marbles-snapshot marbles.json \
  --marbles-store store --marbles-company fpl \
  --principals-snapshot principals.json
```

The autocomplete component is [Mention input](mention-input.md).

Insert `token` into the document/task/record and persist a graph link:

```text
fungos.graph/link@1
  rel: mentions
  source: record:<book>#<document-or-task>
  target: record:<book>#c-ada
```

The label is resolved later. Never copy the display name into the mention.

See [Mentions](mentions.md) for persistence and backlinks, and
[Mention input](mention-input.md) for the themable autocomplete component.

The inspection server exposes the same resolver:

```text
GET /api/suggest?q=ada&kinds=person,account&limit=10
```

## Modes, and what "search" means

`soil:search` is retrieval, not an answer: every hit is a locator plus a
snippet. There is no generation step. A RAG-style surface would take these hits,
fetch the chunks through `soil:vfs`/`soil:records`, and ask `soil:inference` —
that is a separate composition, not something search does.

| Mode | Files | Records | Backend |
|---|---|---|---|
| `substring` | yes | yes | linear scan over indexed text / `search_books` |
| `fuzzy` | yes | no | path and text scoring |
| `full-text` | yes | no | FTS5 `chunks_fts`, ranked by `bm25` |
| `regex` | yes | no | in-process `regex` scan; `tgrep` when the host granted a binary |
| `semantic` | yes | no | cosine over chunk vectors; needs an embedder |

Semantic search is opt-in. `VfsHost::with_embeddings` wires a provider, the
indexer embeds missing chunks in batches of 64, and `knn_search` returns the
nearest chunk paths. The runner does **not** grant an embedder yet, so
`--search` in `soil-webgpu` is substring/fuzzy/full-text/regex; a semantic query
answers `unsupported`. Records are searched through `fungos-records`'
`search_books`; they are not embedded.

`tgrep` is Microsoft's trigram-index grep, not an in-process trigram index. The
host shells out to an explicit binary (`--json`, host-owned root), parses
ripgrep-shaped matches and caps the page. It is implemented and tested, but no
production host wires it yet: `with_tgrep` appears only in tests, and neither
the inspection binary nor the runner exposes a flag for it.

## Caching

The VFS index is the cache. `VfsHost::open` opens
`<root>/.fungos/soil-index.sqlite3` and calls `refresh` once; refresh hashes the
entry list and is a no-op when the tree has not moved. When it has moved, the
rebuild is **whole-index**: entries, extracted text, chunk rows, the FTS5 table
and every vector are deleted and rewritten. There is no per-file delta and no
per-chunk vector reuse, so one edit invalidates the embeddings too, and a
semantic host re-embeds the workspace in 64-chunk batches.

During a session the index does not refresh per query; a Spore calls
`soil:search.refresh` (or `soil:index.refresh`) after writes. The host's other
sources (git, spores, applications, peers, services) are in-memory snapshots
reloaded on `refresh` or when their file mtime moves; records are read from the
book on every query. The MRU cache is a small JSON file (`--mru FILE` in the
inspection binary; in-memory in the runner) used only to tilt ranking.

There is no query-result cache. A full-text query costs one FTS5 `MATCH`;
substring, fuzzy and regex scan every indexed node; semantic is brute-force
cosine over every vector for the model. `Page.generation` names the index that
produced the page.

## Tools, not generation

The consumer this is built for is a tool loop, not a completion inside search:
search returns locators, fetch returns bytes, and the agent decides what to read
next. That is why `soil:search` has no generation step and why a hit is a path
plus a snippet rather than a document.

`soil:vfs/read` is the fetch. A hit does not carry a chunk id or a byte range
today, so a tool that wants "the matched chunk" either reads the whole file or
guesses a range from the snippet. If agent search should be chunk-accurate, the
next ABI change is a chunk locator on the hit; the index already stores chunk ids
(`content_id:ordinal`) and ordinals, so the data is there.

The agent shell does not see this surface yet: Mush links `soil:index` and
implements `index::Host` over its own VFS host, while the global `soil:search`
is currently granted only to Spores in the WebGPU runner. Giving agents the
global search is one import and one host impl in Mush (plus a `search` builtin),
not a second search engine.

## Host behavior

`crates/soil/src/search_host` currently composes:

- `VfsHost` search over the derived SQLite/FTS5 index
- `fungos-records` substring search over an explicit allowlist of books
- bounded local Git discovery under host-granted roots
- local Spore package manifests under host-granted roots
- a host-granted `mycelium peers --json` snapshot
- a host-granted Mycelium/Unibus service catalog snapshot
- optional Microsoft `tgrep` regex search over an explicit root or VFS mount
- graph links and references from `fungos.graph/link@1` and
  `fungos.graph/reference@1` in the approved records books

Results are merged by score, then title, and paginated. `refresh` rebuilds the
VFS index and advances the search generation.

The host supplies the filesystem root, approved books, Git roots, Spore roots,
and discovery snapshots. A Spore does not choose a path, book, provider, or
source. Discovery is opt-in at the host:

```rust
let search = SearchService::open(vfs_root, records_root, books)?
    .with_spore_roots(vec![spore_root])
    .with_mycelium_snapshot_dir(snapshot_dir)?;
```

`snapshot_dir` contains `peers.json` from `mycelium peers --json` and
`services.json` from `mycelium services --json` or an equivalent service
catalog. Search never talks to the Mycelium daemon, opens a network connection,
or mutates peer/service state. A hit only names a local package id, peer origin,
or service id.

Semantic file search is enabled only when the host attaches an embedding provider:

```rust
let search = SearchService::open(vfs_root, records_root, books)?
    .with_embeddings(Arc::new(Model2VecEmbedder::potion_base_8m()?))?;
``` Requesting `action` without an
action source returns no hits rather than inventing one.

## Ranking

- VFS hits retain their derived-index score.
- Exact path promotion happens inside `soil:index`.
- Record hits are newest-first and receive a stable host-side score.
- Semantic mode is available only when the VFS host has an embedding provider;
  otherwise the search returns `unsupported`.

## Boundaries

- Search never grants access. It can only search sources the host opened.
- Search is local and never syncs an index.
- Record text is read from the materialized `fungos-records` view.
- Graph references are derived from approved books only. A reference is a
  locator, not an access grant; see [graph.md](graph.md).
- Git search reads repository name, relative id, branch, and a credential-
  stripped origin URL. It never runs Git or reads working-tree bytes.
- Spore search reads manifests only, not module bytes or asset contents.
- Peer/service search reads host-supplied snapshots; no daemon RPC, network,
  raw socket, or ambient topology scan.
- App/action results require a future explicit action source; no ambient
  process scan or filesystem crawl.

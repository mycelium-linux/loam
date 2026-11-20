//! Canonical Soil host capability contracts.
//!
//! Each package is a WIT contract under `wit/` plus a conformance test. This
//! crate embeds the contract sources so a package rename, interface rename, or
//! dropped function fails `cargo test` rather than silently changing the ABI.

/// A canonical capability package and the interface functions it must expose.
#[derive(Debug)]
pub struct Contract {
    /// Repository-relative path, used in assertions and failure messages.
    pub file: &'static str,
    /// Exact WIT source; embedded so tests do not depend on the cwd.
    pub source: &'static str,
    /// Fully qualified package name, e.g. `soil:time@1.0.0`.
    pub package: &'static str,
    /// Interface name plus the free functions that must exist on it.
    pub interfaces: &'static [(&'static str, &'static [&'static str])],
}

/// Every canonical package under `wit/`, including contract-only and deferred
/// surfaces. See `wit/README.md` for status.
pub const CONTRACTS: &[Contract] = &[
    Contract {
        file: "wit/soil-input.wit",
        source: include_str!("../../../wit/soil-input.wit"),
        package: "soil:input@1.0.0",
        interfaces: &[("types", &[])],
    },
    Contract {
        file: "wit/soil-principal.wit",
        source: include_str!("../../../wit/soil-principal.wit"),
        package: "soil:principal@0.1.0",
        interfaces: &[("principal", &["current"])],
    },
    Contract {
        file: "wit/soil-view.wit",
        source: include_str!("../../../wit/soil-view.wit"),
        package: "soil:view@1.0.0",
        interfaces: &[("viewer", &["open"])],
    },
    Contract {
        file: "wit/soil-git.wit",
        source: include_str!("../../../wit/soil-git.wit"),
        package: "soil:git@0.1.0",
        interfaces: &[("git", &["status"])],
    },
    Contract {
        file: "wit/soil-desktops.wit",
        source: include_str!("../../../wit/soil-desktops.wit"),
        package: "soil:desktops@0.1.0",
        interfaces: &[("desktops", &["available", "open"])],
    },
    Contract {
        file: "wit/soil-time.wit",
        source: include_str!("../../../wit/soil-time.wit"),
        package: "soil:time@1.1.0",
        interfaces: &[("time", &["monotonic-ms", "wall-clock", "tick"])],
    },
    Contract {
        file: "wit/soil-storage.wit",
        source: include_str!("../../../wit/soil-storage.wit"),
        package: "soil:storage@1.0.0",
        interfaces: &[("storage", &["read", "write"])],
    },
    Contract {
        file: "wit/soil-stream.wit",
        source: include_str!("../../../wit/soil-stream.wit"),
        package: "soil:streams@0.1.0",
        interfaces: &[("duplex", &["accept"])],
    },
    Contract {
        file: "wit/soil-vfs.wit",
        source: include_str!("../../../wit/soil-vfs.wit"),
        package: "soil:vfs@1.0.0",
        interfaces: &[("vfs", &["ls", "stat", "read"])],
    },
    Contract {
        file: "wit/soil-vfs-mut.wit",
        source: include_str!("../../../wit/soil-vfs-mut.wit"),
        package: "soil:vfs-mut@0.1.0",
        interfaces: &[("vfs-mut", &["begin-write", "write", "remove", "mkdir", "rmdir", "rename"])],
    },
    Contract {
        file: "wit/soil-index.wit",
        source: include_str!("../../../wit/soil-index.wit"),
        package: "soil:index@1.1.0",
        interfaces: &[("index", &["browse", "search", "knn", "state", "refresh"])],
    },
    Contract {
        file: "wit/soil-records.wit",
        source: include_str!("../../../wit/soil-records.wit"),
        package: "soil:records@0.1.0",
        interfaces: &[(
            "records",
            &["get", "scan", "upsert", "delete", "changes", "apply"],
        )],
    },
    Contract {
        file: "wit/soil-surface-import.wit",
        source: include_str!("../../../wit/soil-surface-import.wit"),
        package: "soil:surface-import@0.1.0",
        interfaces: &[("surface", &["open"])],
    },
    Contract {
        file: "wit/soil-media.wit",
        source: include_str!("../../../wit/soil-media.wit"),
        package: "soil:media@0.1.0",
        interfaces: &[
            ("playback", &["open"]),
            ("encoded", &["open"]),
            ("image", &["open"]),
        ],
    },
    Contract {
        file: "wit/soil-inference.wit",
        source: include_str!("../../../wit/soil-inference.wit"),
        package: "soil:inference@0.3.0",
        interfaces: &[(
            "inference",
            &[
                "models",
                "complete",
                "embed",
                "voice-embed",
                "transcribe",
                "open-stream",
                "state",
            ],
        )],
    },
    Contract {
        file: "wit/soil-codec.wit",
        source: include_str!("../../../wit/soil-codec.wit"),
        package: "soil:codec@0.1.0",
        interfaces: &[("codec", &["decode", "encode", "reset"])],
    },
    Contract {
        file: "wit/soil-spore.wit",
        source: include_str!("../../../wit/soil-spore.wit"),
        package: "soil:spore@0.3.0",
        interfaces: &[(
            "catalog",
            &["entries", "launch", "running", "stop", "install"],
        )],
    },
    Contract {
        file: "wit/soil-search.wit",
        source: include_str!("../../../wit/soil-search.wit"),
        package: "soil:search@0.6.0",
        interfaces: &[
            ("search", &["search", "state", "refresh"]),
            ("mentions", &["suggest"]),
        ],
    },
    Contract {
        file: "wit/soil-http.wit",
        source: include_str!("../../../wit/soil-http.wit"),
        package: "soil:http@0.1.0",
        interfaces: &[("http", &["fetch"])],
    },
    Contract {
        file: "wit/soil-creds.wit",
        source: include_str!("../../../wit/soil-creds.wit"),
        package: "soil:creds@1.0.0",
        interfaces: &[("creds", &["resolve", "sign", "verify"])],
    },
    Contract {
        file: "wit/fungos-http-context.wit",
        source: include_str!("../../../wit/fungos-http-context.wit"),
        package: "fungos:http-context@0.1.0",
        interfaces: &[("context", &["current"])],
    },
    Contract {
        file: "wit/soil-unibus.wit",
        source: include_str!("../../../wit/soil-unibus.wit"),
        package: "soil:unibus@0.1.0",
        interfaces: &[("bus", &["poll", "publish"])],
    },
    Contract {
        file: "wit/soil-graph.wit",
        source: include_str!("../../../wit/soil-graph.wit"),
        package: "soil:graph@0.1.0",
        interfaces: &[("graph", &["neighbors", "references", "state", "refresh"])],
    },
    Contract {
        file: "wit/soil-appdata.wit",
        source: include_str!("../../../wit/soil-appdata.wit"),
        package: "soil:appdata@1.0.0",
        interfaces: &[("appdata", &["open"])],
    },
    Contract {
        file: "wit/fungos-mycelium.wit",
        source: include_str!("../../../wit/fungos-mycelium.wit"),
        package: "fungos:mycelium@0.1.0",
        interfaces: &[("pulse", &["all", "put", "remove", "runs"])],
    },
    Contract {
        file: "wit/soil-mycelium.wit",
        source: include_str!("../../../wit/soil-mycelium.wit"),
        package: "soil:mycelium@0.1.0",
        interfaces: &[
            ("directory", &["targets", "get"]),
            (
                "session",
                &[
                    "open-shell",
                    "read",
                    "write",
                    "resize",
                    "status",
                    "close",
                    "exec",
                ],
            ),
        ],
    },
    Contract {
        file: "wit/soil-terminal.wit",
        source: include_str!("../../../wit/soil-terminal.wit"),
        package: "soil:terminal@0.1.0",
        interfaces: &[(
            "surface",
            &["open", "present", "next-input", "observe", "close"],
        )],
    },
    Contract {
        file: "wit/soil-canvas.wit",
        source: include_str!("../../../wit/soil-canvas.wit"),
        package: "soil:canvas@1.0.0",
        interfaces: &[("surface", &["present", "observe", "input", "close"])],
    },
    Contract {
        file: "wit/soil-scene.wit",
        source: include_str!("../../../wit/soil-scene.wit"),
        package: "soil:scene@1.0.0",
        interfaces: &[("payload", &["supports", "budget"])],
    },
    Contract {
        file: "wit/wasi-audio.wit",
        source: include_str!("../../../wit/wasi-audio.wit"),
        package: "wasi:audio@0.1.0",
        interfaces: &[("playback", &["open"])],
    },
    Contract {
        file: "wit/soil-ble.wit",
        source: include_str!("../../../wit/soil-ble.wit"),
        package: "soil:ble@1.2.0",
        interfaces: &[(
            "ble",
            &[
                "max-links",
                "scan",
                "connect",
                "disconnect",
                "claim",
                "services",
                "read",
                "write",
                "subscribe",
                "next-event",
            ],
        )],
    },
    Contract {
        file: "wit/soil-notify.wit",
        source: include_str!("../../../wit/soil-notify.wit"),
        package: "soil:notify@1.1.0",
        interfaces: &[
            ("publisher", &["publish", "update", "withdraw"]),
            ("inbox", &["query", "subscribe", "dismiss", "act"]),
            ("focus", &["get", "set", "clear"]),
            ("history", &["query"]),
            ("cleanup", &["clear"]),
            (
                "ringer",
                &["incoming", "accept", "decline", "end", "mute", "get-state"],
            ),
        ],
    },
    Contract {
        file: "wit/fungos-actions.wit",
        source: include_str!("../../../wit/fungos-actions.wit"),
        package: "fungos:actions@1.0.0",
        interfaces: &[
            ("actions", &["catalog", "invoke"]),
            ("catalog", &["list-actions"]),
        ],
    },
    Contract {
        file: "wit/soil-agents.wit",
        source: include_str!("../../../wit/soil-agents.wit"),
        package: "soil:agents@0.1.0",
        interfaces: &[
            (
                "observe",
                &[
                    "agents",
                    "get",
                    "intent",
                    "current-scope",
                    "transcript",
                    "subscribe",
                ],
            ),
            ("control", &["respond", "send", "interrupt"]),
        ],
    },
    Contract {
        file: "wit/soil-device.wit",
        source: include_str!("../../../wit/soil-device.wit"),
        package: "soil:device@0.1.0",
        interfaces: &[
            ("battery", &["read", "update-status"]),
            ("estimate", &["read", "update-status"]),
            ("lid", &["read", "log-change"]),
        ],
    },
    Contract {
        file: "wit/soil-metrics.wit",
        source: include_str!("../../../wit/soil-metrics.wit"),
        package: "soil:metrics@0.1.0",
        interfaces: &[("metrics", &["emit", "snapshot"])],
    },
    Contract {
        file: "wit/soil-net.wit",
        source: include_str!("../../../wit/soil-net.wit"),
        package: "soil:net@0.1.0",
        interfaces: &[("net", &["resolve", "probe"])],
    },
    Contract {
        file: "wit/soil-present.wit",
        source: include_str!("../../../wit/soil-present.wit"),
        package: "soil:present@0.1.0",
        interfaces: &[
            ("types", &[]),
            (
                "surface",
                &["size", "scale", "delivery", "present", "present-rgba"],
            ),
            ("input", &["interactive", "poll"]),
            ("cursor", &["set"]),
            ("clipboard", &["set-text"]),
        ],
    },
    Contract {
        file: "wit/soil-terminal-screen.wit",
        source: include_str!("../../../wit/soil-terminal-screen.wit"),
        package: "soil:terminal-screen@0.1.0",
        interfaces: &[(
            "screen",
            &[
                "open-session",
                "sessions",
                "select-session",
                "close-session",
                "snapshot",
                "wait",
                "write",
                "scroll",
                "resize",
            ],
        )],
    },
    Contract {
        file: "wit/soil-vfs-cid.wit",
        source: include_str!("../../../wit/soil-vfs-cid.wit"),
        package: "soil:vfs-cid@0.1.0",
        interfaces: &[("cid", &["find"])],
    },
    Contract {
        file: "wit/soil-worktrees.wit",
        source: include_str!("../../../wit/soil-worktrees.wit"),
        package: "soil:worktrees@0.1.0",
        interfaces: &[("worktrees", &["checkouts"])],
    },
    Contract {
        file: "wit/fungos-webhooks.wit",
        source: include_str!("../../../wit/fungos-webhooks.wit"),
        package: "fungos:webhooks@0.1.0",
        interfaces: &[("admin", &["create", "revoke", "hooks"])],
    },
    Contract {
        file: "wit/terminal-spore.wit",
        source: include_str!("../../../wit/terminal-spore.wit"),
        package: "fungos:terminal-spore@0.1.0",
        interfaces: &[],
    },
];

/// External WIT packages imported by canonical contracts. These are upstream,
/// not Soil-owned, and are vendored under `wit/deps/` so the canonical tree
/// resolves as one graph.
pub const DEPENDENCIES: &[Contract] = &[Contract {
    file: "wit/deps/wasi-webgpu-0.3.0-rc.2/package.wit",
    source: include_str!("../../../wit/deps/wasi-webgpu-0.3.0-rc.2/package.wit"),
    package: "wasi:webgpu@0.3.0-rc.2",
    interfaces: &[("webgpu", &[])],
}];

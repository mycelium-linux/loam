//! The `wasi:webgpu` Spore runner: one store, one Spore, one window.
//!
//! A GPUI Spore is a `wasm32-wasip2` component that renders into a texture it
//! hands over through `soil:present`. This module is the host half: a wasmtime
//! engine, a linker that grants exactly the capabilities the presenter fills
//! in, and the presenter seam a window blits through.
//!
//! It lived in its own crate (`soil-webgpu`) while the host was being split
//! into one crate per contract. That split is gone, and with it the reason for
//! a second crate: a runner is a host, so it is a feature of [`crate`].
//!
//! * [`WebGpuRuntime`] owns the engine, the linker and the component cache;
//! * [`HostSetup`] is what one run is allowed to reach — a presenter, input,
//!   and the grants a presenter filled in;
//! * [`CanvasGpuPresenter`] is how a completed frame leaves, and
//!   [`InputEvent`] is how normalized input arrives;
//! * [`fb`] presents to a panel framebuffer instead of a window.
//!
//! Nothing native crosses into the guest: adapter, device, queue and mapped
//! memory stay behind `wgpu-core`, and a Spore that imports a capability the
//! presenter did not supply fails to link or answers `denied`.

use wasmtime::component::bindgen;

bindgen!({
    path: "wit/webgpu",
    world: "runner",
    exports: {
        "run": async,
    },
    with: {
        "wasi:webgpu/webgpu@0.3.0-rc.2.gpu-texture": wasi_webgpu_wasmtime::Texture,
        // One binding per capability: the host crate's own `soil:*` types,
        // not a second generated copy that would need converting field by
        // field. The linker for each interface is the same module's
        // `add_to_linker`, so the host traits are implemented once.
        "soil:vfs/vfs@1.0.0": crate::vfs_host::vfs,
        "soil:principal/principal@0.1.0": crate::principal_host::api,
        "soil:http/http@0.1.0": crate::http_host::api,
        "soil:records/records@0.1.0": crate::records_host::records,
        "soil:unibus/bus@0.1.0": crate::unibus::bus,
        "soil:search/search@0.6.0": crate::search_host::api,
        "soil:search/mentions@0.6.0": crate::search_host::mention_api,
        // A subscription is host state, not a generated shadow type: the
        // backend keeps the position, so two subscriptions cannot disagree.
        "soil:agents/observe.subscription": AgentSubscription,
    },
    require_store_data_send: true,
});

mod agents;
mod fb;
mod host;
mod present;
mod runtime;

pub use agents::{agent_backend, session_creator};
pub use fb::FramebufferPresenter;
pub use host::{
    CatalogEntry, CatalogError, CatalogHost, CatalogLaunched, CatalogSurface, CatalogSurfaceKind,
    HostSetup, HttpAccess, InstallCatalog, LOCAL_STORE_ORIGIN, MARK_LIMIT, NoTerminal,
    RecordsAccess, SearchAccess, TerminalCell, TerminalHost, TerminalSession, TerminalSnapshot,
    WebGpuHost, in_process_bus, read_mark,
};
pub use present::{
    CanvasFrame, CanvasGpuPresenter, ClipboardHost, CursorHost, CursorStyle, DiscardPresenter,
    FrameCapture, InputEvent, InputKey, PointerButton,
};
pub use runtime::{
    Grants, WebGpuRuntime, run_component_blocking, run_component_capture,
    run_component_capture_with, run_component_capture_with_grants,
    run_component_capture_with_records,
};

/// The denying agent backend, for presenters that own no agent session.
pub use soil_agent_host::NoAgents;

/// One per `observe.subscribe`.
///
/// The resource carries no position of its own: `AgentBackend` keeps the
/// cursor, so a Spore that drops its handle cannot leave a half-read queue
/// behind. The handle exists so a dropped subscription is `stale`, not an
/// empty poll.
pub struct AgentSubscription;

#[cfg(test)]
mod tests {
    const WEBGPU_WIT: &str =
        include_str!("../../wit/webgpu/deps/wasi-webgpu-0.3.0-rc.2/package.wit");
    const RUNNER_WIT: &str = include_str!("../../wit/webgpu/runner.wit");

    #[test]
    fn pins_the_upstream_wasi_webgpu_contract() {
        assert!(WEBGPU_WIT.starts_with("package wasi:webgpu@0.3.0-rc.2;"));
        assert!(RUNNER_WIT.contains("include wasi:webgpu/imports@0.3.0-rc.2;"));
        assert!(RUNNER_WIT.contains("export run: async func() -> u32;"));
    }

    #[test]
    fn the_runner_offers_agents_but_not_their_authority() {
        // The point of the surface is that it can watch and answer. Spawning a
        // harness, choosing a model, or reading a credential is not here, and
        // dropping this assertion is how one of them would quietly arrive.
        assert!(RUNNER_WIT.contains("import soil:agents/observe@0.1.0;"));
        assert!(RUNNER_WIT.contains("import soil:agents/control@0.1.0;"));
        for forbidden in ["spawn", "config", "credential", "secrets"] {
            assert!(
                !RUNNER_WIT.contains(forbidden),
                "the agent surface grew `{forbidden}` authority"
            );
        }
    }
}

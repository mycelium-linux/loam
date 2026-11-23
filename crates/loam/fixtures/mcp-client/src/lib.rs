//! A command that exercises the first real plugin: `soil:mcp`.
//!
//! It is intentionally tiny. If the plugin linker resolved the import and the
//! sidecar answered, this prints the loopback catalog and exits zero.

wit_bindgen::generate!({
    path: "wit",
    world: "guest",
    generate_all,
});

use soil::mcp::mcp;

struct Component;

impl exports::wasi::cli::run::Guest for Component {
    fn run() -> Result<(), ()> {
        match mcp::servers() {
            Ok(servers) => {
                println!("servers: {}", servers.len());
                for server in servers {
                    println!("server: {} {}", server.id, server.title);
                    match mcp::tools(&server.id) {
                        Ok(tools) => {
                            println!("tools: {}", tools.len());
                            for tool in tools {
                                println!("tool: {}", tool.name);
                            }
                        }
                        Err(error) => {
                            eprintln!("tools error: {error:?}");
                            return Err(());
                        }
                    }
                }
            }
            Err(error) => {
                eprintln!("servers error: {error:?}");
                return Err(());
            }
        }
        Ok(())
    }
}

export!(Component);

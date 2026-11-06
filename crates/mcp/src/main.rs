//! RojoManagerMCP: lets agents run Rojo servers, open places, and connect Rojo in Studio. Speaks MCP
//! over stdio (newline-delimited JSON-RPC).

mod backend;
#[cfg(test)]
mod fake;
mod rpc;
mod tools;

fn main() {
    rpc::serve(std::io::stdin().lock(), std::io::stdout(), &backend::Core);
}

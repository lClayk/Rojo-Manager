//! RojoManagerMCP: lets agents run Rojo servers, open places, and connect Rojo in Studio. Speaks MCP
//! over stdio (newline-delimited JSON-RPC).
//!
//! With arguments it runs one tool and prints its result instead: `RojoManagerMCP setup install=bridge`.
//! An agent can then set Rojo Manager up before its MCP client has loaded the server.

mod backend;
#[cfg(test)]
mod fake;
mod rpc;
mod tools;

use backend::Backend as _;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some((tool, pairs)) = args.split_first() else {
        return rpc::serve(std::io::stdin().lock(), std::io::stdout(), &backend::Core);
    };
    let result =
        tools::cli_args(backend::Core.settings(), tool, pairs).and_then(|args| tools::run(&backend::Core, tool, &args));
    match result {
        Ok(text) => println!("{text}"),
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(1);
        }
    }
}

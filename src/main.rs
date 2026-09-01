use std::{error::Error, fs};

use anyhow::Result;
use clap::Parser;
use lsp_server::{Connection, Message, ProtocolError};
use lsp_types::{
    CancelParams, DidChangeTextDocumentParams, DidCloseTextDocumentParams,
    DidOpenTextDocumentParams, DidSaveTextDocumentParams, InitializeParams,
};
use structured_logger::Builder;

mod command;
mod encoding;

#[derive(Parser, Debug)]
#[clap(name = "lspml")]
struct CommandLineOpts {
    /// Without one the language server is started on stdio
    #[clap(subcommand)]
    command: Option<Command>,
    #[clap(long)]
    log_file: Option<String>,
    #[clap(long, default_value = "INFO")]
    log_level: String,
    #[clap(long)]
    modules_file: Option<String>,
}

#[derive(clap::Subcommand, Debug)]
enum Command {
    /// Print an artifact generated from the tag grammar to stdout
    Dump {
        #[clap(value_enum)]
        artifact: Artifact,
    },
}

#[derive(Clone, Debug, clap::ValueEnum)]
enum Artifact {
    /// Custom data for vscode's html language server
    HtmlCustomData,
}

fn main() -> Result<(), Box<dyn Error + Sync + Send>> {
    let opts = CommandLineOpts::parse();

    // Dumping is a one shot write to stdout, which the language server owns for
    // the rest of this function.
    if let Some(Command::Dump { artifact }) = &opts.command {
        return dump(artifact);
    }

    Builder::with_level(&opts.log_level)
        .with_target_writer(
            "*",
            opts.log_file
                .clone()
                .and_then(|file| {
                    fs::File::options()
                        .create(true)
                        .append(true)
                        .open(file)
                        .ok()
                })
                .map(|file| structured_logger::json::new_writer(file))
                .unwrap_or_else(|| structured_logger::json::new_writer(std::io::stderr())),
        )
        .init();
    log::info!("lspml starting...");
    log::debug!("commandline opts: {:?}", opts);
    match opts.modules_file {
        Some(file) => modules::init_module_mappings_from_file(&file),
        None => modules::init_empty_module_mappings(),
    }?;

    let (connection, io_threads) = Connection::stdio();
    // Split instead of `Connection::initialize`: the capabilities we answer
    // with depend on the ones the client sent, which that convenience method
    // only hands back once the answer is already on the wire.
    let initialization_params = match initialize(&connection) {
        Ok(params) => params,
        Err(err) => {
            if let Some(err) = err.downcast_ref::<ProtocolError>()
                && err.channel_is_disconnected()
            {
                io_threads.join()?;
            }
            return Err(err.into());
        }
    };

    main_loop(connection, initialization_params)?;
    io_threads.join()?;

    log::info!("shutting down lspml...");
    return Ok(());
}

fn dump(artifact: &Artifact) -> Result<(), Box<dyn Error + Sync + Send>> {
    print!(
        "{}",
        match artifact {
            Artifact::HtmlCustomData => custom_data::html_custom_data()?,
        }
    );
    return Ok(());
}

fn initialize(connection: &Connection) -> Result<InitializeParams> {
    let (id, params) = connection.initialize_start()?;
    let params: InitializeParams = serde_json::from_value(params)?;
    let capabilities = capabilities::create(&params.capabilities);
    // What the answer declares is what every column on the wire is counted in
    // from here on, in both directions.
    if let Some(position_encoding) = capabilities.position_encoding.clone() {
        encoding::settle(position_encoding);
    }
    let capabilities = serde_json::to_value(capabilities)?;
    connection.initialize_finish(id, serde_json::json!({ "capabilities": capabilities }))?;
    return Ok(params);
}

fn main_loop(
    connection: Connection,
    _initialization_params: InitializeParams,
) -> Result<(), Box<dyn Error + Sync + Send>> {
    log::info!("server started");

    for message in &connection.receiver {
        match message {
            Message::Request(request) => {
                if connection.handle_shutdown(&request)? {
                    break;
                }
                let method = match request.method.as_str() {
                    "textDocument/completion" => command::complete,
                    "textDocument/definition" => command::definition,
                    "textDocument/diagnostic" => command::diagnostic,
                    "textDocument/semanticTokens/full" => command::semantics,
                    "textDocument/codeAction" => command::action,
                    "textDocument/hover" => command::hover,
                    _ => command::unknown,
                };
                let response = method(request)?;
                connection
                    .sender
                    .send(response)
                    .map_err(|err| anyhow::anyhow!(err))?;
            }
            Message::Response(response) => {
                log::info!("got unknown response: {:?}", response);
            }
            Message::Notification(notification) => match notification.method.as_str() {
                "textDocument/didChange" => {
                    changed(serde_json::from_value(notification.params)?)?;
                }
                "textDocument/didOpen" => {
                    opened(serde_json::from_value(notification.params)?)?;
                }
                "textDocument/didSave" => {
                    saved(serde_json::from_value(notification.params)?)?;
                }
                "textDocument/didClose" => {
                    closed(serde_json::from_value(notification.params)?)?;
                }
                "$/cancelRequest" => {
                    let params: CancelParams = serde_json::from_value(notification.params).unwrap();
                    log::debug!("attempted to cancel request {:?}", params.id);
                }
                _ => log::info!("got unknown notification: {:?}", notification),
            },
        }
    }

    return Ok(());
}

fn changed(params: DidChangeTextDocumentParams) -> Result<()> {
    let DidChangeTextDocumentParams {
        text_document,
        mut content_changes,
    } = params;
    let uri = text_document.uri;
    return match content_changes.pop() {
        Some(change) => {
            document_store::update(&uri, text_document.version, change.text).map(|_| {
                log::debug!("updated {:?}", uri);
            })
        }
        None => Ok(()),
    };
}

fn opened(params: DidOpenTextDocumentParams) -> Result<()> {
    let document = params.text_document;
    let uri = document.uri;
    return document_store::update(&uri, document.version, document.text).map(|_| {
        log::debug!("opened {:?}", uri);
    });
}

fn saved(params: DidSaveTextDocumentParams) -> Result<()> {
    let uri = params.text_document.uri;
    return document_store::save(&uri, params.text).map(|_| {
        log::debug!("saved {:?}", uri);
    });
}

fn closed(params: DidCloseTextDocumentParams) -> Result<()> {
    let uri = params.text_document.uri;
    if document_store::close(&uri) {
        log::debug!("closed {:?}", uri);
    }
    return Ok(());
}

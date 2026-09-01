use anyhow::{Error, Result};
use lsp_server::{ErrorCode, Message, Request, RequestId, Response, ResponseError};
use lsp_types::{
    CodeActionParams, CompletionParams, CompletionResponse, DocumentDiagnosticParams,
    DocumentDiagnosticReport, FullDocumentDiagnosticReport, GotoDefinitionParams, HoverParams,
    RelatedFullDocumentDiagnosticReport, SemanticTokens, SemanticTokensParams,
    SemanticTokensResult, Uri,
};
use std::{fmt, sync::Arc};

use document_store::Document;

use crate::encoding;

mod action;
mod complete;
mod definition;
mod hover;
mod semantics;

#[derive(Debug)]
pub(crate) struct LsError {
    pub(crate) message: String,
    pub(crate) code: ErrorCode,
}

impl std::error::Error for LsError {}

impl fmt::Display for LsError {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        return write!(formatter, "{:?}: {}", self.code, self.message);
    }
}

impl From<Error> for LsError {
    fn from(error: Error) -> Self {
        return LsError {
            message: error.to_string(),
            code: ErrorCode::RequestFailed,
        };
    }
}

impl LsError {
    fn to_response(self, request_id: RequestId) -> Response {
        return Response {
            id: request_id,
            result: None,
            error: Some(ResponseError {
                message: self.message,
                code: self.code as i32,
                data: None,
            }),
        };
    }
}

/// The document a request is about, or the error to answer the request with
/// when the file behind it cannot be read.
fn document(uri: &Uri) -> Result<Arc<Document>, LsError> {
    return document_store::get_or_load(uri).map_err(|err| {
        log::error!("failed to read {:?}: {}", uri, err);
        return LsError {
            message: format!("cannot read file {:?}", uri),
            code: ErrorCode::RequestFailed,
        };
    });
}

pub(crate) fn complete(request: Request) -> Result<Message> {
    log::trace!("got completion request: {request:?}");
    let mut params: CompletionParams = serde_json::from_value(request.params)?;
    let uri = params.text_document_position.text_document.uri.clone();
    encoding::inbound(&uri, &mut params);
    return Ok(Message::Response(match complete::complete(params) {
        Ok(mut completions) => {
            encoding::outbound(&uri, &mut completions);
            Response {
                id: request.id,
                result: serde_json::to_value(CompletionResponse::Array(completions)).ok(),
                error: None,
            }
        }
        Err(err) => err.to_response(request.id),
    }));
}

pub(crate) fn definition(request: Request) -> Result<Message> {
    log::trace!("got go to definition request: {request:?}");
    let mut params: GotoDefinitionParams = serde_json::from_value(request.params)?;
    let uri = params
        .text_document_position_params
        .text_document
        .uri
        .clone();
    encoding::inbound(&uri, &mut params);
    return Ok(Message::Response(match definition::definition(params) {
        Ok(mut definition) => {
            encoding::outbound(&uri, &mut definition);
            Response {
                id: request.id,
                result: definition
                    .and_then(|d| serde_json::to_value(d).ok())
                    .or(Some(serde_json::value::Value::Null)),
                error: None,
            }
        }
        Err(err) => err.to_response(request.id),
    }));
}

pub(crate) fn diagnostic(request: Request) -> Result<Message> {
    log::trace!("got diagnose request: {request:?}");
    let params: DocumentDiagnosticParams = serde_json::from_value(request.params)?;
    let uri = params.text_document.uri.clone();
    return Ok(Message::Response(match diagnostic::diagnostic(params) {
        Ok(diagnostic) => {
            let mut items = diagnostic
                .into_iter()
                .map(|d| d.to_lsp_type())
                .collect::<Vec<_>>();
            encoding::outbound(&uri, &mut items);
            Response {
                id: request.id,
                result: serde_json::to_value(DocumentDiagnosticReport::Full(
                    RelatedFullDocumentDiagnosticReport {
                        related_documents: None,
                        full_document_diagnostic_report: FullDocumentDiagnosticReport {
                            result_id: None,
                            items,
                        },
                    },
                ))
                .ok(),
                error: None,
            }
        }
        Err(err) => LsError::from(err).to_response(request.id),
    }));
}

pub(crate) fn hover(request: Request) -> Result<Message> {
    log::trace!("got hover request: {request:?}");
    let mut params: HoverParams = serde_json::from_value(request.params)?;
    let uri = params
        .text_document_position_params
        .text_document
        .uri
        .clone();
    encoding::inbound(&uri, &mut params);
    let response = match hover::hover(params) {
        Ok(Some(mut result)) => {
            encoding::outbound(&uri, &mut result);
            Response {
                id: request.id,
                result: Some(serde_json::to_value(result)?),
                error: None,
            }
        }
        // `Response::result` is skipped when serializing if it is `None`, which
        // would put a response on the wire that has neither a result nor an
        // error. Clients reject that, so "no hover here" has to be an explicit
        // json null.
        Ok(None) => Response {
            id: request.id,
            result: Some(serde_json::value::Value::Null),
            error: None,
        },
        Err(err) => err.to_response(request.id),
    };
    return Ok(Message::Response(response));
}

pub(crate) fn semantics(request: Request) -> Result<Message> {
    log::trace!("got semantics request: {request:?}");
    let params: SemanticTokensParams = serde_json::from_value(request.params)?;
    let uri = params.text_document.uri.clone();
    return Ok(Message::Response(match semantics::semantics(params) {
        Ok(mut tokens) => {
            encoding::outbound_semantic_tokens(&uri, &mut tokens);
            Response {
                id: request.id,
                result: serde_json::to_value(SemanticTokensResult::Tokens(SemanticTokens {
                    result_id: None,
                    data: tokens,
                }))
                .ok(),
                error: None,
            }
        }
        Err(err) => err.to_response(request.id),
    }));
}

pub(crate) fn action(request: Request) -> Result<Message> {
    log::trace!("got code-action request: {request:?}");
    let mut params: CodeActionParams = serde_json::from_value(request.params)?;
    let uri = params.text_document.uri.clone();
    encoding::inbound(&uri, &mut params);
    return Ok(Message::Response(match action::action(params) {
        Ok(mut actions) => {
            encoding::outbound(&uri, &mut actions);
            match serde_json::to_value(actions) {
                Ok(result) => Response {
                    id: request.id,
                    result: Some(result),
                    error: None,
                },
                // TODO: this might not be the way to handle json errors..
                Err(err) => Response {
                    id: request.id,
                    result: None,
                    error: Some(ResponseError {
                        message: format!("{}", err),
                        code: ErrorCode::RequestFailed as i32,
                        data: None,
                    }),
                },
            }
        }
        Err(err) => err.to_response(request.id),
    }));
}

pub(crate) fn unknown(request: Request) -> Result<Message> {
    log::info!("got unknonwn request: {request:?}");
    return Ok(Message::Response(Response {
        id: request.id,
        result: None,
        error: Some(ResponseError {
            message: format!("method \"{}\" not found", request.method),
            code: ErrorCode::MethodNotFound as i32,
            data: None,
        }),
    }));
}

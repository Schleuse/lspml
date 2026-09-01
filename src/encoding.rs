use std::{
    collections::HashMap,
    sync::{Arc, OnceLock},
};

use lsp_types::{
    CodeAction, CodeActionOrCommand, CodeActionParams, CompletionItem, CompletionParams,
    CompletionTextEdit, Diagnostic, DiagnosticRelatedInformation, GotoDefinitionParams,
    GotoDefinitionResponse, Hover, HoverParams, InsertReplaceEdit, Location, LocationLink,
    Position, PositionEncodingKind, Range, SemanticToken, TextDocumentPositionParams, TextEdit,
    Uri, WorkspaceEdit,
};

use document_store::Document;

/// The encoding the handshake settled on, which is what every column on the
/// wire is counted in.
static ENCODING: OnceLock<PositionEncodingKind> = OnceLock::new();

/// Records what the handshake settled on. Called once, before the first request
/// arrives; until then columns are rewritten, which is what the utf-16 the
/// negotiation falls back to needs.
pub(crate) fn settle(encoding: PositionEncodingKind) {
    if ENCODING.set(encoding).is_err() {
        log::warn!("the position encoding was settled twice");
    }
}

/// Rewrites the positions in `value` from the columns the client counts into
/// the byte columns the server works in.
///
/// `uri` is the document the request is about, which is the one every position
/// that does not name a document of its own belongs to.
pub(crate) fn inbound<T: Translate>(uri: &Uri, value: &mut T) {
    translate(Direction::Inbound, uri, value);
}

/// Rewrites the positions in `value` from the byte columns the server works in
/// into the columns the client counts. The counterpart of [`inbound`].
pub(crate) fn outbound<T: Translate>(uri: &Uri, value: &mut T) {
    translate(Direction::Outbound, uri, value);
}

/// Rewrites the columns of `tokens`, which are counted in the client's encoding
/// like every other column on the wire.
///
/// Semantic tokens carry no positions: each one is a line and column delta from
/// the token before it, plus a length. Translating them means walking the deltas
/// back into the positions they describe and deriving new deltas from the
/// translated ones.
pub(crate) fn outbound_semantic_tokens(uri: &Uri, tokens: &mut Vec<SemanticToken>) {
    let Some(mut translator) = translator(Direction::Outbound) else {
        return;
    };
    let mut line = 0;
    let mut column = 0;
    let mut previous = Position::default();
    for token in tokens {
        line += token.delta_line;
        column = match token.delta_line {
            0 => column + token.delta_start,
            _ => token.delta_start,
        };
        // A token never spans lines, so its length is a column distance within
        // the line it starts on.
        let start = translator.position(uri, position(line, column));
        let end = translator.position(uri, position(line, column + token.length));
        token.delta_line = start.line - previous.line;
        token.delta_start = match token.delta_line {
            0 => start.character - previous.character,
            _ => start.character,
        };
        token.length = end.character - start.character;
        previous = start;
    }
}

fn translate<T: Translate>(direction: Direction, uri: &Uri, value: &mut T) {
    if let Some(mut translator) = translator(direction) {
        value.translate(&mut translator, uri);
    }
}

/// What to rewrite the columns of one message with, or `None` where the client
/// counts what the server already produces and there is nothing to rewrite.
fn translator(direction: Direction) -> Option<Translator> {
    return match rewrites_columns(ENCODING.get()) {
        true => Some(Translator::new(direction)),
        false => None,
    };
}

/// Whether columns have to be rewritten at all. A client that counts bytes
/// counts what the server already produces, which leaves nothing to do. An
/// encoding the handshake never settled is treated as the utf-16 it falls back
/// to, so a position is never left in the wrong encoding by accident.
fn rewrites_columns(encoding: Option<&PositionEncodingKind>) -> bool {
    return encoding != Some(&PositionEncodingKind::UTF8);
}

#[derive(Clone, Copy, Debug)]
enum Direction {
    Inbound,
    Outbound,
}

/// Rewrites the columns of the positions in one message.
///
/// A message names few documents but can name many positions in each, so the
/// line index of every document it touches is looked up once and held for as
/// long as the message takes to translate.
pub(crate) struct Translator {
    direction: Direction,
    documents: HashMap<Uri, Option<Arc<Document>>>,
}

impl Translator {
    fn new(direction: Direction) -> Translator {
        return Translator {
            direction,
            documents: HashMap::new(),
        };
    }

    fn position(&mut self, uri: &Uri, position: Position) -> Position {
        let direction = self.direction;
        return match self.document(uri) {
            Some(document) => match direction {
                Direction::Inbound => document.line_index.byte_columns(position),
                Direction::Outbound => document.line_index.utf16_columns(position),
            },
            // Nothing to count the columns against. Answering with the position
            // as it stands is what the server did before it converted at all,
            // and beats dropping the message.
            None => position,
        };
    }

    fn range(&mut self, uri: &Uri, range: &mut Range) {
        range.start = self.position(uri, range.start);
        range.end = self.position(uri, range.end);
    }

    fn document(&mut self, uri: &Uri) -> Option<Arc<Document>> {
        return self
            .documents
            .entry(uri.clone())
            .or_insert_with(|| match document_store::get_or_load(uri) {
                Ok(document) => Some(document),
                Err(err) => {
                    log::warn!("cannot count the columns of {:?}: {}", uri, err);
                    None
                }
            })
            .clone();
    }
}

/// A message shape whose positions belong to a document and are therefore
/// counted in one encoding on the wire and another inside the server.
pub(crate) trait Translate {
    /// Rewrites every position in this value. `uri` names the document the
    /// positions belong to, which shapes carrying a uri of their own override.
    fn translate(&mut self, translator: &mut Translator, uri: &Uri);
}

impl<T: Translate> Translate for Option<T> {
    fn translate(&mut self, translator: &mut Translator, uri: &Uri) {
        if let Some(value) = self {
            value.translate(translator, uri);
        }
    }
}

impl<T: Translate> Translate for Vec<T> {
    fn translate(&mut self, translator: &mut Translator, uri: &Uri) {
        for value in self {
            value.translate(translator, uri);
        }
    }
}

impl Translate for Position {
    fn translate(&mut self, translator: &mut Translator, uri: &Uri) {
        *self = translator.position(uri, *self);
    }
}

impl Translate for Range {
    fn translate(&mut self, translator: &mut Translator, uri: &Uri) {
        translator.range(uri, self);
    }
}

impl Translate for TextEdit {
    fn translate(&mut self, translator: &mut Translator, uri: &Uri) {
        translator.range(uri, &mut self.range);
    }
}

impl Translate for TextDocumentPositionParams {
    fn translate(&mut self, translator: &mut Translator, uri: &Uri) {
        self.position.translate(translator, uri);
    }
}

impl Translate for CompletionParams {
    fn translate(&mut self, translator: &mut Translator, uri: &Uri) {
        self.text_document_position.translate(translator, uri);
    }
}

impl Translate for GotoDefinitionParams {
    fn translate(&mut self, translator: &mut Translator, uri: &Uri) {
        self.text_document_position_params
            .translate(translator, uri);
    }
}

impl Translate for HoverParams {
    fn translate(&mut self, translator: &mut Translator, uri: &Uri) {
        self.text_document_position_params
            .translate(translator, uri);
    }
}

impl Translate for CodeActionParams {
    fn translate(&mut self, translator: &mut Translator, uri: &Uri) {
        translator.range(uri, &mut self.range);
        self.context.diagnostics.translate(translator, uri);
    }
}

impl Translate for Hover {
    fn translate(&mut self, translator: &mut Translator, uri: &Uri) {
        self.range.translate(translator, uri);
    }
}

impl Translate for CompletionItem {
    fn translate(&mut self, translator: &mut Translator, uri: &Uri) {
        match &mut self.text_edit {
            Some(CompletionTextEdit::Edit(edit)) => edit.translate(translator, uri),
            Some(CompletionTextEdit::InsertAndReplace(InsertReplaceEdit {
                insert,
                replace,
                ..
            })) => {
                translator.range(uri, insert);
                translator.range(uri, replace);
            }
            None => (),
        }
        self.additional_text_edits.translate(translator, uri);
    }
}

/// A diagnostic's `data` is deliberately left alone. It is opaque to the client,
/// which only hands it back with the code action request it belongs to, and the
/// edits it carries reach the client through the action's workspace edit - which
/// is translated where every other outbound edit is.
impl Translate for Diagnostic {
    fn translate(&mut self, translator: &mut Translator, uri: &Uri) {
        translator.range(uri, &mut self.range);
        self.related_information.translate(translator, uri);
    }
}

impl Translate for DiagnosticRelatedInformation {
    fn translate(&mut self, translator: &mut Translator, uri: &Uri) {
        self.location.translate(translator, uri);
    }
}

/// A location names the document it belongs to, which is not necessarily the
/// one the request was about - a go-to-definition answers with the file it
/// jumped to.
impl Translate for Location {
    fn translate(&mut self, translator: &mut Translator, _uri: &Uri) {
        let uri = self.uri.clone();
        translator.range(&uri, &mut self.range);
    }
}

impl Translate for LocationLink {
    fn translate(&mut self, translator: &mut Translator, uri: &Uri) {
        // Only the origin belongs to the document the request was about; the
        // other two ranges are counted against the file jumped to.
        self.origin_selection_range.translate(translator, uri);
        let target = self.target_uri.clone();
        translator.range(&target, &mut self.target_range);
        translator.range(&target, &mut self.target_selection_range);
    }
}

impl Translate for GotoDefinitionResponse {
    fn translate(&mut self, translator: &mut Translator, uri: &Uri) {
        match self {
            GotoDefinitionResponse::Scalar(location) => location.translate(translator, uri),
            GotoDefinitionResponse::Array(locations) => locations.translate(translator, uri),
            GotoDefinitionResponse::Link(links) => links.translate(translator, uri),
        }
    }
}

impl Translate for CodeActionOrCommand {
    fn translate(&mut self, translator: &mut Translator, uri: &Uri) {
        match self {
            CodeActionOrCommand::CodeAction(action) => action.translate(translator, uri),
            // A command carries arguments the server itself defines, so there
            // is no position in it to find.
            CodeActionOrCommand::Command(_) => (),
        }
    }
}

impl Translate for CodeAction {
    fn translate(&mut self, translator: &mut Translator, uri: &Uri) {
        self.diagnostics.translate(translator, uri);
        self.edit.translate(translator, uri);
    }
}

/// The server describes every edit it offers through `changes`, so the
/// `document_changes` half of a workspace edit is deliberately not walked. A
/// code action that starts using it has to be added here.
impl Translate for WorkspaceEdit {
    fn translate(&mut self, translator: &mut Translator, _uri: &Uri) {
        if let Some(changes) = &mut self.changes {
            for (edited, edits) in changes {
                let edited = edited.clone();
                edits.translate(translator, &edited);
            }
        }
    }
}

fn position(line: u32, character: u32) -> Position {
    return Position { line, character };
}

#[cfg(test)]
mod tests {
    use lsp_types::{
        Diagnostic, Location, LocationLink, PositionEncodingKind, Range, SemanticToken, Uri,
    };

    use super::{Direction, Translate, Translator, position};

    /// A line whose columns differ between the two encodings from the "ö"
    /// onwards: `<div class="größe">` is 21 bytes but 19 utf-16 code units, so
    /// the `<sp:if` behind it starts at column 21 for the server and at column
    /// 19 for the client.
    const TEXT: &str = include_str!("../fixtures/german.spml");
    const UMLAUT_LINE: u32 = 5;

    fn document(name: &str) -> Uri {
        let uri = format!("file:///encoding/{}.spml", name)
            .parse::<Uri>()
            .expect("test uri should parse");
        document_store::update(&uri, 1, TEXT.to_string()).expect("fixture should parse");
        return uri;
    }

    fn token(delta_line: u32, delta_start: u32, length: u32) -> SemanticToken {
        return SemanticToken {
            delta_line,
            delta_start,
            length,
            token_type: 0,
            token_modifiers_bitset: 0,
        };
    }

    fn translated<T: Translate>(direction: Direction, uri: &Uri, mut value: T) -> T {
        value.translate(&mut Translator::new(direction), uri);
        return value;
    }

    #[test]
    fn test_a_client_counting_bytes_has_nothing_rewritten() {
        assert!(!super::rewrites_columns(Some(&PositionEncodingKind::UTF8)));
        assert!(super::rewrites_columns(Some(&PositionEncodingKind::UTF16)));
        assert!(super::rewrites_columns(None));
    }

    #[test]
    fn test_an_outbound_range_is_counted_in_the_clients_code_units() {
        let uri = document("outbound-range");

        let range = translated(
            Direction::Outbound,
            &uri,
            Range {
                start: position(UMLAUT_LINE, 28),
                end: position(UMLAUT_LINE, 40),
            },
        );

        assert_eq!(range.start, position(UMLAUT_LINE, 26));
        assert_eq!(range.end, position(UMLAUT_LINE, 38));
    }

    #[test]
    fn test_an_inbound_position_is_counted_in_bytes() {
        let uri = document("inbound-position");

        let position = translated(Direction::Inbound, &uri, position(UMLAUT_LINE, 19));

        assert_eq!(position, super::position(UMLAUT_LINE, 21));
    }

    #[test]
    fn test_a_location_is_counted_against_the_document_it_names() {
        let uri = document("location-target");
        let requested = document("location-origin");

        let location = translated(
            Direction::Outbound,
            &requested,
            Location {
                uri: uri.clone(),
                range: Range {
                    start: position(UMLAUT_LINE, 28),
                    end: position(UMLAUT_LINE, 28),
                },
            },
        );

        assert_eq!(location.range.start, position(UMLAUT_LINE, 26));
    }

    #[test]
    fn test_only_the_origin_of_a_location_link_belongs_to_the_requested_document() {
        let requested = document("link-origin");
        let target = document("link-target");

        let link = translated(
            Direction::Outbound,
            &requested,
            LocationLink {
                origin_selection_range: Some(Range {
                    start: position(UMLAUT_LINE, 28),
                    end: position(UMLAUT_LINE, 40),
                }),
                target_uri: target,
                target_range: Range {
                    start: position(UMLAUT_LINE, 21),
                    end: position(UMLAUT_LINE, 21),
                },
                target_selection_range: Range {
                    start: position(UMLAUT_LINE, 21),
                    end: position(UMLAUT_LINE, 21),
                },
            },
        );

        assert_eq!(
            link.origin_selection_range.map(|range| range.start),
            Some(position(UMLAUT_LINE, 26))
        );
        assert_eq!(link.target_range.start, position(UMLAUT_LINE, 19));
        assert_eq!(link.target_selection_range.start, position(UMLAUT_LINE, 19));
    }

    #[test]
    fn test_the_data_of_a_diagnostic_is_left_in_the_columns_the_server_counts() {
        let uri = document("diagnostic-data");
        let data = serde_json::json!([{
            "range": {
                "start": { "line": UMLAUT_LINE, "character": 28 },
                "end": { "line": UMLAUT_LINE, "character": 40 },
            },
            "newText": "",
        }]);

        let diagnostic = translated(
            Direction::Outbound,
            &uri,
            Diagnostic {
                range: Range {
                    start: position(UMLAUT_LINE, 28),
                    end: position(UMLAUT_LINE, 40),
                },
                data: Some(data.clone()),
                ..Diagnostic::default()
            },
        );

        assert_eq!(diagnostic.range.start, position(UMLAUT_LINE, 26));
        assert_eq!(diagnostic.data, Some(data));
    }

    #[test]
    fn test_a_position_in_a_document_the_store_cannot_read_is_left_alone() {
        let uri = "file:///encoding/missing.spml"
            .parse::<Uri>()
            .expect("test uri should parse");

        let position = translated(Direction::Outbound, &uri, position(UMLAUT_LINE, 28));

        assert_eq!(position, super::position(UMLAUT_LINE, 28));
    }

    #[test]
    fn test_semantic_tokens_are_recounted_through_the_positions_their_deltas_describe() {
        let uri = document("semantic-tokens");
        // The tag name, the "Grüße" in the body and the closing tag of the
        // umlaut line. "Grüße" is seven bytes and five code units, so it
        // shortens both its own length and the delta of the token behind it.
        let mut tokens = vec![token(UMLAUT_LINE, 21, 6), token(0, 27, 7), token(0, 7, 8)];

        super::outbound_semantic_tokens(&uri, &mut tokens);

        assert_eq!(tokens[0], token(UMLAUT_LINE, 19, 6));
        assert_eq!(tokens[1], token(0, 27, 5));
        assert_eq!(tokens[2], token(0, 5, 8));
    }

    #[test]
    fn test_a_position_survives_the_trip_out_to_the_client_and_back() {
        let uri = document("round-trip");
        let start = position(UMLAUT_LINE, 28);

        let outbound = translated(Direction::Outbound, &uri, start);
        let inbound = translated(Direction::Inbound, &uri, outbound);

        assert_ne!(outbound, start);
        assert_eq!(inbound, start);
    }
}

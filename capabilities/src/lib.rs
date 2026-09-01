use std::fmt::{Display, Formatter};

use lsp_types::{
    ClientCapabilities, CodeActionKind, CodeActionOptions, CodeActionProviderCapability,
    CompletionOptions, CompletionOptionsCompletionItem, DiagnosticOptions,
    DiagnosticServerCapabilities, HoverOptions, HoverProviderCapability, NumberOrString, OneOf,
    PositionEncodingKind, SemanticTokenModifier, SemanticTokenType, SemanticTokensFullOptions,
    SemanticTokensLegend, SemanticTokensOptions, SemanticTokensServerCapabilities,
    ServerCapabilities, TextDocumentSyncCapability, TextDocumentSyncKind, WorkDoneProgressOptions,
};

/// Characters that begin a completable construct, which clients use to fire
/// completion as the developer types: a tag ("<") and the separator between its
/// namespace and its name (":"), an attribute value ("\""), an expression
/// variable ("$"), a member of an object (".") and a path segment of a uri
/// ("/"). A space is deliberately not among them - it would fire inside prose,
/// where nothing is completable.
pub const COMPLETION_TRIGGER_CHARACTERS: &'static [&'static str] = &["<", ":", "\"", "$", ".", "/"];

pub const TOKEN_TYPES: &'static [SemanticTokenType] = &[
    SemanticTokenType::ENUM,
    SemanticTokenType::ENUM_MEMBER,
    SemanticTokenType::FUNCTION,
    SemanticTokenType::KEYWORD,
    SemanticTokenType::MACRO,
    SemanticTokenType::METHOD,
    SemanticTokenType::NUMBER,
    SemanticTokenType::OPERATOR,
    SemanticTokenType::PROPERTY,
    SemanticTokenType::REGEXP,
    SemanticTokenType::STRING,
    SemanticTokenType::VARIABLE,
];

pub const TOKEN_MODIFIERS: &'static [SemanticTokenModifier] = &[
    SemanticTokenModifier::DECLARATION,
    SemanticTokenModifier::DEFINITION,
    SemanticTokenModifier::DEPRECATED,
    SemanticTokenModifier::DOCUMENTATION,
    SemanticTokenModifier::MODIFICATION,
];

pub enum CodeActionImplementation {
    GenerateDefaultHeaders,
    NameToCondition,
    ConditionToName,
    FixSpelSyntax,
    RemoveSuperfluousCode,
    AddMissingCode,
}

impl CodeActionImplementation {
    pub const GENERATE_DEFAULT_HEADER_CODE: NumberOrString = NumberOrString::Number(7126);
    pub const FIX_SPEL_SYNTAX_CODE: NumberOrString = NumberOrString::Number(7127);
    pub const REMOVE_SUPERFLUOUS_CODE: NumberOrString = NumberOrString::Number(7128);
    pub const ADD_MISSING_CODE: NumberOrString = NumberOrString::Number(7129);

    pub(crate) fn kinds() -> Vec<CodeActionKind> {
        return vec![
            CodeActionImplementation::GenerateDefaultHeaders.to_kind(),
            CodeActionImplementation::NameToCondition.to_kind(),
            CodeActionImplementation::ConditionToName.to_kind(),
            CodeActionImplementation::FixSpelSyntax.to_kind(),
            CodeActionImplementation::RemoveSuperfluousCode.to_kind(),
            CodeActionImplementation::AddMissingCode.to_kind(),
            CodeActionKind::SOURCE_FIX_ALL,
        ];
    }

    pub fn to_kind(&self) -> CodeActionKind {
        return CodeActionKind::new(match self {
            CodeActionImplementation::GenerateDefaultHeaders => "refactor.generate_default_headers",
            CodeActionImplementation::NameToCondition => "refactor.name_to_condition",
            CodeActionImplementation::ConditionToName => "refactor.condition_to_name",
            CodeActionImplementation::FixSpelSyntax => "quickfix.fix_spel_syntax",
            CodeActionImplementation::RemoveSuperfluousCode => "quickfix.remove_superfluous_code",
            CodeActionImplementation::AddMissingCode => "quickfix.add_missing_code",
        });
    }
}

impl Display for CodeActionImplementation {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            CodeActionImplementation::GenerateDefaultHeaders => "refactor.generate_default_headers",
            CodeActionImplementation::NameToCondition => "refactor.name_to_condition",
            CodeActionImplementation::ConditionToName => "refactor.condition_to_name",
            CodeActionImplementation::FixSpelSyntax => "quickfix.fix_spel_syntax",
            CodeActionImplementation::RemoveSuperfluousCode => "quickfix.remove_superfluous_code",
            CodeActionImplementation::AddMissingCode => "quickfix.add_missing_code",
        })
    }
}

/// Picks the encoding the positions on the wire are counted in.
///
/// Every position this server emits comes from a tree-sitter point, whose
/// columns count bytes, so utf-8 is what we would like to answer. A client only
/// has to understand utf-16 though, and rejects an encoding it never offered -
/// vscode advertises utf-16 alone and refuses to connect to a server declaring
/// anything else. Falling back to utf-16 leaves those clients misplacing
/// positions behind a multi byte character, which is what converting the
/// columns in #9 is for.
pub fn negotiate_position_encoding(client: &ClientCapabilities) -> PositionEncodingKind {
    return match &client.general {
        Some(general) => match &general.position_encodings {
            Some(encodings) if encodings.contains(&PositionEncodingKind::UTF8) => {
                PositionEncodingKind::UTF8
            }
            _ => PositionEncodingKind::UTF16,
        },
        None => PositionEncodingKind::UTF16,
    };
}

pub fn create(client: &ClientCapabilities) -> ServerCapabilities {
    return ServerCapabilities {
        definition_provider: Some(OneOf::Left(true)),
        text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
        position_encoding: Some(negotiate_position_encoding(client)),
        semantic_tokens_provider: Some(SemanticTokensServerCapabilities::SemanticTokensOptions(
            SemanticTokensOptions {
                full: Some(SemanticTokensFullOptions::Bool(true)),
                legend: SemanticTokensLegend {
                    token_types: TOKEN_TYPES.to_vec(),
                    token_modifiers: TOKEN_MODIFIERS.to_vec(),
                },
                ..Default::default()
            },
        )),
        diagnostic_provider: Some(DiagnosticServerCapabilities::Options(DiagnosticOptions {
            inter_file_dependencies: true,
            ..DiagnosticOptions::default()
        })),
        completion_provider: Some(CompletionOptions {
            completion_item: Some(CompletionOptionsCompletionItem {
                label_details_support: Some(true),
            }),
            trigger_characters: Some(
                COMPLETION_TRIGGER_CHARACTERS
                    .iter()
                    .map(|character| character.to_string())
                    .collect(),
            ),
            ..CompletionOptions::default()
        }),
        hover_provider: Some(HoverProviderCapability::Options(HoverOptions {
            work_done_progress_options: WorkDoneProgressOptions {
                work_done_progress: Some(true),
            },
        })),
        code_action_provider: Some(CodeActionProviderCapability::Options(CodeActionOptions {
            code_action_kinds: Some(CodeActionImplementation::kinds()),
            ..CodeActionOptions::default()
        })),
        ..ServerCapabilities::default()
    };
}

#[cfg(test)]
mod tests {
    use lsp_types::{ClientCapabilities, GeneralClientCapabilities, PositionEncodingKind};

    use super::{create, negotiate_position_encoding};

    fn advertising(encodings: Option<Vec<PositionEncodingKind>>) -> ClientCapabilities {
        return ClientCapabilities {
            general: Some(GeneralClientCapabilities {
                position_encodings: encodings,
                ..GeneralClientCapabilities::default()
            }),
            ..ClientCapabilities::default()
        };
    }

    #[test]
    fn test_document_highlight_is_not_advertised() {
        assert_eq!(
            create(&ClientCapabilities::default()).document_highlight_provider,
            None
        );
    }

    #[test]
    fn test_positions_are_declared_to_be_encoded_in_utf8_where_it_is_supported() {
        let client = advertising(Some(vec![
            PositionEncodingKind::UTF16,
            PositionEncodingKind::UTF8,
        ]));
        assert_eq!(
            negotiate_position_encoding(&client),
            PositionEncodingKind::UTF8
        );
        assert_eq!(
            create(&client).position_encoding,
            Some(PositionEncodingKind::UTF8)
        );
    }

    // Vscode advertises this exact list and rejects a server that answers with
    // anything but utf-16, refusing to start the client at all.
    #[test]
    fn test_positions_fall_back_to_utf16_where_utf8_is_not_supported() {
        let client = advertising(Some(vec![PositionEncodingKind::UTF16]));
        assert_eq!(
            negotiate_position_encoding(&client),
            PositionEncodingKind::UTF16
        );
        assert_eq!(
            create(&client).position_encoding,
            Some(PositionEncodingKind::UTF16)
        );
    }

    #[test]
    fn test_positions_fall_back_to_utf16_for_a_client_that_advertises_no_encoding() {
        assert_eq!(
            negotiate_position_encoding(&advertising(None)),
            PositionEncodingKind::UTF16
        );
        assert_eq!(
            negotiate_position_encoding(&ClientCapabilities::default()),
            PositionEncodingKind::UTF16
        );
    }

    #[test]
    fn test_completion_triggers_on_the_characters_that_begin_a_construct() {
        assert_eq!(
            create(&ClientCapabilities::default())
                .completion_provider
                .and_then(|provider| provider.trigger_characters),
            Some(
                ["<", ":", "\"", "$", ".", "/"]
                    .iter()
                    .map(|character| character.to_string())
                    .collect::<Vec<String>>()
            )
        );
    }
}

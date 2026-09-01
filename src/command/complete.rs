use std::{fs, iter::Iterator};

use anyhow::Result;
use lsp_types::{
    CompletionContext, CompletionItem, CompletionItemKind, CompletionParams, CompletionTextEdit,
    CompletionTriggerKind, Documentation, MarkupContent, MarkupKind, Position, Range,
    TextDocumentPositionParams, TextEdit, Uri as Url,
};

use grammar::{
    self, AttributeRule, TagAttribute, TagAttributeType, TagAttributes, TagChildren, TagDefinition,
};
use parser::{
    AttributeValue, ErrorNode, Node, ParsableTag, ParsedAttribute, ParsedTag, SpelAttribute,
    SpelAttributeValue, SpmlTag, TagBody, TagError,
};
use spel::ast::{SpelAst, SpelResult, StringLiteral, Uri, Word, WordFragment};

use super::LsError;

/// What made the client ask for completions. Declaring trigger characters means
/// completion now fires as the developer types, at positions where nothing is
/// completable just as readily as at ones where something is.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Trigger {
    /// The developer asked for completions explicitly, e.g. via ctrl+space.
    Invoked,
    /// The developer typed one of the declared trigger characters.
    Character(char),
}

/// The kind of construct a completion belongs to. Every trigger character
/// begins one kind and no other, so a typed character only ever yields
/// completions of the construct it begins - typing prose yields none.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Completable {
    /// A tag, including the operators that terminate it.
    Tag,
    /// The name of an attribute. No trigger character begins one, because the
    /// space that precedes it is too noisy to trigger on.
    AttributeName,
    /// Anything within an attribute value: spel, value domains and uris.
    AttributeValue,
}

impl Trigger {
    fn of(context: Option<&CompletionContext>) -> Trigger {
        return match context {
            Some(CompletionContext {
                trigger_kind: CompletionTriggerKind::TRIGGER_CHARACTER,
                trigger_character: Some(character),
            }) => match character.chars().next() {
                Some(character) => Trigger::Character(character),
                None => Trigger::Invoked,
            },
            _ => Trigger::Invoked,
        };
    }

    fn completes(&self, completable: Completable) -> bool {
        let character = match self {
            Trigger::Invoked => return true,
            Trigger::Character(character) => character,
        };
        return match character {
            // "<" opens a tag and ":" separates its namespace from its name
            '<' | ':' => completable == Completable::Tag,
            // """ opens an attribute value, "$" begins an expression variable,
            // "." accesses an object member and "/" enters a path segment
            '"' | '$' | '.' | '/' => completable == Completable::AttributeValue,
            _ => false,
        };
    }
}

#[derive(Debug)]
struct CompletionCollector<'a> {
    cursor: Position,
    file: &'a Url,
    document: &'a document_store::Document,
    trigger: Trigger,
    completions: Vec<CompletionItem>,
}

impl CompletionCollector<'_> {
    fn new<'a>(
        params: &'a TextDocumentPositionParams,
        document: &'a document_store::Document,
        trigger: Trigger,
    ) -> CompletionCollector<'a> {
        return CompletionCollector {
            cursor: params.position,
            file: &params.text_document.uri,
            document,
            trigger,
            completions: Vec::new(),
        };
    }

    fn search_completions_in_document(&mut self) {
        return match self.document.tree.node_at(self.cursor) {
            Some(Node::Tag(ParsedTag::Valid(tag))) => self.search_completions_in_tag(tag),
            Some(Node::Tag(ParsedTag::Erroneous(tag, errors))) => {
                for error in errors {
                    match error {
                        TagError::Superfluous(_, location) if location.contains(&self.cursor) => {
                            return self.complete_attributes_of(tag);
                        }
                        TagError::Missing(text, _) if self.trigger.completes(Completable::Tag) => {
                            self.completions.push(CompletionItem {
                                detail: None,
                                documentation: None,
                                insert_text: Some(text.to_string()),
                                kind: Some(CompletionItemKind::SNIPPET),
                                ..Default::default()
                            });
                        }
                        err => log::debug!("unhandled tag error {:?} in {:?}", err, tag),
                    }
                }
                self.search_completions_in_tag(tag);
                // "/>" e.g. is always added as missing right after the last non-whitespace
                // character, terminating it's parent node. that means that our cursor can never be
                // placed after all attributes and still be considered inside the tag node. the
                // only case we could ever trigger this is by moving the cursor in front of an
                // attribute - where it could be arguably okay to suggest "/>" - but that is not
                // the intended usecase here.
                // in order to change this we would have to instead check the closest node before
                // the cursor for missing operators (maybe even skipping text nodes?) in addition
                // to what we're completing originally...
                //
                // if let Some(Node::Tag(ParsedTag::Erroneous(tag, errors))) =
                //     self.document.tree.closest_node_prior_to(self.cursor)
                // {
                //     for error in errors {
                //         if let TagError::Missing(text, _) = error {
                //             self.completions.push(CompletionItem {
                //                 label: format!(
                //                     "close {} tag with \"{}\"",
                //                     tag.definition().name,
                //                     text
                //                 ),
                //                 kind: Some(CompletionItemKind::OPERATOR), // ?
                //                 insert_text: Some(text.to_string()),
                //                 ..Default::default()
                //             })
                //         }
                //     }
                // }
            }
            Some(Node::Tag(ParsedTag::Unparsable(text, location)))
                if self.trigger.completes(Completable::Tag) =>
            {
                return self.completions.push(CompletionItem {
                    label: format!("unparsable \"{}\"", text),
                    kind: Some(CompletionItemKind::TEXT),
                    text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                        new_text: text.to_string(),
                        range: location.range(),
                    })),
                    ..Default::default()
                });
            }
            Some(mut node @ Node::Error(ErrorNode { content, range })) => {
                let mut closest_tag = None;
                loop {
                    (node, closest_tag) = match self.document.tree.parent_of(&node) {
                        Some(parent @ Node::Tag(ParsedTag::Valid(tag))) => (parent, Some(tag)),
                        Some(parent @ Node::Tag(ParsedTag::Erroneous(tag, errors))) => {
                            let new_text = match self.trigger.completes(Completable::Tag) {
                                true => errors.iter().find_map(|error| match error {
                                    TagError::Missing(text, _) if text.starts_with(&**content) => {
                                        Some(text)
                                    }
                                    _ => None,
                                }),
                                false => None,
                            };
                            if let Some(new_text) = new_text {
                                return self.completions.push(CompletionItem {
                                    label: new_text.to_string(),
                                    kind: Some(CompletionItemKind::KEYWORD),
                                    insert_text: Some(new_text.to_string()),
                                    text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                                        new_text: new_text.to_string(),
                                        range: *range,
                                    })),
                                    ..Default::default()
                                });
                            }
                            (parent, Some(tag))
                        }
                        Some(parent) => (parent, None),
                        None => break,
                    };
                }
                if let Some(tag) = closest_tag {
                    self.complete_attributes_of(tag);
                } else {
                    self.complete_top_level_tags();
                }
            }
            _ => self.complete_top_level_tags(),
        };
    }

    fn complete_top_level_tags(&mut self) {
        self.complete_tags(grammar::TOP_LEVEL_TAGS.iter());
    }

    fn complete_tags<'a>(&mut self, tags: impl Iterator<Item = &'a TagDefinition>) {
        let range = match self.tag_name_range() {
            Some(range) => range,
            None => return,
        };
        tags.map(|tag| Self::tag_to_completion(tag, range))
            .for_each(|completion| self.completions.push(completion));
    }

    fn complete_tag<'a>(&mut self, tag: &TagDefinition) {
        if let Some(range) = self.tag_name_range() {
            self.completions.push(Self::tag_to_completion(tag, range));
        }
    }

    /// The range a completed tag name replaces, unless the cursor is nowhere a
    /// tag name can be completed.
    fn tag_name_range(&self) -> Option<Range> {
        if !self.trigger.completes(Completable::Tag) {
            return None;
        }
        let range = self.determine_tag_range();
        // a tag name being typed reaches back to the "<" that opens it. prose
        // does not, which is how a ":" in a sentence is told apart from the
        // namespace separator in "<sp:".
        if self.trigger != Trigger::Invoked && range.start.character == self.cursor.character {
            return None;
        }
        return Some(range);
    }

    fn determine_tag_range(&self) -> Range {
        let line = self.line_up_to_cursor();
        let mut start = self.cursor.character;
        for (i, c) in line.chars().rev().enumerate() {
            match c {
                'a'..='z' | 'A'..='Z' | '0'..='9' | ':' | '_' | '-' => continue,
                '<' => start = start.saturating_sub((i as u32) + 1),
                _ => (),
            }
            break;
        }
        return Range {
            start: Position {
                line: self.cursor.line as u32,
                character: start,
            },
            end: Position {
                line: self.cursor.line as u32,
                character: self.cursor.character as u32,
            },
        };
    }

    /// The line before the cursor. Like the rest of the server, this reads the
    /// cursor character as a byte offset, which a client that counts utf-16
    /// code units does not send - converting it is #9. Until then the offset is
    /// clamped to a character boundary, so that a cursor behind a multi byte
    /// character truncates the line instead of panicking.
    fn line_up_to_cursor(&self) -> &str {
        let line = match self.document.text.lines().nth(self.cursor.line as usize) {
            Some(line) => line,
            None => return "",
        };
        let mut end = (self.cursor.character as usize).min(line.len());
        while end > 0 && !line.is_char_boundary(end) {
            end -= 1;
        }
        return &line[..end];
    }

    fn tag_to_completion(tag: &TagDefinition, range: Range) -> CompletionItem {
        let new_text = format!("<{}", tag.name);
        return CompletionItem {
            label: new_text.clone(),
            kind: Some(CompletionItemKind::KEYWORD),
            detail: tag.detail.map(|detail| detail.to_string()),
            documentation: tag.documentation.map(|detail| {
                Documentation::MarkupContent(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: detail.to_string(),
                })
            }),
            text_edit: Some(CompletionTextEdit::Edit(TextEdit { new_text, range })),
            ..Default::default()
        };
    }

    fn search_completions_in_tag(&mut self, tag: &SpmlTag) {
        if let Some(body) = tag.body() {
            if self.is_cursor_in_body(body) {
                return self.complete_children_of(tag);
            }
        }
        for (name, attribute) in &tag.spel_attributes() {
            let attribute = match attribute {
                ParsedAttribute::Valid(attribute) => attribute,
                ParsedAttribute::Erroneous(attribute, _) => attribute,
                ParsedAttribute::Unparsable(_, _) => continue,
            };
            if attribute.value.is_inside(&self.cursor) {
                if !self.trigger.completes(Completable::AttributeValue) {
                    return;
                }
                let definition = tag.definition();
                let attribute_type = name
                    .strip_suffix("_attribute")
                    .and_then(|name| definition.attributes.get_by_name(name));
                return match &attribute.value.spel {
                    // SpelAst::Comparable(_) => (),
                    // SpelAst::Condition(_) => (),
                    // SpelAst::Expression(_) => (),
                    // SpelAst::Identifier(_) => (),
                    // SpelAst::Object(_) => (),
                    // SpelAst::Query(_) => (),
                    // SpelAst::Regex(_) => (),
                    SpelAst::String(_)
                        if attribute_type
                            .is_some_and(|a| matches!(&a.r#type, TagAttributeType::Module)) =>
                    {
                        modules::all_modules().iter().for_each(|(name, _)| {
                            self.completions.push(CompletionItem {
                                label: name.to_owned(),
                                kind: Some(CompletionItemKind::MODULE),
                                insert_text: Some(name.to_owned()),
                                ..Default::default()
                            })
                        });
                    }
                    SpelAst::String(_) => {
                        let name = name.strip_suffix("_attribute").unwrap_or(name);
                        for (_, values) in tag
                            .definition()
                            .attribute_rules
                            .iter()
                            .filter_map(AttributeRule::value_domain)
                            .filter(|(attribute_name, _)| attribute_name == &name)
                        {
                            values.iter().for_each(|value| {
                                self.completions.push(CompletionItem {
                                    label: value.to_string(),
                                    kind: Some(CompletionItemKind::ENUM_MEMBER),
                                    detail: None,
                                    documentation: None,
                                    ..Default::default()
                                })
                            });
                        }
                    }
                    SpelAst::Uri(SpelResult::Valid(uri)) => {
                        let mut module = None;
                        if let Some(TagAttribute {
                            r#type: TagAttributeType::Uri { module_attribute },
                            ..
                        }) = attribute_type
                        {
                            let module_attribute = match tag.spel_attribute(&module_attribute) {
                                Some(ParsedAttribute::Valid(attribute)) => Some(attribute),
                                Some(ParsedAttribute::Erroneous(attribute, _)) => Some(attribute),
                                _ => None,
                            };
                            if let Some(SpelAttribute {
                                value:
                                    SpelAttributeValue {
                                        spel: SpelAst::String(SpelResult::Valid(Word { fragments })),
                                        ..
                                    },
                                ..
                            }) = module_attribute
                            {
                                if fragments.len() == 1 {
                                    if let WordFragment::String(StringLiteral { content, .. }) =
                                        &fragments[0]
                                    {
                                        module = modules::find_module_by_name(content);
                                    }
                                }
                            }
                        }
                        let module = module.or_else(|| {
                            modules::find_module_for_file(std::path::Path::new(
                                self.file.path().as_str(),
                            ))
                        });
                        if let Some(module) = &module {
                            if let Err(err) = self.complete_uri(uri, module) {
                                log::error!("failed to complete uri: {}", err);
                            }
                        }
                    }
                    _ => (),
                };
            }
        }
        return self.complete_attributes_of(tag);
    }

    fn is_cursor_in_body(&self, body: &TagBody) -> bool {
        if (self.cursor.line as usize) < body.open_location.line {
            return false;
        }
        if (self.cursor.line as usize) == body.open_location.line {
            if (self.cursor.character as usize) <= body.open_location.char {
                return false;
            }
        }
        return true;
    }

    fn complete_uri(&mut self, uri: &Uri, module: &modules::Module) -> Result<()> {
        let path = uri.to_string();
        for entry in fs::read_dir(module.path.clone() + path.as_str())? {
            let entry = entry?;
            let file_name = entry.file_name();
            let file_name = match file_name.to_str() {
                Some(file_name) => file_name,
                None => continue,
            };
            let name;
            if path.len() == 0 {
                name = "/".to_string() + file_name;
            } else {
                name = file_name.to_string();
            }
            if entry.path().is_dir() {
                self.completions.push(CompletionItem {
                    label: name.clone() + "/",
                    kind: Some(CompletionItemKind::FOLDER),
                    insert_text: Some(name + "/"),
                    ..Default::default()
                })
            } else if name.ends_with(".spml") {
                self.completions.push(CompletionItem {
                    label: name.clone(),
                    kind: Some(CompletionItemKind::FILE),
                    insert_text: Some(name),
                    ..Default::default()
                })
            }
        }
        return Ok(());
    }

    fn complete_attributes_of(&mut self, tag: &SpmlTag) {
        if !self.trigger.completes(Completable::AttributeName) {
            return;
        }
        if let TagAttributes::These(possible) = tag.definition().attributes {
            possible
                .iter()
                .filter(|attribute| tag.spel_attribute(attribute.name).is_none())
                .map(|attribute| CompletionItem {
                    label: attribute.name.to_string(),
                    kind: Some(CompletionItemKind::PROPERTY),
                    detail: attribute.detail.map(|detail| detail.to_string()),
                    documentation: attribute.documentation.map(|detail| {
                        Documentation::MarkupContent(MarkupContent {
                            kind: MarkupKind::Markdown,
                            value: detail.to_string(),
                        })
                    }),
                    insert_text: Some(attribute.name.to_string() + "=\""),
                    ..Default::default()
                })
                .for_each(|completion| self.completions.push(completion));
        };
    }

    fn complete_children_of(&mut self, tag: &SpmlTag) {
        match tag.definition().children {
            TagChildren::Any => self.complete_top_level_tags(),
            TagChildren::None => (),
            TagChildren::Scalar(tag) => self.complete_tag(tag),
            TagChildren::Vector(tags) => self.complete_tags(tags.iter()),
        };
    }
}

pub(crate) fn complete(params: CompletionParams) -> Result<Vec<CompletionItem>, LsError> {
    let text_params = params.text_document_position;
    let uri = &text_params.text_document.uri;
    let document = match document_store::get(uri) {
        Some(document) => document,
        None => match document_store::Document::from_uri(uri) {
            Ok(document) => document_store::put(uri, document),
            // completion fires on every trigger character the developer types,
            // so a document we cannot read must not raise an error each time.
            Err(err) => {
                log::error!("failed to read {:?}: {}", uri, err);
                return Ok(Vec::new());
            }
        },
    };
    let mut completion_collector = CompletionCollector::new(
        &text_params,
        &document,
        Trigger::of(params.context.as_ref()),
    );
    completion_collector.search_completions_in_document();
    return Ok(completion_collector.completions);
}

#[cfg(test)]
mod tests {
    use lsp_types::{Position, TextDocumentIdentifier, TextDocumentPositionParams, Uri as Url};

    use super::{Completable, CompletionCollector, Trigger};

    const HEADER: &str = "<%@ page language=\"java\" pageEncoding=\"UTF-8\"%>\n";

    #[test]
    fn test_every_declared_trigger_character_completes_something() {
        for character in capabilities::COMPLETION_TRIGGER_CHARACTERS {
            let trigger = Trigger::Character(character.chars().next().expect("empty trigger"));
            assert!(
                [
                    Completable::Tag,
                    Completable::AttributeName,
                    Completable::AttributeValue,
                ]
                .iter()
                .any(|completable| trigger.completes(*completable)),
                "{:?} is declared as a trigger but completes nothing",
                trigger
            );
        }
    }

    #[test]
    fn test_opening_bracket_completes_tags() {
        assert!(
            completions_at(&format!("{}<|", HEADER), Trigger::Character('<'))
                .contains(&"<sp:if".to_string())
        );
    }

    #[test]
    fn test_namespace_separator_completes_tags() {
        assert!(
            completions_at(&format!("{}<sp:|", HEADER), Trigger::Character(':'))
                .contains(&"<sp:if".to_string())
        );
    }

    #[test]
    fn test_opening_quote_completes_attribute_values() {
        assert_eq!(
            completions_at(
                &format!(
                    "{}<sp:calendarsheet name=\"sheet\" action=\"new\" scope=\"|\"/>",
                    HEADER
                ),
                Trigger::Character('"')
            ),
            vec!["page", "request", "session"]
        );
    }

    #[test]
    fn test_trigger_characters_complete_nothing_in_prose() {
        for marked_text in [
            "Größe:| 3,5 m²",
            "3,5 m².| Siehe",
            "siehe http:/|/example.com",
            "Preis $|5",
            "er sagte \"|hallo\"",
        ] {
            let trigger = Trigger::Character(
                marked_text
                    .split_once('|')
                    .and_then(|(before, _)| before.chars().last())
                    .expect("no cursor marker"),
            );
            let text = format!(
                "{}<sp:if condition=\"${{_x}}\">\n{}\n</sp:if>",
                HEADER, marked_text
            );
            assert_eq!(
                completions_at(&text, trigger),
                Vec::<String>::new(),
                "{:?} triggered by {:?}",
                marked_text,
                trigger
            );
        }
    }

    #[test]
    fn test_invoked_completion_still_completes_tags_in_prose() {
        let text = format!(
            "{}<sp:if condition=\"${{_x}}\">\nGröße:| 3,5 m²\n</sp:if>",
            HEADER
        );
        assert!(completions_at(&text, Trigger::Invoked).contains(&"<sp:if".to_string()));
    }

    #[test]
    fn test_expression_and_path_triggers_reach_attribute_values() {
        // spel completion is not implemented yet, so an object member or an
        // expression variable completes nothing of its own. what this pins down
        // is that the trigger characters reach an attribute value at all.
        for character in ['$', '.', '/'] {
            assert_eq!(
                completions_at(
                    &format!(
                        "{}<sp:calendarsheet name=\"sheet\" action=\"new\" scope=\"|\"/>",
                        HEADER
                    ),
                    Trigger::Character(character)
                ),
                vec!["page", "request", "session"],
                "triggered by {:?}",
                character
            );
        }
    }

    #[test]
    fn test_value_trigger_does_not_complete_attribute_names() {
        let text = format!("{}<sp:include module=\"test\" .|/>", HEADER);
        assert_eq!(
            completions_at(&text, Trigger::Character('.')),
            Vec::<String>::new()
        );
    }

    #[test]
    fn test_completion_inside_a_tag_completes_attribute_names() {
        let text = format!("{}<sp:include module=\"test\" |/>", HEADER);
        assert_eq!(
            completions_at(&text, Trigger::Invoked),
            vec![
                "anchor",
                "arguments",
                "context",
                "mode",
                "return",
                "template",
                "uri"
            ]
        );
    }

    #[test]
    fn test_completion_within_a_multi_byte_character() {
        // read as a byte offset, the cursor sits inside the two bytes of "ö".
        // clients count characters, so they do send such an offset.
        let text = format!("{}<sp:if condition=\"${{_x}}\">\nGrö|ße\n</sp:if>", HEADER);
        assert!(completions_at(&text, Trigger::Invoked).contains(&"<sp:if".to_string()));
    }

    #[test]
    fn test_completion_of_an_unreadable_document_is_empty() {
        let _ = modules::init_empty_module_mappings();
        let params = lsp_types::CompletionParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier {
                    uri: "file:///does/not/exist.spml".parse::<Url>().unwrap(),
                },
                position: Position {
                    line: 0,
                    character: 0,
                },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
            context: None,
        };
        assert_eq!(super::complete(params).map(|c| c.len()).ok(), Some(0));
    }

    fn completions_at(marked_text: &str, trigger: Trigger) -> Vec<String> {
        let _ = modules::init_empty_module_mappings();
        let (text, cursor) = split_cursor_marker(marked_text);
        let document = document_store::Document::new(text).expect("failed to parse document");
        let params = TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: "file:///some/test/file.spml".parse::<Url>().unwrap(),
            },
            position: cursor,
        };
        let mut collector = CompletionCollector::new(&params, &document, trigger);
        collector.search_completions_in_document();
        return collector
            .completions
            .iter()
            .map(|completion| completion.label.clone())
            .collect();
    }

    /// Splits a document that marks the cursor with a "|" into its text and the
    /// cursor position, counted in utf-16 code units as a client sends it.
    fn split_cursor_marker(marked_text: &str) -> (String, Position) {
        let (before, after) = marked_text.split_once('|').expect("no cursor marker");
        return (
            before.to_string() + after,
            Position {
                line: before.matches('\n').count() as u32,
                character: before
                    .rsplit('\n')
                    .next()
                    .unwrap_or(before)
                    .encode_utf16()
                    .count() as u32,
            },
        );
    }
}

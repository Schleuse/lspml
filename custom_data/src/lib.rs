//! Html custom data for vscode's html language server, generated from the tag
//! grammar so that it cannot drift from it.

use anyhow::Result;
use grammar::{AttributeRule, TagAttributes, TagDefinition};
use serde::Serialize;

const SCHEMA: &str = "https://raw.githubusercontent.com/microsoft/vscode-html-languageservice/main/docs/customData.schema.json";

/// The version of the custom data format, not of this crate.
const VERSION: f32 = 1.1;

#[derive(Debug, Serialize)]
struct HtmlCustomData {
    #[serde(rename = "$schema")]
    schema: &'static str,
    version: f32,
    tags: Vec<Tag>,
}

/// Deliberately without a `description`: the server answers hover itself, and
/// a second copy here would render as a differently formatted tooltip.
#[derive(Debug, Serialize)]
struct Tag {
    name: &'static str,
    void: bool,
    attributes: Vec<Attribute>,
}

#[derive(Debug, Serialize)]
struct Attribute {
    name: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    values: Option<Vec<AttributeValue>>,
}

#[derive(Debug, Serialize)]
struct AttributeValue {
    name: &'static str,
}

/// Renders the custom data for the whole tag grammar as json.
///
/// Tab indented and newline terminated, which is what the extension
/// repository's formatter expects of the file this ends up in.
pub fn html_custom_data() -> Result<String> {
    let mut buffer = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(b"\t");
    let mut serializer = serde_json::Serializer::with_formatter(&mut buffer, formatter);
    generate().serialize(&mut serializer)?;
    let mut json = String::from_utf8(buffer)?;
    json.push('\n');
    return Ok(json);
}

fn generate() -> HtmlCustomData {
    let mut tags = grammar::all_tags()
        .into_iter()
        .map(tag)
        .collect::<Vec<Tag>>();
    // Sorted so that a regenerated file stays diffable against the committed
    // one no matter what order the grammar is walked in.
    tags.sort_by_key(|tag| tag.name);
    return HtmlCustomData {
        schema: SCHEMA,
        version: VERSION,
        tags,
    };
}

fn tag(definition: &TagDefinition) -> Tag {
    return Tag {
        name: definition.name,
        void: !definition.permits_body(),
        attributes: attributes(definition),
    };
}

fn attributes(definition: &TagDefinition) -> Vec<Attribute> {
    return match definition.attributes {
        TagAttributes::None => Vec::new(),
        TagAttributes::These(attributes) => attributes
            .iter()
            .map(|attribute| Attribute {
                name: attribute.name,
                values: values(definition, attribute.name),
            })
            .collect(),
    };
}

fn values(definition: &TagDefinition, attribute: &str) -> Option<Vec<AttributeValue>> {
    let values = definition
        .attribute_rules
        .iter()
        .filter_map(AttributeRule::value_domain)
        .filter(|(name, _)| *name == attribute)
        .flat_map(|(_, domain)| domain.iter().map(|name| AttributeValue { name }))
        .collect::<Vec<AttributeValue>>();
    return match values.is_empty() {
        true => None,
        false => Some(values),
    };
}

#[cfg(test)]
mod tests {
    use grammar::{TagAttributes, TagChildren, TagDefinition, all_tags};
    use serde_json::Value;

    use super::html_custom_data;

    fn generated() -> Value {
        return serde_json::from_str(&html_custom_data().unwrap()).expect("emitted json");
    }

    fn tag<'a>(data: &'a Value, name: &str) -> &'a Value {
        return data["tags"]
            .as_array()
            .expect("tags is an array")
            .iter()
            .find(|tag| tag["name"] == name)
            .unwrap_or_else(|| panic!("{} is emitted", name));
    }

    fn attribute<'a>(data: &'a Value, tag_name: &str, name: &str) -> &'a Value {
        return tag(data, tag_name)["attributes"]
            .as_array()
            .expect("attributes is an array")
            .iter()
            .find(|attribute| attribute["name"] == name)
            .unwrap_or_else(|| panic!("{} has a {} attribute", tag_name, name));
    }

    /// The bug this generator exists to prevent: the hand written file marked
    /// sp:if, sp:for, sp:set and sp:include void, so the html language server
    /// stopped folding and auto closing the tags that give an spml file its
    /// shape. Asserted over the whole tag set rather than those four, so that
    /// a tag added to the grammar later is covered without anyone remembering.
    #[test]
    fn test_no_tag_that_accepts_children_is_emitted_as_void() {
        let data = generated();
        for definition in all_tags() {
            if matches!(definition.children, TagChildren::None) {
                continue;
            }
            assert_eq!(
                tag(&data, definition.name)["void"],
                Value::Bool(false),
                "{} accepts children",
                definition.name
            );
        }
    }

    /// A tag can take body text without taking child tags, which is just as
    /// much a reason not to be void.
    #[test]
    fn test_no_tag_that_takes_a_body_is_emitted_as_void() {
        let data = generated();
        for definition in all_tags() {
            if !definition.permits_body() {
                continue;
            }
            assert_eq!(
                tag(&data, definition.name)["void"],
                Value::Bool(false),
                "{} takes a body",
                definition.name
            );
        }
    }

    #[test]
    fn test_a_tag_that_takes_neither_children_nor_a_body_is_emitted_as_void() {
        assert_eq!(tag(&generated(), "sp:break")["void"], Value::Bool(true));
    }

    #[test]
    fn test_every_tag_in_the_grammar_is_emitted() {
        let data = generated();
        assert_eq!(
            data["tags"].as_array().expect("tags is an array").len(),
            all_tags().len()
        );
        for definition in all_tags() {
            tag(&data, definition.name);
        }
    }

    #[test]
    fn test_tags_are_emitted_in_a_stable_order() {
        let data = generated();
        let names = data["tags"]
            .as_array()
            .expect("tags is an array")
            .iter()
            .map(|tag| tag["name"].as_str().expect("a tag has a name").to_owned())
            .collect::<Vec<String>>();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
    }

    #[test]
    fn test_a_tag_carries_the_attributes_of_its_definition() {
        let data = generated();
        for definition in all_tags() {
            let expected = match definition.attributes {
                TagAttributes::None => vec![],
                TagAttributes::These(attributes) => {
                    attributes.iter().map(|a| a.name).collect::<Vec<&str>>()
                }
            };
            let emitted = tag(&data, definition.name)["attributes"]
                .as_array()
                .unwrap_or_else(|| panic!("{} has attributes", definition.name))
                .iter()
                .map(|a| a["name"].as_str().expect("an attribute has a name"))
                .collect::<Vec<&str>>();
            assert_eq!(emitted, expected, "attributes of {}", definition.name);
        }
    }

    /// sp:barcode restricts scope to page, request or session.
    #[test]
    fn test_an_attribute_restricted_to_a_set_of_values_carries_them() {
        let data = generated();
        let values = attribute(&data, "sp:barcode", "scope")["values"]
            .as_array()
            .expect("scope carries its values")
            .iter()
            .map(|value| value["name"].as_str().expect("a value has a name"))
            .collect::<Vec<&str>>();
        assert_eq!(values, vec!["page", "request", "session"]);
    }

    /// sp:map requires `object` when `action` is `merge`, which says nothing
    /// about the values `object` itself may take. Reading that rule as a
    /// domain would leave completion offering `merge` as the one legal object.
    #[test]
    fn test_an_attribute_required_by_another_attributes_value_carries_no_values() {
        let data = generated();
        assert_eq!(attribute(&data, "sp:map", "object")["values"], Value::Null);
    }

    #[test]
    fn test_the_attribute_that_conditions_others_still_carries_its_own_domain() {
        let data = generated();
        let values = attribute(&data, "sp:map", "action")["values"]
            .as_array()
            .expect("action carries its values")
            .iter()
            .map(|value| value["name"].as_str().expect("a value has a name"))
            .collect::<Vec<&str>>();
        assert!(values.contains(&"merge"), "{:?}", values);
        assert!(values.contains(&"clear"), "{:?}", values);
    }

    #[test]
    fn test_an_unrestricted_attribute_carries_no_values() {
        let data = generated();
        assert_eq!(
            attribute(&data, "sp:if", "condition")["values"],
            Value::Null
        );
    }

    #[test]
    fn test_no_documentation_is_emitted() {
        let json = html_custom_data().unwrap();
        assert!(!json.contains("description"));
        let documented = all_tags()
            .iter()
            .filter_map(|tag: &&TagDefinition| tag.documentation)
            .find(|documentation| !documentation.trim().is_empty())
            .expect("at least one tag is documented");
        assert!(!json.contains(documented.trim()));
    }

    #[test]
    fn test_the_emitted_json_is_tab_indented_and_ends_in_a_newline() {
        let json = html_custom_data().unwrap();
        assert!(json.contains("\n\t\"tags\": ["));
        assert!(json.ends_with("}\n"));
    }
}

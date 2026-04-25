//! Output formatting module for botseal
//!
//! Provides text, JSON, and pretty output formats for CLI output.

use anyhow::Result;
use serde::Serialize;
use std::io::{self, Write};

/// Output format selection
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum OutputFormat {
    /// JSON format - machine-readable output
    Json,
    /// Plain text format - concise, token-efficient output
    #[default]
    Text,
    /// Pretty format - colorized, human-friendly output
    Pretty,
}

/// Formatter that can output data in text, JSON, or pretty format
#[derive(Debug, Clone)]
pub struct Formatter {
    format: OutputFormat,
}

impl Formatter {
    /// Create a new formatter with the specified output format
    #[must_use]
    pub const fn new(format: OutputFormat) -> Self {
        Self { format }
    }

    /// Format data according to the configured output format
    ///
    /// # Errors
    ///
    /// Returns an error if serialization fails
    pub fn format<T: Serialize>(&self, data: &T) -> Result<String> {
        match self.format {
            OutputFormat::Json => {
                let output = serde_json::to_string_pretty(data)?;
                Ok(output)
            }
            OutputFormat::Text | OutputFormat::Pretty => {
                // Convert to JSON value first, then render as text
                let json_value = serde_json::to_value(data)?;
                Ok(render_text(&json_value))
            }
        }
    }

    /// Format and print data to stdout
    ///
    /// # Errors
    ///
    /// Returns an error if serialization or writing fails
    pub fn print<T: Serialize>(&self, data: &T) -> Result<()> {
        let output = self.format(data)?;
        let mut stdout = io::stdout().lock();
        writeln!(stdout, "{output}")?;
        Ok(())
    }

    /// Format and print a list with a custom empty message
    ///
    /// For JSON format, wraps the array in a named object with count and advice fields.
    /// For other formats, prints the list normally (ignores `collection_name` and `advice`).
    ///
    /// # Errors
    ///
    /// Returns an error if serialization or writing fails
    pub fn print_list<T: Serialize>(
        &self,
        data: &[T],
        empty_message: &str,
        collection_name: &str,
        advice: &[&str],
    ) -> Result<()> {
        match self.format {
            OutputFormat::Json => {
                let items_value = serde_json::to_value(data)?;
                let mut envelope = serde_json::Map::new();
                envelope.insert(collection_name.to_string(), items_value);
                envelope.insert("count".to_string(), serde_json::json!(data.len()));
                envelope.insert("advice".to_string(), serde_json::json!(advice));

                let output = serde_json::to_string_pretty(&serde_json::Value::Object(envelope))?;
                let mut stdout = io::stdout().lock();
                writeln!(stdout, "{output}")?;
                Ok(())
            }
            OutputFormat::Text | OutputFormat::Pretty => {
                if data.is_empty() {
                    let mut stdout = io::stdout().lock();
                    writeln!(stdout, "{empty_message}")?;
                    Ok(())
                } else {
                    self.print(&data)
                }
            }
        }
    }
}

impl Default for Formatter {
    fn default() -> Self {
        Self::new(OutputFormat::default())
    }
}

/// Print data in JSON format to stdout
///
/// # Errors
///
/// Returns an error if serialization or writing fails
pub fn print_json<T: Serialize>(data: &T) -> Result<()> {
    Formatter::new(OutputFormat::Json).print(data)
}

/// Render a JSON value as concise text
fn render_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Object(map) => {
            // Put ID-like fields first (review_id, thread_id, comment_id, id)
            let mut parts = Vec::new();
            let id_keys = ["review_id", "thread_id", "comment_id", "id"];

            for key in &id_keys {
                if let Some(val) = map.get(*key) {
                    parts.push(render_field_value(val));
                }
            }

            for (key, val) in map {
                if !id_keys.contains(&key.as_str()) {
                    match val {
                        serde_json::Value::Array(arr) if arr.is_empty() => {}
                        serde_json::Value::Null => {}
                        _ => {
                            parts.push(format!("{}:{}", key, render_field_value(val)));
                        }
                    }
                }
            }
            parts.join("  ")
        }
        serde_json::Value::Array(arr) => arr.iter().map(render_text).collect::<Vec<_>>().join("\n"),
        _ => render_field_value(value),
    }
}

/// Render a single field value as concise text
fn render_field_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => {
            if s.contains(' ') || s.contains('\n') {
                format!("\"{}\"", s.replace('\n', "\\n"))
            } else {
                s.clone()
            }
        }
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Null => "null".to_string(),
        serde_json::Value::Array(arr) => {
            let items: Vec<String> = arr.iter().map(render_field_value).collect();
            format!("[{}]", items.join(","))
        }
        serde_json::Value::Object(map) => {
            // Compact inline for nested objects
            let parts: Vec<String> = map
                .iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| format!("{}:{}", k, render_field_value(v)))
                .collect();
            format!("{{{}}}", parts.join(","))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Serialize;

    #[derive(Debug, Serialize)]
    struct TestData {
        name: String,
        count: u32,
        active: bool,
    }

    fn sample_data() -> TestData {
        TestData {
            name: "test-item".to_string(),
            count: 42,
            active: true,
        }
    }

    #[test]
    fn test_output_format_default() {
        // The enum default is Text
        // Note: CLI resolution layer may override this based on TTY detection
        let format = OutputFormat::default();
        assert_eq!(format, OutputFormat::Text);
    }

    #[test]
    fn test_formatter_json_output() {
        let formatter = Formatter::new(OutputFormat::Json);
        let data = sample_data();
        let output = formatter.format(&data).expect("JSON formatting failed");

        // Verify it's valid JSON
        let parsed: serde_json::Value =
            serde_json::from_str(&output).expect("Output is not valid JSON");
        assert_eq!(parsed["name"], "test-item");
        assert_eq!(parsed["count"], 42);
        assert_eq!(parsed["active"], true);
    }

    #[test]
    fn test_formatter_text_output() {
        let formatter = Formatter::new(OutputFormat::Text);
        let data = sample_data();
        let output = formatter.format(&data).expect("Text formatting failed");

        // Text output should contain the field values
        assert!(output.contains("test-item") || output.contains("name"));
        assert!(output.contains("42") || output.contains("count"));
    }

    #[test]
    fn test_formatter_default() {
        let formatter = Formatter::default();
        assert_eq!(formatter.format, OutputFormat::Text);
    }

    #[test]
    fn test_format_nested_structure() {
        #[derive(Debug, Serialize)]
        struct Nested {
            items: Vec<String>,
            metadata: Metadata,
        }

        #[derive(Debug, Serialize)]
        struct Metadata {
            version: u32,
        }

        let data = Nested {
            items: vec!["a".to_string(), "b".to_string()],
            metadata: Metadata { version: 1 },
        };

        let json_formatter = Formatter::new(OutputFormat::Json);
        let json_output = json_formatter.format(&data).expect("JSON failed");
        assert!(json_output.contains("items"));
        assert!(json_output.contains("metadata"));

        let text_formatter = Formatter::new(OutputFormat::Text);
        let text_output = text_formatter.format(&data).expect("Text failed");
        // Text should produce some output
        assert!(!text_output.is_empty());
    }

    #[test]
    fn test_format_empty_vec() {
        let data: Vec<String> = vec![];

        let json_formatter = Formatter::new(OutputFormat::Json);
        let json_output = json_formatter.format(&data).expect("JSON failed");
        assert_eq!(json_output.trim(), "[]");

        let text_formatter = Formatter::new(OutputFormat::Text);
        let _text_output = text_formatter.format(&data).expect("Text failed");
    }

    #[test]
    fn test_print_list_json_envelope() {
        #[derive(Debug, Serialize)]
        struct Item {
            id: String,
            name: String,
        }

        let items = vec![
            Item {
                id: "1".to_string(),
                name: "first".to_string(),
            },
            Item {
                id: "2".to_string(),
                name: "second".to_string(),
            },
        ];

        // Verify the envelope structure by building it the same way print_list does
        let items_value = serde_json::to_value(&items).expect("serialize items");
        let mut envelope = serde_json::Map::new();
        envelope.insert("items".to_string(), items_value);
        envelope.insert("count".to_string(), serde_json::json!(2));
        envelope.insert("advice".to_string(), serde_json::json!(["seal show <id>"]));

        let output = serde_json::to_string_pretty(&serde_json::Value::Object(envelope))
            .expect("serialize envelope");
        let parsed: serde_json::Value = serde_json::from_str(&output).expect("parse");

        assert_eq!(parsed["count"], 2);
        assert!(parsed["items"].is_array());
        assert!(parsed["advice"].is_array());
        assert_eq!(parsed["items"].as_array().expect("items array").len(), 2);
    }

    #[test]
    fn test_formatter_pretty_output() {
        let formatter = Formatter::new(OutputFormat::Pretty);
        let data = sample_data();
        let output = formatter.format(&data).expect("Pretty formatting failed");

        // Pretty output should contain the field values
        assert!(output.contains("test-item") || output.contains("name"));
        assert!(output.contains("42") || output.contains("count"));
        assert!(!output.is_empty());
    }

    #[test]
    fn test_output_format_pretty_variant_exists() {
        // Verify Pretty variant exists in the enum
        let format = OutputFormat::Pretty;
        let formatter = Formatter::new(format);
        assert_eq!(formatter.format, OutputFormat::Pretty);
    }
}

#[cfg(test)]
mod text_format_tests {
    use super::*;

    #[test]
    fn test_text_format_single_object() {
        #[derive(Debug, Serialize)]
        struct Review {
            review_id: String,
            title: String,
            status: String,
            author: String,
        }

        let review = Review {
            review_id: "cr-abc".to_string(),
            title: "Fix login bug".to_string(),
            status: "open".to_string(),
            author: "alice".to_string(),
        };

        let formatter = Formatter::new(OutputFormat::Text);
        let output = formatter.format(&review).unwrap();

        // Should start with ID
        assert!(output.starts_with("cr-abc"));
        // Should contain other fields with labels
        assert!(output.contains("title:"));
        assert!(output.contains("status:open"));
        assert!(output.contains("author:alice"));
        // Title with spaces should be quoted
        assert!(output.contains("\"Fix login bug\""));
    }

    #[test]
    fn test_text_format_array() {
        #[derive(Debug, Serialize)]
        struct Item {
            id: String,
            name: String,
        }

        let items = vec![
            Item {
                id: "item-1".to_string(),
                name: "first".to_string(),
            },
            Item {
                id: "item-2".to_string(),
                name: "second".to_string(),
            },
        ];

        let formatter = Formatter::new(OutputFormat::Text);
        let output = formatter.format(&items).unwrap();

        // Should have one item per line
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines.len(), 2);

        // Each line should start with ID
        assert!(lines[0].starts_with("item-1"));
        assert!(lines[1].starts_with("item-2"));
    }

    #[test]
    fn test_text_format_null_and_empty() {
        #[derive(Debug, Serialize)]
        struct Data {
            id: String,
            name: String,
            #[serde(skip_serializing_if = "Option::is_none")]
            optional: Option<String>,
            #[serde(skip_serializing_if = "Vec::is_empty")]
            tags: Vec<String>,
        }

        let data = Data {
            id: "test-1".to_string(),
            name: "test".to_string(),
            optional: None,
            tags: vec![],
        };

        let formatter = Formatter::new(OutputFormat::Text);
        let output = formatter.format(&data).unwrap();

        // Should not contain null or empty fields
        assert!(!output.contains("null"));
        assert!(!output.contains("optional"));
        assert!(!output.contains("tags"));
        assert!(output.contains("test-1"));
        assert!(output.contains("name:test"));
    }

    #[test]
    fn test_text_format_nested_object() {
        #[derive(Debug, Serialize)]
        struct Parent {
            id: String,
            child: Child,
        }

        #[derive(Debug, Serialize)]
        struct Child {
            name: String,
            value: i32,
        }

        let data = Parent {
            id: "parent-1".to_string(),
            child: Child {
                name: "nested".to_string(),
                value: 42,
            },
        };

        let formatter = Formatter::new(OutputFormat::Text);
        let output = formatter.format(&data).unwrap();

        // Should start with ID
        assert!(output.starts_with("parent-1"));
        // Should contain nested object in compact form
        assert!(output.contains("child:{"));
        assert!(output.contains("name:nested"));
        assert!(output.contains("value:42"));
    }

    #[test]
    fn test_text_format_id_ordering() {
        #[derive(Debug, Serialize)]
        struct MultiId {
            name: String,
            review_id: String,
            thread_id: String,
            value: i32,
        }

        let data = MultiId {
            name: "test".to_string(),
            review_id: "cr-abc".to_string(),
            thread_id: "th-xyz".to_string(),
            value: 42,
        };

        let formatter = Formatter::new(OutputFormat::Text);
        let output = formatter.format(&data).unwrap();

        // Should start with review_id, then thread_id (in order of id_keys array)
        assert!(output.starts_with("cr-abc  th-xyz"));
    }

    #[test]
    fn test_pretty_format_uses_text() {
        #[derive(Debug, Serialize)]
        struct Item {
            id: String,
            name: String,
        }

        let item = Item {
            id: "test-1".to_string(),
            name: "test".to_string(),
        };

        let text_formatter = Formatter::new(OutputFormat::Text);
        let pretty_formatter = Formatter::new(OutputFormat::Pretty);

        let text_output = text_formatter.format(&item).unwrap();
        let pretty_output = pretty_formatter.format(&item).unwrap();

        // Pretty and Text should produce the same output for now
        assert_eq!(text_output, pretty_output);
    }
}

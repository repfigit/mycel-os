//! Tool Parser - Extract tool calls from LLM responses
//!
//! Parses tool calls from various formats that LLMs might output:
//! - XML tags: `<tool_call>{"name": "...", "arguments": {...}}</tool_call>`
//! - JSON blocks: ```json\n{"tool_call": {"name": "...", ...}}```
//! - Function syntax: `tool_name({"arg": "value"})`
//! - Direct JSON with name/arguments fields

use anyhow::{anyhow, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A parsed tool call from an LLM response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub name: String,
    #[serde(default, alias = "args")]
    pub arguments: HashMap<String, serde_json::Value>,
}

/// Result of parsing an LLM response
#[derive(Debug, Clone)]
pub struct ParsedResponse {
    /// Text before any tool calls
    pub prefix_text: String,
    /// Extracted tool calls (if any)
    pub tool_calls: Vec<ToolCall>,
    /// Text after all tool calls
    #[allow(dead_code)]
    pub suffix_text: String,
    /// Which format was detected
    #[allow(dead_code)]
    pub format_detected: Option<ToolCallFormat>,
}

/// The format in which tool calls were detected
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCallFormat {
    XmlTags,          // <tool_call>...</tool_call>
    JsonCodeBlock,    // ```json ... ```
    FunctionSyntax,   // tool_name({...})
    DirectJson,       // {"name": "...", "arguments": {...}}
}

impl ParsedResponse {
    /// Check if this response contains any tool calls
    pub fn has_tool_calls(&self) -> bool {
        !self.tool_calls.is_empty()
    }

    /// Get the full text without tool call blocks
    #[allow(dead_code)]
    pub fn text_only(&self) -> String {
        let mut result = self.prefix_text.trim().to_string();
        if !self.suffix_text.is_empty() {
            if !result.is_empty() {
                result.push(' ');
            }
            result.push_str(self.suffix_text.trim());
        }
        result
    }
}

/// Parse tool calls from an LLM response, trying multiple formats
pub fn parse_tool_calls(response: &str) -> ParsedResponse {
    // Try formats in order of specificity

    // 1. XML tags (most explicit)
    if let Some(parsed) = try_parse_xml_tags(response) {
        if parsed.has_tool_calls() {
            return parsed;
        }
    }

    // 2. JSON code blocks
    if let Some(parsed) = try_parse_json_code_blocks(response) {
        if parsed.has_tool_calls() {
            return parsed;
        }
    }

    // 3. Function call syntax
    if let Some(parsed) = try_parse_function_syntax(response) {
        if parsed.has_tool_calls() {
            return parsed;
        }
    }

    // 4. Direct JSON in text
    if let Some(parsed) = try_parse_direct_json(response) {
        if parsed.has_tool_calls() {
            return parsed;
        }
    }

    // No tool calls found
    ParsedResponse {
        prefix_text: response.to_string(),
        tool_calls: Vec::new(),
        suffix_text: String::new(),
        format_detected: None,
    }
}

/// Parse XML-style tool call tags: <tool_call>...</tool_call>
fn try_parse_xml_tags(response: &str) -> Option<ParsedResponse> {
    let mut prefix_text = String::new();
    let mut tool_calls = Vec::new();
    let mut suffix_text = String::new();

    let mut remaining = response;
    let mut found_first_tool = false;

    // Support both <tool_call> and <function_call> tags
    let open_tags = ["<tool_call>", "<function_call>", "<tool>"];
    let close_tags = ["</tool_call>", "</function_call>", "</tool>"];

    'outer: while !remaining.is_empty() {
        // Find the earliest opening tag
        let mut earliest_start: Option<(usize, usize)> = None; // (position, tag_index)

        for (idx, tag) in open_tags.iter().enumerate() {
            if let Some(pos) = remaining.find(tag) {
                match earliest_start {
                    None => earliest_start = Some((pos, idx)),
                    Some((existing_pos, _)) if pos < existing_pos => {
                        earliest_start = Some((pos, idx));
                    }
                    _ => {}
                }
            }
        }

        let Some((start_idx, tag_idx)) = earliest_start else {
            break;
        };

        let open_tag = open_tags[tag_idx];
        let close_tag = close_tags[tag_idx];

        // Add text before this tool call
        let before = &remaining[..start_idx];
        if !found_first_tool {
            prefix_text.push_str(before);
            found_first_tool = true;
        } else {
            suffix_text.push_str(before);
        }

        // Find the closing tag
        let after_start = &remaining[start_idx + open_tag.len()..];
        if let Some(end_idx) = after_start.find(close_tag) {
            let tool_json = after_start[..end_idx].trim();

            // Try to parse the JSON
            if let Ok(call) = parse_tool_call_json(tool_json) {
                tool_calls.push(call);
            }

            remaining = &after_start[end_idx + close_tag.len()..];
        } else {
            // No closing tag - treat rest as text
            suffix_text.push_str(&remaining[start_idx..]);
            break 'outer;
        }
    }

    // Add any remaining text
    if !found_first_tool {
        return None; // No tags found at all
    }

    suffix_text.push_str(remaining);

    Some(ParsedResponse {
        prefix_text,
        tool_calls,
        suffix_text,
        format_detected: Some(ToolCallFormat::XmlTags),
    })
}

/// Parse JSON code blocks that contain tool calls
fn try_parse_json_code_blocks(response: &str) -> Option<ParsedResponse> {
    let re = Regex::new(r"```(?:json)?\s*\n?([\s\S]*?)```").ok()?;

    let mut tool_calls = Vec::new();
    let mut prefix_text = String::new();
    let mut last_end = 0;
    let mut found_any = false;

    for cap in re.captures_iter(response) {
        let full_match = cap.get(0)?;
        let json_content = cap.get(1)?.as_str().trim();

        // Try to parse as a tool call
        if let Ok(call) = parse_tool_call_json(json_content) {
            if !found_any {
                prefix_text = response[..full_match.start()].to_string();
                found_any = true;
            }
            tool_calls.push(call);
            last_end = full_match.end();
        } else if let Ok(wrapper) = serde_json::from_str::<serde_json::Value>(json_content) {
            // Check for wrapped format: {"tool_call": {...}} or {"function_call": {...}}
            let inner = wrapper.get("tool_call")
                .or_else(|| wrapper.get("function_call"))
                .or_else(|| wrapper.get("tool"));

            if let Some(inner_val) = inner {
                if let Ok(call) = serde_json::from_value::<ToolCall>(inner_val.clone()) {
                    if !found_any {
                        prefix_text = response[..full_match.start()].to_string();
                        found_any = true;
                    }
                    tool_calls.push(call);
                    last_end = full_match.end();
                }
            }
        }
    }

    if !found_any {
        return None;
    }

    Some(ParsedResponse {
        prefix_text,
        tool_calls,
        suffix_text: response[last_end..].to_string(),
        format_detected: Some(ToolCallFormat::JsonCodeBlock),
    })
}

/// Parse function-call syntax: tool_name({"arg": "value"})
fn try_parse_function_syntax(response: &str) -> Option<ParsedResponse> {
    // Match patterns like: tool_name({"key": "value"}) or tool_name({...})
    let re = Regex::new(r"([a-zA-Z_][a-zA-Z0-9_]*)\s*\(\s*(\{[\s\S]*?\})\s*\)").ok()?;

    let mut tool_calls = Vec::new();
    let mut prefix_text = String::new();
    let mut last_end = 0;
    let mut found_any = false;

    for cap in re.captures_iter(response) {
        let full_match = cap.get(0)?;
        let func_name = cap.get(1)?.as_str();
        let args_json = cap.get(2)?.as_str();

        // Skip common false positives
        if ["if", "while", "for", "function", "return", "var", "let", "const"].contains(&func_name) {
            continue;
        }

        // Try to parse the arguments as JSON
        if let Ok(args) = serde_json::from_str::<HashMap<String, serde_json::Value>>(args_json) {
            if !found_any {
                prefix_text = response[..full_match.start()].to_string();
                found_any = true;
            }
            tool_calls.push(ToolCall {
                name: func_name.to_string(),
                arguments: args,
            });
            last_end = full_match.end();
        }
    }

    if !found_any {
        return None;
    }

    Some(ParsedResponse {
        prefix_text,
        tool_calls,
        suffix_text: response[last_end..].to_string(),
        format_detected: Some(ToolCallFormat::FunctionSyntax),
    })
}

/// Parse direct JSON objects in the response that look like tool calls
fn try_parse_direct_json(response: &str) -> Option<ParsedResponse> {
    // Find balanced JSON objects that contain "name" field
    let mut tool_calls = Vec::new();
    let mut prefix_text = String::new();
    let mut found_any = false;
    let mut last_end = 0;

    // Find all potential JSON objects by tracking brace depth
    let chars: Vec<char> = response.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        if chars[i] == '{' {
            let start = i;
            let mut depth = 1;
            let mut in_string = false;
            let mut escape = false;
            i += 1;

            while i < chars.len() && depth > 0 {
                let c = chars[i];
                if escape {
                    escape = false;
                } else if c == '\\' && in_string {
                    escape = true;
                } else if c == '"' {
                    in_string = !in_string;
                } else if !in_string {
                    if c == '{' {
                        depth += 1;
                    } else if c == '}' {
                        depth -= 1;
                    }
                }
                i += 1;
            }

            if depth == 0 {
                let json_str: String = chars[start..i].iter().collect();

                // Check if it contains "name" field (could be a tool call)
                if json_str.contains("\"name\"") {
                    if let Ok(call) = serde_json::from_str::<ToolCall>(&json_str) {
                        // Validate it looks like a real tool call
                        if !call.name.is_empty()
                            && call.name.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-')
                        {
                            if !found_any {
                                prefix_text = chars[..start].iter().collect();
                                found_any = true;
                            }
                            tool_calls.push(call);
                            last_end = i;
                        }
                    }
                }
            }
        } else {
            i += 1;
        }
    }

    if !found_any {
        return None;
    }

    Some(ParsedResponse {
        prefix_text,
        tool_calls,
        suffix_text: chars[last_end..].iter().collect(),
        format_detected: Some(ToolCallFormat::DirectJson),
    })
}

/// Parse a single tool call JSON block with multiple fallback strategies
fn parse_tool_call_json(json_str: &str) -> Result<ToolCall> {
    let cleaned = json_str.trim();

    // 1. Direct parsing
    if let Ok(call) = serde_json::from_str::<ToolCall>(cleaned) {
        return Ok(call);
    }

    // 2. Strip markdown code block markers
    let stripped = strip_markdown_code_block(cleaned);
    if let Ok(call) = serde_json::from_str::<ToolCall>(&stripped) {
        return Ok(call);
    }

    // 3. Try to find and extract a JSON object
    if let Some(start) = cleaned.find('{') {
        // Find matching closing brace (handle nested objects)
        let mut depth = 0;
        let mut end_pos = None;

        for (i, ch) in cleaned[start..].char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end_pos = Some(start + i);
                        break;
                    }
                }
                _ => {}
            }
        }

        if let Some(end) = end_pos {
            let json_part = &cleaned[start..=end];
            if let Ok(call) = serde_json::from_str::<ToolCall>(json_part) {
                return Ok(call);
            }

            // Try parsing as generic JSON and extracting fields
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(json_part) {
                if let Some(name) = value.get("name").and_then(|v| v.as_str()) {
                    let arguments = value.get("arguments")
                        .or_else(|| value.get("args"))
                        .or_else(|| value.get("params"))
                        .and_then(|v| v.as_object())
                        .map(|obj| obj.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                        .unwrap_or_default();

                    return Ok(ToolCall {
                        name: name.to_string(),
                        arguments,
                    });
                }
            }
        }
    }

    Err(anyhow!("Failed to parse tool call JSON: {}", json_str))
}

/// Strip markdown code block markers
fn strip_markdown_code_block(s: &str) -> String {
    let mut result = s.trim().to_string();

    // Remove opening ```json or ```
    if result.starts_with("```") {
        if let Some(newline_pos) = result.find('\n') {
            result = result[newline_pos + 1..].to_string();
        } else {
            result = result[3..].to_string();
        }
    }

    // Remove closing ```
    if result.ends_with("```") {
        result = result[..result.len() - 3].to_string();
    }

    result.trim().to_string()
}

/// Format tools for injection into LLM prompts
pub fn format_tools_for_prompt(tools: &[super::protocol::McpTool]) -> String {
    if tools.is_empty() {
        return String::new();
    }

    let mut output = String::from("You have access to these tools:\n\n");

    for tool in tools {
        output.push_str(&format!("## {}\n", tool.name));
        output.push_str(&format!("{}\n", tool.description));

        // Format the input schema in a simple way
        if let Some(properties) = tool.input_schema.get("properties") {
            output.push_str("Parameters: ");
            if let Some(obj) = properties.as_object() {
                let params: Vec<String> = obj.keys().map(|k| format!("\"{}\"", k)).collect();
                output.push_str(&format!("{{{}}}\n", params.join(", ")));
            }
        }
        output.push('\n');
    }

    output.push_str(
        r#"To use a tool, respond with:
<tool_call>
{"name": "tool_name", "arguments": {"param": "value"}}
</tool_call>

After the tool result, continue your response naturally.
"#,
    );

    output
}

/// Format a tool result for inclusion in the conversation
pub fn format_tool_result(tool_name: &str, result: &super::protocol::CallToolResult) -> String {
    let mut output = String::new();

    for content in &result.content {
        match content {
            super::protocol::ToolContent::Text { text } => {
                output.push_str(text);
            }
            super::protocol::ToolContent::Image { .. } => {
                output.push_str("[Image content]");
            }
            super::protocol::ToolContent::Resource { resource } => {
                if let Some(text) = &resource.text {
                    output.push_str(text);
                } else {
                    output.push_str(&format!("[Resource: {}]", resource.uri));
                }
            }
        }
    }

    if result.is_error {
        format!("Tool '{}' error: {}", tool_name, output)
    } else {
        format!("Tool '{}' result:\n{}", tool_name, output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_xml_tool_call() {
        let response = r#"Let me search for that.
<tool_call>
{"name": "xbps_search", "arguments": {"query": "htop"}}
</tool_call>
Here are the results."#;

        let parsed = parse_tool_calls(response);

        assert_eq!(parsed.tool_calls.len(), 1);
        assert_eq!(parsed.tool_calls[0].name, "xbps_search");
        assert_eq!(parsed.format_detected, Some(ToolCallFormat::XmlTags));
        assert!(parsed.prefix_text.contains("Let me search"));
    }

    #[test]
    fn test_parse_function_call_tag() {
        let response = r#"<function_call>
{"name": "system_info", "arguments": {}}
</function_call>"#;

        let parsed = parse_tool_calls(response);

        assert_eq!(parsed.tool_calls.len(), 1);
        assert_eq!(parsed.tool_calls[0].name, "system_info");
        assert_eq!(parsed.format_detected, Some(ToolCallFormat::XmlTags));
    }

    #[test]
    fn test_parse_multiple_tool_calls() {
        let response = r#"
<tool_call>
{"name": "tool1", "arguments": {}}
</tool_call>
<tool_call>
{"name": "tool2", "arguments": {"x": 1}}
</tool_call>
"#;

        let parsed = parse_tool_calls(response);

        assert_eq!(parsed.tool_calls.len(), 2);
        assert_eq!(parsed.tool_calls[0].name, "tool1");
        assert_eq!(parsed.tool_calls[1].name, "tool2");
    }

    #[test]
    fn test_parse_json_code_block() {
        let response = r#"I'll search for that package:

```json
{"name": "xbps_search", "arguments": {"query": "python"}}
```

Let me know what you find."#;

        let parsed = parse_tool_calls(response);

        assert_eq!(parsed.tool_calls.len(), 1);
        assert_eq!(parsed.tool_calls[0].name, "xbps_search");
        assert_eq!(parsed.format_detected, Some(ToolCallFormat::JsonCodeBlock));
    }

    #[test]
    fn test_parse_wrapped_json_code_block() {
        let response = r#"```json
{"tool_call": {"name": "service_status", "arguments": {"service": "sshd"}}}
```"#;

        let parsed = parse_tool_calls(response);

        assert_eq!(parsed.tool_calls.len(), 1);
        assert_eq!(parsed.tool_calls[0].name, "service_status");
    }

    #[test]
    fn test_parse_function_syntax() {
        let response = r#"Let me check that for you: xbps_search({"query": "vim"})"#;

        let parsed = parse_tool_calls(response);

        assert_eq!(parsed.tool_calls.len(), 1);
        assert_eq!(parsed.tool_calls[0].name, "xbps_search");
        assert_eq!(parsed.format_detected, Some(ToolCallFormat::FunctionSyntax));
    }

    #[test]
    fn test_parse_direct_json() {
        let response = r#"I'll use this tool: {"name": "system_info", "arguments": {}}"#;

        let parsed = parse_tool_calls(response);

        assert_eq!(parsed.tool_calls.len(), 1);
        assert_eq!(parsed.tool_calls[0].name, "system_info");
        assert_eq!(parsed.format_detected, Some(ToolCallFormat::DirectJson));
    }

    #[test]
    fn test_parse_no_tool_calls() {
        let response = "Just a normal response without any tools.";

        let parsed = parse_tool_calls(response);

        assert!(!parsed.has_tool_calls());
        assert_eq!(parsed.format_detected, None);
    }

    #[test]
    fn test_parse_tool_call_with_nested_json() {
        let response = r#"<tool_call>
{"name": "complex_tool", "arguments": {"config": {"nested": {"deep": true}}, "list": [1, 2, 3]}}
</tool_call>"#;

        let parsed = parse_tool_calls(response);

        assert_eq!(parsed.tool_calls.len(), 1);
        assert_eq!(parsed.tool_calls[0].name, "complex_tool");
        assert!(parsed.tool_calls[0].arguments.contains_key("config"));
    }

    #[test]
    fn test_parse_args_alias() {
        // Some LLMs use "args" instead of "arguments"
        let response = r#"<tool_call>
{"name": "test_tool", "args": {"key": "value"}}
</tool_call>"#;

        let parsed = parse_tool_calls(response);

        assert_eq!(parsed.tool_calls.len(), 1);
        assert_eq!(parsed.tool_calls[0].name, "test_tool");
        assert!(parsed.tool_calls[0].arguments.contains_key("key"));
    }

    #[test]
    fn test_skip_false_positive_functions() {
        let response = r#"Here's some code:
if (condition) {
    return value;
}
for (item in items) {
    process(item);
}"#;

        let parsed = parse_tool_calls(response);

        assert!(!parsed.has_tool_calls());
    }

    #[test]
    fn test_format_tools() {
        use super::super::protocol::McpTool;

        let tools = vec![McpTool {
            name: "xbps_search".to_string(),
            description: "Search for packages".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"}
                }
            }),
        }];

        let formatted = format_tools_for_prompt(&tools);

        assert!(formatted.contains("xbps_search"));
        assert!(formatted.contains("Search for packages"));
        assert!(formatted.contains("<tool_call>"));
    }
}

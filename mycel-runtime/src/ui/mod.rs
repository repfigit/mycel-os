//! UI Factory - Dynamic interface generation
//!
//! Creates surfaces (UI elements) on-demand based on AI specifications.
//!
//! Security features:
//! - Content Security Policy (CSP) headers on all HTML surfaces
//! - Minimal external resource loading
//! - XSS protection via HTML escaping

use anyhow::Result;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::ai::UiSpec;
use crate::config::MycelConfig;

/// Content Security Policy for surfaces without external resources
const CSP_STRICT: &str = "default-src 'none'; style-src 'unsafe-inline'; img-src 'self' data:; base-uri 'none'; form-action 'none'; frame-ancestors 'none';";

/// Content Security Policy for surfaces with CodeMirror CDN
const CSP_CODEMIRROR: &str = "default-src 'none'; script-src https://cdnjs.cloudflare.com 'unsafe-inline'; style-src 'unsafe-inline' https://cdnjs.cloudflare.com; img-src 'self' data:; base-uri 'none'; form-action 'none'; frame-ancestors 'none';";

/// Factory for creating UI surfaces
#[derive(Clone)]
pub struct UiFactory {
    config: MycelConfig,
}

impl UiFactory {
    pub fn new(config: &MycelConfig) -> Result<Self> {
        Ok(Self {
            config: config.clone(),
        })
    }

    /// Create a surface from a UI specification
    pub fn create_surface(&self, spec: &UiSpec) -> Result<Surface> {
        let id = Uuid::new_v4().to_string();

        let surface_type = match spec.ui_type.as_str() {
            "html" => SurfaceType::Html,
            "react" => SurfaceType::React,
            "native" => SurfaceType::Native,
            _ => SurfaceType::Html,
        };

        Ok(Surface {
            id,
            title: spec.title.clone(),
            surface_type,
            width: spec.width,
            height: spec.height,
            content: spec.content.clone(),
            interactive: spec.interactive,
            state: SurfaceState::Created,
        })
    }

    /// Create a simple text display surface
    pub fn text_surface(&self, title: &str, content: &str) -> Surface {
        Surface {
            id: Uuid::new_v4().to_string(),
            title: title.to_string(),
            surface_type: SurfaceType::Html,
            width: 600,
            height: 400,
            content: format!(
                r#"<!DOCTYPE html>
<html>
<head>
    <meta charset="utf-8">
    <meta http-equiv="Content-Security-Policy" content="{}">
    <meta name="referrer" content="no-referrer">
    <style>
        body {{
            font-family: system-ui, sans-serif;
            padding: 20px;
            background: #1a1a2e;
            color: #eee;
            line-height: 1.6;
        }}
        pre {{
            background: #16213e;
            padding: 15px;
            border-radius: 8px;
            overflow-x: auto;
        }}
    </style>
</head>
<body>
    <pre>{}</pre>
</body>
</html>"#,
                CSP_STRICT,
                html_escape::encode_text(content)
            ),
            interactive: false,
            state: SurfaceState::Created,
        }
    }

    /// Create a code editor surface
    pub fn code_editor_surface(&self, title: &str, code: &str, language: &str) -> Surface {
        Surface {
            id: Uuid::new_v4().to_string(),
            title: title.to_string(),
            surface_type: SurfaceType::Html,
            width: 800,
            height: 600,
            content: format!(
                r#"<!DOCTYPE html>
<html>
<head>
    <meta charset="utf-8">
    <meta http-equiv="Content-Security-Policy" content="{}">
    <meta name="referrer" content="no-referrer">
    <link rel="stylesheet" href="https://cdnjs.cloudflare.com/ajax/libs/codemirror/5.65.5/codemirror.min.css">
    <script src="https://cdnjs.cloudflare.com/ajax/libs/codemirror/5.65.5/codemirror.min.js"></script>
    <script src="https://cdnjs.cloudflare.com/ajax/libs/codemirror/5.65.5/mode/python/python.min.js"></script>
    <style>
        body {{ margin: 0; }}
        .CodeMirror {{ height: 100vh; font-size: 14px; }}
    </style>
</head>
<body>
    <textarea id="code">{}</textarea>
    <script>
        var editor = CodeMirror.fromTextArea(document.getElementById('code'), {{
            lineNumbers: true,
            mode: '{}',
            theme: 'default'
        }});
    </script>
</body>
</html>"#,
                CSP_CODEMIRROR,
                html_escape::encode_text(code),
                language
            ),
            interactive: true,
            state: SurfaceState::Created,
        }
    }

    /// Create a comparison/diff surface
    pub fn comparison_surface(&self, title: &str, items: Vec<(&str, &str)>) -> Surface {
        let columns: String = items
            .iter()
            .map(|(name, content)| {
                format!(
                    r#"<div class="column">
                        <h3>{}</h3>
                        <div class="content">{}</div>
                    </div>"#,
                    html_escape::encode_text(name),
                    html_escape::encode_text(content)
                )
            })
            .collect::<Vec<_>>()
            .join("\n");

        let column_count = items.len();

        Surface {
            id: Uuid::new_v4().to_string(),
            title: title.to_string(),
            surface_type: SurfaceType::Html,
            width: 1200,
            height: 800,
            content: format!(
                r#"<!DOCTYPE html>
<html>
<head>
    <meta charset="utf-8">
    <meta http-equiv="Content-Security-Policy" content="{}">
    <meta name="referrer" content="no-referrer">
    <style>
        body {{
            font-family: system-ui, sans-serif;
            margin: 0;
            padding: 20px;
            background: #1a1a2e;
            color: #eee;
        }}
        .container {{
            display: grid;
            grid-template-columns: repeat({}, 1fr);
            gap: 20px;
            height: calc(100vh - 40px);
        }}
        .column {{
            background: #16213e;
            border-radius: 8px;
            padding: 15px;
            overflow-y: auto;
        }}
        .column h3 {{
            margin-top: 0;
            padding-bottom: 10px;
            border-bottom: 1px solid #0f3460;
        }}
        .content {{
            white-space: pre-wrap;
            font-family: monospace;
            font-size: 13px;
        }}
    </style>
</head>
<body>
    <div class="container">
        {}
    </div>
</body>
</html>"#,
                CSP_STRICT, column_count, columns
            ),
            interactive: true,
            state: SurfaceState::Created,
        }
    }
}

/// A UI surface that can be displayed
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Surface {
    pub id: String,
    pub title: String,
    pub surface_type: SurfaceType,
    pub width: u32,
    pub height: u32,
    pub content: String,
    pub interactive: bool,
    pub state: SurfaceState,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MycelConfig;

    #[test]
    fn test_text_surface_generation() {
        let config = MycelConfig::default();
        let factory = UiFactory::new(&config).unwrap();
        let surface = factory.text_surface("Test Title", "Hello World");

        assert_eq!(surface.title, "Test Title");
        assert!(surface.content.contains("Hello World"));
        assert!(surface.content.contains("<!DOCTYPE html>"));
    }

    #[test]
    fn test_code_editor_surface() {
        let config = MycelConfig::default();
        let factory = UiFactory::new(&config).unwrap();
        let surface = factory.code_editor_surface("Code", "print('hi')", "python");

        assert!(surface.content.contains("print('hi')"));
        assert!(surface.content.contains("python"));
        assert!(surface.interactive);
    }
}

/// Types of surfaces
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SurfaceType {
    /// Raw HTML content
    Html,
    /// React component
    React,
    /// Native widgets (GTK/Qt)
    Native,
}

/// Surface lifecycle state
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SurfaceState {
    Created,
    Rendering,
    Active,
    Hidden,
    Destroyed,
}

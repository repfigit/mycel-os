//! Code Generation utilities
//!
//! Helpers for generating, validating, and managing AI-generated code.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;

/// A generated code artifact
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeArtifact {
    pub id: String,
    pub language: CodeLanguage,
    pub code: String,
    pub description: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub executed: bool,
    pub saved_path: Option<PathBuf>,
}

impl CodeArtifact {
    pub fn new(language: CodeLanguage, code: String, description: String) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            language,
            code,
            description,
            created_at: chrono::Utc::now(),
            executed: false,
            saved_path: None,
        }
    }

    /// Save the code to a file
    pub fn save(&mut self, base_path: &str) -> Result<PathBuf> {
        let extension = match self.language {
            CodeLanguage::Python => "py",
            CodeLanguage::JavaScript => "js",
            CodeLanguage::TypeScript => "ts",
            CodeLanguage::Rust => "rs",
            CodeLanguage::Shell => "sh",
            CodeLanguage::Html => "html",
            CodeLanguage::Css => "css",
            CodeLanguage::Unknown => "txt",
        };

        let filename = format!("{}_{}.{}", 
            self.created_at.format("%Y%m%d_%H%M%S"),
            &self.id[..8],
            extension
        );
        
        let path = PathBuf::from(base_path).join(&filename);
        std::fs::create_dir_all(base_path)?;
        std::fs::write(&path, &self.code)?;
        
        self.saved_path = Some(path.clone());
        Ok(path)
    }
}

/// Supported code languages
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub enum CodeLanguage {
    Python,
    JavaScript,
    TypeScript,
    Rust,
    Shell,
    Html,
    Css,
    Unknown,
}

impl CodeLanguage {
    /// Detect language from code content
    pub fn detect(code: &str) -> Self {
        let code_lower = code.to_lowercase();
        
        // Check for shebangs first
        if code.starts_with("#!/usr/bin/env python") || code.starts_with("#!/usr/bin/python") {
            return Self::Python;
        }
        if code.starts_with("#!/bin/bash") || code.starts_with("#!/bin/sh") {
            return Self::Shell;
        }
        
        // Check for language-specific patterns
        if code_lower.contains("import ") && (code_lower.contains("def ") || code_lower.contains("class ")) {
            return Self::Python;
        }
        if code_lower.contains("fn ") && code_lower.contains("let ") && code_lower.contains("->") {
            return Self::Rust;
        }
        if code_lower.contains("interface ") || code_lower.contains(": string") || code_lower.contains(": number") {
            return Self::TypeScript;
        }
        if code_lower.contains("const ") || code_lower.contains("function ") || code_lower.contains("=>") {
            return Self::JavaScript;
        }
        if code_lower.contains("<!doctype") || code_lower.contains("<html") {
            return Self::Html;
        }
        if code.contains("{") && code.contains("}") && (code.contains("color:") || code.contains("margin:")) {
            return Self::Css;
        }
        
        Self::Unknown
    }

    /// Get the file extension for this language
    pub fn extension(&self) -> &'static str {
        match self {
            Self::Python => "py",
            Self::JavaScript => "js",
            Self::TypeScript => "ts",
            Self::Rust => "rs",
            Self::Shell => "sh",
            Self::Html => "html",
            Self::Css => "css",
            Self::Unknown => "txt",
        }
    }

    /// Get the interpreter/compiler command
    pub fn executor(&self) -> Option<&'static str> {
        match self {
            Self::Python => Some("python3"),
            Self::JavaScript => Some("node"),
            Self::Shell => Some("bash"),
            _ => None,
        }
    }
}

/// Template for common code patterns
pub struct CodeTemplate {
    pub name: String,
    pub description: String,
    pub language: CodeLanguage,
    pub template: String,
    pub variables: Vec<String>,
}

impl CodeTemplate {
    /// Common templates for quick generation
    pub fn builtin_templates() -> Vec<Self> {
        vec![
            Self {
                name: "python_script".to_string(),
                description: "Basic Python script with main function".to_string(),
                language: CodeLanguage::Python,
                template: r#"#!/usr/bin/env python3
"""{{description}}"""

def main():
    {{code}}

if __name__ == "__main__":
    main()
"#.to_string(),
                variables: vec!["description".to_string(), "code".to_string()],
            },
            Self {
                name: "data_analysis".to_string(),
                description: "Python data analysis script".to_string(),
                language: CodeLanguage::Python,
                template: r#"#!/usr/bin/env python3
"""{{description}}"""
import json
from collections import Counter

def analyze(data):
    {{analysis_code}}

def main():
    # Input data
    data = {{input_data}}
    
    # Run analysis
    result = analyze(data)
    
    # Output
    print(json.dumps(result, indent=2))

if __name__ == "__main__":
    main()
"#.to_string(),
                variables: vec!["description".to_string(), "analysis_code".to_string(), "input_data".to_string()],
            },
            Self {
                name: "web_scraper".to_string(),
                description: "Simple web data fetcher (no actual scraping to respect policies)".to_string(),
                language: CodeLanguage::Python,
                template: r#"#!/usr/bin/env python3
"""{{description}}"""
import urllib.request
import json

def fetch_data(url):
    with urllib.request.urlopen(url) as response:
        return response.read().decode('utf-8')

def main():
    url = "{{url}}"
    data = fetch_data(url)
    {{processing_code}}

if __name__ == "__main__":
    main()
"#.to_string(),
                variables: vec!["description".to_string(), "url".to_string(), "processing_code".to_string()],
            },
        ]
    }
}

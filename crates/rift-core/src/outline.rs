//! AST skeletons: parse a source file with tree-sitter and emit a
//! signatures-only outline — declaration headers with line numbers, bodies
//! elided. This is what the model explores instead of full file reads,
//! typically 10-20x fewer tokens.

use std::path::Path;

use anyhow::{bail, Result};
use tree_sitter::{Language, Node, Parser};

struct LangSpec {
    language: Language,
    /// Node kinds emitted as outline entries.
    decls: &'static [&'static str],
    /// Node kinds whose bodies are recursed into (impl blocks, classes, mods).
    containers: &'static [&'static str],
    /// Node kinds that are transparent wrappers to recurse through without
    /// emitting (export statements, decorated definitions).
    transparent: &'static [&'static str],
}

fn spec_for(path: &Path) -> Option<LangSpec> {
    let ext = path.extension()?.to_str()?;
    Some(match ext {
        "rs" => LangSpec {
            language: tree_sitter_rust::LANGUAGE.into(),
            decls: &[
                "function_item", "struct_item", "enum_item", "trait_item", "impl_item",
                "mod_item", "const_item", "static_item", "type_item", "macro_definition",
                "union_item",
            ],
            containers: &["impl_item", "trait_item", "mod_item"],
            transparent: &[],
        },
        "py" => LangSpec {
            language: tree_sitter_python::LANGUAGE.into(),
            decls: &["function_definition", "class_definition"],
            containers: &["class_definition"],
            transparent: &["decorated_definition"],
        },
        "js" | "jsx" | "mjs" | "cjs" => LangSpec {
            language: tree_sitter_javascript::LANGUAGE.into(),
            decls: &[
                "function_declaration", "generator_function_declaration", "class_declaration",
                "method_definition", "lexical_declaration", "variable_declaration",
            ],
            containers: &["class_declaration"],
            transparent: &["export_statement"],
        },
        "ts" => LangSpec {
            language: tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            decls: &[
                "function_declaration", "class_declaration", "method_definition",
                "interface_declaration", "type_alias_declaration", "enum_declaration",
                "lexical_declaration", "variable_declaration", "abstract_class_declaration",
                "method_signature",
            ],
            containers: &["class_declaration", "abstract_class_declaration", "interface_declaration"],
            transparent: &["export_statement"],
        },
        "tsx" => LangSpec {
            language: tree_sitter_typescript::LANGUAGE_TSX.into(),
            decls: &[
                "function_declaration", "class_declaration", "method_definition",
                "interface_declaration", "type_alias_declaration", "enum_declaration",
                "lexical_declaration", "variable_declaration",
            ],
            containers: &["class_declaration", "interface_declaration"],
            transparent: &["export_statement"],
        },
        "go" => LangSpec {
            language: tree_sitter_go::LANGUAGE.into(),
            decls: &[
                "function_declaration", "method_declaration", "type_declaration",
                "const_declaration", "var_declaration",
            ],
            containers: &[],
            transparent: &[],
        },
        _ => return None,
    })
}

pub fn supports(path: &Path) -> bool {
    spec_for(path).is_some()
}

/// Produce the signatures-only outline of `source` for the language implied
/// by `path`'s extension.
pub fn outline_source(path: &Path, source: &str) -> Result<String> {
    let Some(spec) = spec_for(path) else {
        bail!(
            "unsupported file type '{}' (supported: .rs .py .js .jsx .ts .tsx .go); use the read tool instead",
            path.display()
        )
    };
    let mut parser = Parser::new();
    parser.set_language(&spec.language)?;
    let Some(tree) = parser.parse(source, None) else {
        bail!("parse failed for {}", path.display())
    };
    let mut out = String::new();
    visit(tree.root_node(), source, &spec, 0, &mut out);
    if out.is_empty() {
        out = "(no top-level declarations found)".into();
    }
    Ok(out)
}

fn visit(node: Node, source: &str, spec: &LangSpec, depth: usize, out: &mut String) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        let kind = child.kind();
        if spec.transparent.contains(&kind) {
            visit(child, source, spec, depth, out);
        } else if spec.decls.contains(&kind) {
            emit_header(child, source, depth, out);
            if spec.containers.contains(&kind) {
                if let Some(body) = child.child_by_field_name("body") {
                    visit(body, source, spec, depth + 1, out);
                }
            }
        }
    }
}

/// Header = node text up to its body (or the whole first line), whitespace
/// collapsed, with the 1-based start line number.
fn emit_header(node: Node, source: &str, depth: usize, out: &mut String) {
    let start = node.start_byte();
    let end = node
        .child_by_field_name("body")
        .map(|b| b.start_byte())
        .unwrap_or_else(|| node.end_byte())
        .min(source.len());
    let raw = &source[start..end];
    // Without a body field (e.g. type aliases), keep just the first line.
    let raw = if node.child_by_field_name("body").is_none() {
        raw.lines().next().unwrap_or(raw)
    } else {
        raw
    };
    let header: String = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let header: String = header.chars().take(160).collect();
    let line = node.start_position().row + 1;
    let indent = "  ".repeat(depth);
    out.push_str(&format!("{line:>5}  {indent}{header} …\n"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn rust_outline_has_fns_and_impls_without_bodies() {
        let src = r#"
pub struct Point { x: i32, y: i32 }

impl Point {
    pub fn new(x: i32, y: i32) -> Self {
        let secret_body = 42;
        Self { x, y }
    }
}

fn helper() -> bool { true }
"#;
        let out = outline_source(&PathBuf::from("a.rs"), src).unwrap();
        assert!(out.contains("pub struct Point"));
        assert!(out.contains("impl Point"));
        assert!(out.contains("pub fn new(x: i32, y: i32) -> Self"));
        assert!(out.contains("fn helper() -> bool"));
        assert!(!out.contains("secret_body"));
    }

    #[test]
    fn python_outline_includes_class_methods_and_decorated() {
        let src = r#"
class Greeter:
    def __init__(self, name):
        self.name = name

    @property
    def loud(self):
        return self.name.upper()

def main():
    hidden = 1
"#;
        let out = outline_source(&PathBuf::from("a.py"), src).unwrap();
        assert!(out.contains("class Greeter"));
        assert!(out.contains("def __init__(self, name)"));
        assert!(out.contains("def loud(self)"));
        assert!(out.contains("def main()"));
        assert!(!out.contains("hidden"));
    }

    #[test]
    fn typescript_outline_handles_exports_and_interfaces() {
        let src = r#"
export interface User { id: number }
export function getUser(id: number): User {
    const internal = id * 2;
    return { id: internal };
}
export class Repo {
    find(id: number): User { return { id }; }
}
"#;
        let out = outline_source(&PathBuf::from("a.ts"), src).unwrap();
        assert!(out.contains("interface User"));
        assert!(out.contains("function getUser(id: number): User"));
        assert!(out.contains("class Repo"));
        assert!(!out.contains("internal"));
    }

    #[test]
    fn unsupported_extension_errors() {
        assert!(outline_source(&PathBuf::from("a.xyz"), "x").is_err());
    }
}

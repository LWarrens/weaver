use crate::adapters::edges::{self, RawEdge};
use crate::adapters::symbols;

/// Dispatch edge extraction by file extension.
/// Returns an empty vec if no extractor is registered for the extension.
pub fn extract_edges_for_extension(ext: &str, source: &str) -> Vec<RawEdge> {
    match ext.trim_start_matches('.').to_lowercase().as_str() {
        "rs" | "rust" => edges::extract_rust_edges(source),
        "ts" | "tsx" => edges::extract_typescript_edges(source),
        "js" | "mjs" | "cjs" => edges::extract_javascript_edges(source),
        "py" => edges::extract_python_edges(source),
        "go" => edges::extract_go_edges(source),
        "java" => edges::extract_java_edges(source),
        "c" | "h" => edges::extract_c_edges(source),
        "cpp" | "cc" | "cxx" | "hpp" | "hxx" => edges::extract_cpp_edges(source),
        "rb" => edges::extract_ruby_edges(source),
        _ => vec![],
    }
}

/// Dispatch symbol extraction by file extension.
/// Returns Err if no extractor is registered for the extension.
pub fn extract_symbols_for_extension(
    ext: &str,
    source: &str,
) -> Result<Vec<symbols::Symbol>, String> {
    match ext.trim_start_matches('.').to_lowercase().as_str() {
        // Tier 0
        "json" => json_extract(source),
        "md" | "markdown" => markdown_extract(source),
        "adoc" | "asciidoc" => asciidoc_extract(source),
        "yml" | "yaml" => yaml_extract(source),
        "toml" => toml_extract(source),
        "xml" => xml_extract(source),
        // Tier 1
        "js" | "mjs" | "cjs" => javascript_extract(source),
        "ts" => typescript_extract(source),
        "tsx" => tsx_extract(source),
        "py" => python_extract(source),
        "java" => java_extract(source),
        "go" => go_extract(source),
        "rs" | "rust" => rust_extract(source),
        // Tier 2
        "sh" | "bash" | "zsh" => bash_extract(source),
        "dockerfile" | "containerfile" => dockerfile_extract(source),
        "cmake" => cmake_extract(source),
        "mk" | "makefile" => make_extract(source),
        "tf" | "tfvars" | "hcl" => hcl_extract(source),
        "nix" => nix_extract(source),
        // Tier 3
        "c" | "h" => c_extract(source),
        "cpp" | "cc" | "cxx" | "hpp" | "hxx" => cpp_extract(source),
        "cs" => csharp_extract(source),
        "php" => php_extract(source),
        "rb" => ruby_extract(source),
        "swift" => swift_extract(source),
        "scala" | "sc" => scala_extract(source),
        "ex" | "exs" => elixir_extract(source),
        "lua" => lua_extract(source),
        "hs" | "lhs" => haskell_extract(source),
        other => Err(format!("no extractor registered for extension '{}'", other)),
    }
}

/// Return whether the extension is a source format this binary can actually parse.
/// Uses cfg!() so the answer reflects which tree-sitter features were compiled in.
pub fn has_extractor_for_extension(ext: &str) -> bool {
    match ext.trim_start_matches('.').to_lowercase().as_str() {
        "json" => cfg!(feature = "tree-sitter-json"),
        "md" | "markdown" => cfg!(feature = "tree-sitter-md"),
        "adoc" | "asciidoc" => cfg!(feature = "tree-sitter-asciidoc"),
        "yml" | "yaml" => cfg!(feature = "tree-sitter-yaml"),
        "toml" => cfg!(feature = "tree-sitter-toml-ng"),
        "xml" => cfg!(feature = "tree-sitter-xml"),
        "js" | "mjs" | "cjs" => cfg!(feature = "tree-sitter-javascript"),
        "ts" | "tsx" => cfg!(feature = "tree-sitter-typescript"),
        "py" => cfg!(feature = "tree-sitter-python"),
        "java" => cfg!(feature = "tree-sitter-java"),
        "go" => cfg!(feature = "tree-sitter-go"),
        "rs" | "rust" => cfg!(feature = "tree-sitter-rust"),
        "sh" | "bash" | "zsh" => cfg!(feature = "tree-sitter-bash"),
        "dockerfile" | "containerfile" => cfg!(feature = "tree-sitter-containerfile"),
        "cmake" => cfg!(feature = "tree-sitter-cmake"),
        "mk" | "makefile" => cfg!(feature = "tree-sitter-make"),
        "tf" | "tfvars" | "hcl" => cfg!(feature = "tree-sitter-hcl"),
        "nix" => cfg!(feature = "tree-sitter-nix"),
        "c" | "h" => cfg!(feature = "tree-sitter-c"),
        "cpp" | "cc" | "cxx" | "hpp" | "hxx" => cfg!(feature = "tree-sitter-cpp"),
        "cs" => cfg!(feature = "tree-sitter-c-sharp"),
        "php" => cfg!(feature = "tree-sitter-php"),
        "rb" => cfg!(feature = "tree-sitter-ruby"),
        "swift" => cfg!(feature = "tree-sitter-swift"),
        "scala" | "sc" => cfg!(feature = "tree-sitter-scala"),
        "ex" | "exs" => cfg!(feature = "tree-sitter-elixir"),
        "lua" => cfg!(feature = "tree-sitter-lua"),
        "hs" | "lhs" => cfg!(feature = "tree-sitter-haskell"),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Extract the name from a tree-sitter node: try the "name" field first,
/// then fall back to the first identifier-like named child.
#[allow(dead_code)]
fn node_name<'a>(node: tree_sitter::Node<'a>, source: &'a [u8]) -> Option<String> {
    node.child_by_field_name("name")
        .and_then(|n| n.utf8_text(source).ok())
        .or_else(|| {
            node.named_children(&mut node.walk())
                .find_map(|c| match c.kind() {
                    "identifier"
                    | "type_identifier"
                    | "property_identifier"
                    | "field_identifier" => c.utf8_text(source).ok(),
                    _ => None,
                })
        })
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

// ---------------------------------------------------------------------------
// Tier 0 extractors
// ---------------------------------------------------------------------------

#[cfg(feature = "tree-sitter-json")]
fn json_extract(source: &str) -> Result<Vec<symbols::Symbol>, String> {
    use tree_sitter::Parser;
    use tree_sitter_json::LANGUAGE;

    let mut parser = Parser::new();
    parser
        .set_language(&LANGUAGE.into())
        .map_err(|_| "failed to load JSON parser".to_string())?;

    let tree = parser
        .parse(source, None)
        .ok_or_else(|| "failed to parse source".to_string())?;
    if tree.root_node().has_error() {
        return Err("parse errors in source".to_string());
    }

    Ok(vec![symbols::Symbol {
        name: "document".to_string(),
        kind: "json".to_string(),
        start_line: 1,
        end_line: 1,
        ..Default::default()
    }])
}

#[cfg(not(feature = "tree-sitter-json"))]
fn json_extract(_: &str) -> Result<Vec<symbols::Symbol>, String> {
    Err("no extractor registered for json".to_string())
}

#[cfg(feature = "tree-sitter-md")]
fn markdown_extract(source: &str) -> Result<Vec<symbols::Symbol>, String> {
    use tree_sitter::Parser;

    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_md::LANGUAGE.into())
        .map_err(|_| "failed to load Markdown parser".to_string())?;

    let tree = parser
        .parse(source, None)
        .ok_or_else(|| "failed to parse source".to_string())?;
    if tree.root_node().has_error() {
        return Err("parse errors in source".to_string());
    }

    Ok(vec![symbols::Symbol {
        name: "document".to_string(),
        kind: "markdown".to_string(),
        start_line: 1,
        end_line: 1,
        ..Default::default()
    }])
}

#[cfg(not(feature = "tree-sitter-md"))]
fn markdown_extract(_: &str) -> Result<Vec<symbols::Symbol>, String> {
    Err("no extractor registered for markdown".to_string())
}

#[cfg(feature = "tree-sitter-asciidoc")]
fn asciidoc_extract(source: &str) -> Result<Vec<symbols::Symbol>, String> {
    use tree_sitter::Parser;

    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_asciidoc::language())
        .map_err(|_| "failed to load AsciiDoc parser".to_string())?;

    let tree = parser
        .parse(source, None)
        .ok_or_else(|| "failed to parse source".to_string())?;
    if tree.root_node().has_error() {
        return Err("parse errors in source".to_string());
    }

    Ok(vec![symbols::Symbol {
        name: "document".to_string(),
        kind: "asciidoc".to_string(),
        start_line: 1,
        end_line: 1,
        ..Default::default()
    }])
}

#[cfg(not(feature = "tree-sitter-asciidoc"))]
fn asciidoc_extract(_: &str) -> Result<Vec<symbols::Symbol>, String> {
    Err("no extractor registered for asciidoc".to_string())
}

#[cfg(feature = "tree-sitter-yaml")]
fn yaml_extract(source: &str) -> Result<Vec<symbols::Symbol>, String> {
    use tree_sitter::Parser;

    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_yaml::LANGUAGE.into())
        .map_err(|_| "failed to load YAML parser".to_string())?;

    let tree = parser
        .parse(source, None)
        .ok_or_else(|| "failed to parse source".to_string())?;
    if tree.root_node().has_error() {
        return Err("parse errors in source".to_string());
    }

    Ok(vec![symbols::Symbol {
        name: "document".to_string(),
        kind: "yaml".to_string(),
        start_line: 1,
        end_line: 1,
        ..Default::default()
    }])
}

#[cfg(not(feature = "tree-sitter-yaml"))]
fn yaml_extract(_: &str) -> Result<Vec<symbols::Symbol>, String> {
    Err("no extractor registered for yaml".to_string())
}

#[cfg(feature = "tree-sitter-toml-ng")]
fn toml_extract(source: &str) -> Result<Vec<symbols::Symbol>, String> {
    use tree_sitter::Parser;

    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_toml_ng::LANGUAGE.into())
        .map_err(|_| "failed to load TOML parser".to_string())?;

    let tree = parser
        .parse(source, None)
        .ok_or_else(|| "failed to parse source".to_string())?;
    if tree.root_node().has_error() {
        return Err("parse errors in source".to_string());
    }

    Ok(vec![symbols::Symbol {
        name: "document".to_string(),
        kind: "toml".to_string(),
        start_line: 1,
        end_line: 1,
        ..Default::default()
    }])
}

#[cfg(not(feature = "tree-sitter-toml-ng"))]
fn toml_extract(_: &str) -> Result<Vec<symbols::Symbol>, String> {
    Err("no extractor registered for toml".to_string())
}

#[cfg(feature = "tree-sitter-xml")]
fn xml_extract(source: &str) -> Result<Vec<symbols::Symbol>, String> {
    use tree_sitter::Parser;

    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_xml::LANGUAGE_XML.into())
        .map_err(|_| "failed to load XML parser".to_string())?;

    let tree = parser
        .parse(source, None)
        .ok_or_else(|| "failed to parse source".to_string())?;
    if tree.root_node().has_error() {
        return Err("parse errors in source".to_string());
    }

    Ok(vec![symbols::Symbol {
        name: "document".to_string(),
        kind: "xml".to_string(),
        start_line: 1,
        end_line: 1,
        ..Default::default()
    }])
}

#[cfg(not(feature = "tree-sitter-xml"))]
fn xml_extract(_: &str) -> Result<Vec<symbols::Symbol>, String> {
    Err("no extractor registered for xml".to_string())
}

// ---------------------------------------------------------------------------
// Tier 1 extractors
// ---------------------------------------------------------------------------

#[cfg(feature = "tree-sitter-javascript")]
fn javascript_extract(source: &str) -> Result<Vec<symbols::Symbol>, String> {
    use tree_sitter::Parser;

    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_javascript::LANGUAGE.into())
        .map_err(|_| "failed to load JavaScript parser".to_string())?;

    let tree = parser
        .parse(source, None)
        .ok_or_else(|| "failed to parse source".to_string())?;

    let bytes = source.as_bytes();
    let mut found = Vec::new();
    let mut stack = vec![tree.root_node()];

    while let Some(node) = stack.pop() {
        let kind = node.kind();
        if matches!(
            kind,
            "function_declaration"
                | "generator_function_declaration"
                | "class_declaration"
                | "method_definition"
        ) {
            if let Some(name) = node_name(node, bytes) {
                found.push(symbols::Symbol {
                    name,
                    kind: kind.to_string(),
                    start_line: node.start_position().row + 1,
                    end_line: node.end_position().row + 1,
                    ..Default::default()
                });
            }
        }
        let mut i = 0;
        while let Some(c) = node.named_child(i) {
            stack.push(c);
            i += 1;
        }
    }

    Ok(found)
}

#[cfg(not(feature = "tree-sitter-javascript"))]
fn javascript_extract(_: &str) -> Result<Vec<symbols::Symbol>, String> {
    Err("no extractor registered for js".to_string())
}

#[cfg(feature = "tree-sitter-typescript")]
fn typescript_extract(source: &str) -> Result<Vec<symbols::Symbol>, String> {
    use tree_sitter::Parser;

    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into())
        .map_err(|_| "failed to load TypeScript parser".to_string())?;

    extract_ts_symbols(&mut parser, source)
}

#[cfg(not(feature = "tree-sitter-typescript"))]
fn typescript_extract(_: &str) -> Result<Vec<symbols::Symbol>, String> {
    Err("no extractor registered for ts".to_string())
}

#[cfg(feature = "tree-sitter-typescript")]
fn tsx_extract(source: &str) -> Result<Vec<symbols::Symbol>, String> {
    use tree_sitter::Parser;

    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_typescript::LANGUAGE_TSX.into())
        .map_err(|_| "failed to load TSX parser".to_string())?;

    extract_ts_symbols(&mut parser, source)
}

#[cfg(not(feature = "tree-sitter-typescript"))]
fn tsx_extract(_: &str) -> Result<Vec<symbols::Symbol>, String> {
    Err("no extractor registered for tsx".to_string())
}

#[cfg(feature = "tree-sitter-typescript")]
fn extract_ts_symbols(
    parser: &mut tree_sitter::Parser,
    source: &str,
) -> Result<Vec<symbols::Symbol>, String> {
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| "failed to parse source".to_string())?;

    let bytes = source.as_bytes();
    let mut found = Vec::new();
    let mut stack = vec![tree.root_node()];

    while let Some(node) = stack.pop() {
        let kind = node.kind();
        if matches!(
            kind,
            "function_declaration"
                | "generator_function_declaration"
                | "class_declaration"
                | "abstract_class_declaration"
                | "method_definition"
                | "interface_declaration"
                | "type_alias_declaration"
                | "enum_declaration"
        ) {
            if let Some(name) = node_name(node, bytes) {
                found.push(symbols::Symbol {
                    name,
                    kind: kind.to_string(),
                    start_line: node.start_position().row + 1,
                    end_line: node.end_position().row + 1,
                    ..Default::default()
                });
            }
        }
        let mut i = 0;
        while let Some(c) = node.named_child(i) {
            stack.push(c);
            i += 1;
        }
    }

    Ok(found)
}

#[cfg(feature = "tree-sitter-python")]
fn python_extract(source: &str) -> Result<Vec<symbols::Symbol>, String> {
    use tree_sitter::Parser;

    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_python::LANGUAGE.into())
        .map_err(|_| "failed to load Python parser".to_string())?;

    let tree = parser
        .parse(source, None)
        .ok_or_else(|| "failed to parse source".to_string())?;

    let bytes = source.as_bytes();
    let mut found = Vec::new();
    let mut stack = vec![tree.root_node()];

    while let Some(node) = stack.pop() {
        let kind = node.kind();
        if matches!(
            kind,
            "function_definition" | "async_function_definition" | "class_definition"
        ) {
            if let Some(name) = node_name(node, bytes) {
                found.push(symbols::Symbol {
                    name,
                    kind: kind.to_string(),
                    start_line: node.start_position().row + 1,
                    end_line: node.end_position().row + 1,
                    ..Default::default()
                });
            }
        }
        let mut i = 0;
        while let Some(c) = node.named_child(i) {
            stack.push(c);
            i += 1;
        }
    }

    Ok(found)
}

#[cfg(not(feature = "tree-sitter-python"))]
fn python_extract(_: &str) -> Result<Vec<symbols::Symbol>, String> {
    Err("no extractor registered for py".to_string())
}

#[cfg(feature = "tree-sitter-java")]
fn java_extract(source: &str) -> Result<Vec<symbols::Symbol>, String> {
    use tree_sitter::Parser;

    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_java::LANGUAGE.into())
        .map_err(|_| "failed to load Java parser".to_string())?;

    let tree = parser
        .parse(source, None)
        .ok_or_else(|| "failed to parse source".to_string())?;

    let bytes = source.as_bytes();
    let mut found = Vec::new();
    let mut stack = vec![tree.root_node()];

    while let Some(node) = stack.pop() {
        let kind = node.kind();
        if matches!(
            kind,
            "class_declaration"
                | "interface_declaration"
                | "enum_declaration"
                | "annotation_type_declaration"
                | "method_declaration"
                | "constructor_declaration"
        ) {
            if let Some(name) = node_name(node, bytes) {
                found.push(symbols::Symbol {
                    name,
                    kind: kind.to_string(),
                    start_line: node.start_position().row + 1,
                    end_line: node.end_position().row + 1,
                    ..Default::default()
                });
            }
        }
        let mut i = 0;
        while let Some(c) = node.named_child(i) {
            stack.push(c);
            i += 1;
        }
    }

    Ok(found)
}

#[cfg(not(feature = "tree-sitter-java"))]
fn java_extract(_: &str) -> Result<Vec<symbols::Symbol>, String> {
    Err("no extractor registered for java".to_string())
}

#[cfg(feature = "tree-sitter-go")]
fn go_extract(source: &str) -> Result<Vec<symbols::Symbol>, String> {
    use tree_sitter::Parser;

    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_go::LANGUAGE.into())
        .map_err(|_| "failed to load Go parser".to_string())?;

    let tree = parser
        .parse(source, None)
        .ok_or_else(|| "failed to parse source".to_string())?;

    let bytes = source.as_bytes();
    let mut found = Vec::new();
    let mut stack = vec![tree.root_node()];

    while let Some(node) = stack.pop() {
        let kind = node.kind();
        if matches!(
            kind,
            "function_declaration" | "method_declaration" | "type_spec"
        ) {
            if let Some(name) = node_name(node, bytes) {
                found.push(symbols::Symbol {
                    name,
                    kind: kind.to_string(),
                    start_line: node.start_position().row + 1,
                    end_line: node.end_position().row + 1,
                    ..Default::default()
                });
            }
        }
        let mut i = 0;
        while let Some(c) = node.named_child(i) {
            stack.push(c);
            i += 1;
        }
    }

    Ok(found)
}

#[cfg(not(feature = "tree-sitter-go"))]
fn go_extract(_: &str) -> Result<Vec<symbols::Symbol>, String> {
    Err("no extractor registered for go".to_string())
}

#[cfg(feature = "tree-sitter-rust")]
fn rust_extract(source: &str) -> Result<Vec<symbols::Symbol>, String> {
    symbols::extract_rust_symbols(source)
}

#[cfg(not(feature = "tree-sitter-rust"))]
fn rust_extract(_: &str) -> Result<Vec<symbols::Symbol>, String> {
    Err("no extractor registered for rs".to_string())
}

// ---------------------------------------------------------------------------
// Tier 2 extractors
// ---------------------------------------------------------------------------

#[cfg(feature = "tree-sitter-bash")]
fn bash_extract(source: &str) -> Result<Vec<symbols::Symbol>, String> {
    use tree_sitter::Parser;

    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_bash::LANGUAGE.into())
        .map_err(|_| "failed to load Bash parser".to_string())?;

    let tree = parser
        .parse(source, None)
        .ok_or_else(|| "failed to parse source".to_string())?;

    let bytes = source.as_bytes();
    let mut found = Vec::new();
    let mut stack = vec![tree.root_node()];

    while let Some(node) = stack.pop() {
        let kind = node.kind();
        if matches!(kind, "function_definition") {
            if let Some(name) = node_name(node, bytes) {
                found.push(symbols::Symbol {
                    name,
                    kind: kind.to_string(),
                    start_line: node.start_position().row + 1,
                    end_line: node.end_position().row + 1,
                    ..Default::default()
                });
            }
        }
        let mut i = 0;
        while let Some(c) = node.named_child(i) {
            stack.push(c);
            i += 1;
        }
    }

    Ok(found)
}

#[cfg(not(feature = "tree-sitter-bash"))]
fn bash_extract(_: &str) -> Result<Vec<symbols::Symbol>, String> {
    Err("no extractor registered for sh".to_string())
}

#[cfg(feature = "tree-sitter-containerfile")]
fn dockerfile_extract(source: &str) -> Result<Vec<symbols::Symbol>, String> {
    use tree_sitter::Parser;

    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_containerfile::LANGUAGE.into())
        .map_err(|_| "failed to load Dockerfile parser".to_string())?;

    let tree = parser
        .parse(source, None)
        .ok_or_else(|| "failed to parse source".to_string())?;
    if tree.root_node().has_error() {
        return Err("parse errors in source".to_string());
    }

    Ok(vec![symbols::Symbol {
        name: "dockerfile".to_string(),
        kind: "dockerfile".to_string(),
        start_line: 1,
        end_line: 1,
        ..Default::default()
    }])
}

#[cfg(not(feature = "tree-sitter-containerfile"))]
fn dockerfile_extract(_: &str) -> Result<Vec<symbols::Symbol>, String> {
    Err("no extractor registered for dockerfile".to_string())
}

#[cfg(feature = "tree-sitter-cmake")]
fn cmake_extract(source: &str) -> Result<Vec<symbols::Symbol>, String> {
    use tree_sitter::Parser;

    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_cmake::LANGUAGE.into())
        .map_err(|_| "failed to load CMake parser".to_string())?;

    let tree = parser
        .parse(source, None)
        .ok_or_else(|| "failed to parse source".to_string())?;

    let bytes = source.as_bytes();
    let mut found = Vec::new();
    let mut stack = vec![tree.root_node()];

    while let Some(node) = stack.pop() {
        let kind = node.kind();
        if matches!(kind, "function_def" | "macro_def") {
            if let Some(name) = node_name(node, bytes) {
                found.push(symbols::Symbol {
                    name,
                    kind: kind.to_string(),
                    start_line: node.start_position().row + 1,
                    end_line: node.end_position().row + 1,
                    ..Default::default()
                });
            }
        }
        let mut i = 0;
        while let Some(c) = node.named_child(i) {
            stack.push(c);
            i += 1;
        }
    }

    Ok(found)
}

#[cfg(not(feature = "tree-sitter-cmake"))]
fn cmake_extract(_: &str) -> Result<Vec<symbols::Symbol>, String> {
    Err("no extractor registered for cmake".to_string())
}

#[cfg(feature = "tree-sitter-make")]
fn make_extract(source: &str) -> Result<Vec<symbols::Symbol>, String> {
    use tree_sitter::Parser;

    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_make::LANGUAGE.into())
        .map_err(|_| "failed to load Make parser".to_string())?;

    let tree = parser
        .parse(source, None)
        .ok_or_else(|| "failed to parse source".to_string())?;

    let bytes = source.as_bytes();
    let mut found = Vec::new();
    let mut stack = vec![tree.root_node()];

    while let Some(node) = stack.pop() {
        let kind = node.kind();
        if matches!(kind, "rule") {
            if let Some(target) = node.child_by_field_name("targets") {
                if let Ok(name) = target.utf8_text(bytes) {
                    let name = name.trim().to_string();
                    if !name.is_empty() {
                        found.push(symbols::Symbol {
                            name,
                            kind: "target".to_string(),
                            start_line: node.start_position().row + 1,
                            end_line: node.end_position().row + 1,
                            ..Default::default()
                        });
                    }
                }
            }
        }
        let mut i = 0;
        while let Some(c) = node.named_child(i) {
            stack.push(c);
            i += 1;
        }
    }

    Ok(found)
}

#[cfg(not(feature = "tree-sitter-make"))]
fn make_extract(_: &str) -> Result<Vec<symbols::Symbol>, String> {
    Err("no extractor registered for makefile".to_string())
}

#[cfg(feature = "tree-sitter-hcl")]
fn hcl_extract(source: &str) -> Result<Vec<symbols::Symbol>, String> {
    use tree_sitter::Parser;

    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_hcl::LANGUAGE.into())
        .map_err(|_| "failed to load HCL parser".to_string())?;

    let tree = parser
        .parse(source, None)
        .ok_or_else(|| "failed to parse source".to_string())?;

    let bytes = source.as_bytes();
    let mut found = Vec::new();
    let mut stack = vec![tree.root_node()];

    while let Some(node) = stack.pop() {
        let kind = node.kind();
        // HCL blocks: resource "type" "name" { ... }, module "name" { ... }, etc.
        if matches!(kind, "block") {
            let mut labels: Vec<&str> = Vec::new();
            let mut ci = 0;
            while let Some(child) = node.named_child(ci) {
                if child.kind() == "string_lit" || child.kind() == "identifier" {
                    if let Ok(text) = child.utf8_text(bytes) {
                        labels.push(text);
                    }
                }
                ci += 1;
            }
            if labels.len() >= 2 {
                found.push(symbols::Symbol {
                    name: labels[1..].join(".").trim_matches('"').to_string(),
                    kind: labels[0].to_string(),
                    start_line: node.start_position().row + 1,
                    end_line: node.end_position().row + 1,
                    ..Default::default()
                });
            }
        }
        let mut i = 0;
        while let Some(c) = node.named_child(i) {
            stack.push(c);
            i += 1;
        }
    }

    Ok(found)
}

#[cfg(not(feature = "tree-sitter-hcl"))]
fn hcl_extract(_: &str) -> Result<Vec<symbols::Symbol>, String> {
    Err("no extractor registered for hcl".to_string())
}

#[cfg(feature = "tree-sitter-nix")]
fn nix_extract(source: &str) -> Result<Vec<symbols::Symbol>, String> {
    use tree_sitter::Parser;

    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_nix::LANGUAGE.into())
        .map_err(|_| "failed to load Nix parser".to_string())?;

    let tree = parser
        .parse(source, None)
        .ok_or_else(|| "failed to parse source".to_string())?;

    let bytes = source.as_bytes();
    let mut found = Vec::new();
    let mut stack = vec![tree.root_node()];

    while let Some(node) = stack.pop() {
        let kind = node.kind();
        // Top-level bindings: identifier = ...
        if kind == "binding" {
            if let Some(attr) = node.child_by_field_name("attrpath") {
                if let Ok(name) = attr.utf8_text(bytes) {
                    found.push(symbols::Symbol {
                        name: name.to_string(),
                        kind: "binding".to_string(),
                        start_line: node.start_position().row + 1,
                        end_line: node.end_position().row + 1,
                        ..Default::default()
                    });
                }
            }
        }
        let mut i = 0;
        while let Some(c) = node.named_child(i) {
            stack.push(c);
            i += 1;
        }
    }

    Ok(found)
}

#[cfg(not(feature = "tree-sitter-nix"))]
fn nix_extract(_: &str) -> Result<Vec<symbols::Symbol>, String> {
    Err("no extractor registered for nix".to_string())
}

// ---------------------------------------------------------------------------
// Tier 3 extractors
// ---------------------------------------------------------------------------

#[cfg(feature = "tree-sitter-c")]
fn c_extract(source: &str) -> Result<Vec<symbols::Symbol>, String> {
    use tree_sitter::Parser;

    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_c::LANGUAGE.into())
        .map_err(|_| "failed to load C parser".to_string())?;

    let tree = parser
        .parse(source, None)
        .ok_or_else(|| "failed to parse source".to_string())?;

    let bytes = source.as_bytes();
    let mut found = Vec::new();
    let mut stack = vec![tree.root_node()];

    while let Some(node) = stack.pop() {
        let kind = node.kind();
        if matches!(
            kind,
            "function_definition" | "struct_specifier" | "enum_specifier" | "type_definition"
        ) {
            let name = if kind == "function_definition" {
                c_declarator_name_from_fn(node, bytes)
            } else {
                node_name(node, bytes)
            };
            if let Some(name) = name {
                found.push(symbols::Symbol {
                    name,
                    kind: kind.to_string(),
                    start_line: node.start_position().row + 1,
                    end_line: node.end_position().row + 1,
                    ..Default::default()
                });
            }
        }
        let mut i = 0;
        while let Some(c) = node.named_child(i) {
            stack.push(c);
            i += 1;
        }
    }

    Ok(found)
}

#[cfg(not(feature = "tree-sitter-c"))]
fn c_extract(_: &str) -> Result<Vec<symbols::Symbol>, String> {
    Err("no extractor registered for c".to_string())
}

/// Walk the declarator chain of a C/C++ `function_definition` to extract the name.
/// The tree-sitter C/C++ grammar nests the function name inside `declarator` fields,
/// not as a direct `name` field — so `node_name()` would pick up the return type instead.
#[cfg(any(feature = "tree-sitter-c", feature = "tree-sitter-cpp"))]
fn c_declarator_name_from_fn(fn_node: tree_sitter::Node, bytes: &[u8]) -> Option<String> {
    fn unwrap_declarator(node: tree_sitter::Node, bytes: &[u8]) -> Option<String> {
        match node.kind() {
            "identifier" => node.utf8_text(bytes).ok().map(str::to_string),
            "qualified_identifier" => node
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(bytes).ok())
                .map(str::to_string),
            "function_declarator" | "pointer_declarator" | "reference_declarator" => node
                .child_by_field_name("declarator")
                .and_then(|n| unwrap_declarator(n, bytes)),
            _ => None,
        }
    }
    fn_node
        .child_by_field_name("declarator")
        .and_then(|d| unwrap_declarator(d, bytes))
}

#[cfg(feature = "tree-sitter-cpp")]
fn cpp_extract(source: &str) -> Result<Vec<symbols::Symbol>, String> {
    use tree_sitter::Parser;

    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_cpp::LANGUAGE.into())
        .map_err(|_| "failed to load C++ parser".to_string())?;

    let tree = parser
        .parse(source, None)
        .ok_or_else(|| "failed to parse source".to_string())?;

    let bytes = source.as_bytes();
    let mut found = Vec::new();
    let mut stack = vec![tree.root_node()];

    while let Some(node) = stack.pop() {
        let kind = node.kind();
        if matches!(
            kind,
            "function_definition"
                | "class_specifier"
                | "struct_specifier"
                | "enum_specifier"
                | "namespace_definition"
                | "template_declaration"
        ) {
            let name = if kind == "function_definition" {
                c_declarator_name_from_fn(node, bytes)
            } else {
                node_name(node, bytes)
            };
            if let Some(name) = name {
                found.push(symbols::Symbol {
                    name,
                    kind: kind.to_string(),
                    start_line: node.start_position().row + 1,
                    end_line: node.end_position().row + 1,
                    ..Default::default()
                });
            }
        }
        let mut i = 0;
        while let Some(c) = node.named_child(i) {
            stack.push(c);
            i += 1;
        }
    }

    Ok(found)
}

#[cfg(not(feature = "tree-sitter-cpp"))]
fn cpp_extract(_: &str) -> Result<Vec<symbols::Symbol>, String> {
    Err("no extractor registered for cpp".to_string())
}

#[cfg(feature = "tree-sitter-c-sharp")]
fn csharp_extract(source: &str) -> Result<Vec<symbols::Symbol>, String> {
    use tree_sitter::Parser;

    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_c_sharp::LANGUAGE.into())
        .map_err(|_| "failed to load C# parser".to_string())?;

    let tree = parser
        .parse(source, None)
        .ok_or_else(|| "failed to parse source".to_string())?;

    let bytes = source.as_bytes();
    let mut found = Vec::new();
    let mut stack = vec![tree.root_node()];

    while let Some(node) = stack.pop() {
        let kind = node.kind();
        if matches!(
            kind,
            "class_declaration"
                | "interface_declaration"
                | "enum_declaration"
                | "struct_declaration"
                | "record_declaration"
                | "method_declaration"
                | "constructor_declaration"
                | "namespace_declaration"
        ) {
            if let Some(name) = node_name(node, bytes) {
                found.push(symbols::Symbol {
                    name,
                    kind: kind.to_string(),
                    start_line: node.start_position().row + 1,
                    end_line: node.end_position().row + 1,
                    ..Default::default()
                });
            }
        }
        let mut i = 0;
        while let Some(c) = node.named_child(i) {
            stack.push(c);
            i += 1;
        }
    }

    Ok(found)
}

#[cfg(not(feature = "tree-sitter-c-sharp"))]
fn csharp_extract(_: &str) -> Result<Vec<symbols::Symbol>, String> {
    Err("no extractor registered for cs".to_string())
}

#[cfg(feature = "tree-sitter-php")]
fn php_extract(source: &str) -> Result<Vec<symbols::Symbol>, String> {
    use tree_sitter::Parser;

    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_php::LANGUAGE_PHP.into())
        .map_err(|_| "failed to load PHP parser".to_string())?;

    let tree = parser
        .parse(source, None)
        .ok_or_else(|| "failed to parse source".to_string())?;

    let bytes = source.as_bytes();
    let mut found = Vec::new();
    let mut stack = vec![tree.root_node()];

    while let Some(node) = stack.pop() {
        let kind = node.kind();
        if matches!(
            kind,
            "class_declaration"
                | "interface_declaration"
                | "enum_declaration"
                | "trait_declaration"
                | "function_definition"
                | "method_declaration"
        ) {
            if let Some(name) = node_name(node, bytes) {
                found.push(symbols::Symbol {
                    name,
                    kind: kind.to_string(),
                    start_line: node.start_position().row + 1,
                    end_line: node.end_position().row + 1,
                    ..Default::default()
                });
            }
        }
        let mut i = 0;
        while let Some(c) = node.named_child(i) {
            stack.push(c);
            i += 1;
        }
    }

    Ok(found)
}

#[cfg(not(feature = "tree-sitter-php"))]
fn php_extract(_: &str) -> Result<Vec<symbols::Symbol>, String> {
    Err("no extractor registered for php".to_string())
}

#[cfg(feature = "tree-sitter-ruby")]
fn ruby_extract(source: &str) -> Result<Vec<symbols::Symbol>, String> {
    use tree_sitter::Parser;

    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_ruby::LANGUAGE.into())
        .map_err(|_| "failed to load Ruby parser".to_string())?;

    let tree = parser
        .parse(source, None)
        .ok_or_else(|| "failed to parse source".to_string())?;

    let bytes = source.as_bytes();
    let mut found = Vec::new();
    let mut stack = vec![tree.root_node()];

    while let Some(node) = stack.pop() {
        let kind = node.kind();
        if matches!(kind, "method" | "singleton_method" | "class" | "module") {
            if let Some(name) = node_name(node, bytes) {
                found.push(symbols::Symbol {
                    name,
                    kind: kind.to_string(),
                    start_line: node.start_position().row + 1,
                    end_line: node.end_position().row + 1,
                    ..Default::default()
                });
            }
        }
        let mut i = 0;
        while let Some(c) = node.named_child(i) {
            stack.push(c);
            i += 1;
        }
    }

    Ok(found)
}

#[cfg(not(feature = "tree-sitter-ruby"))]
fn ruby_extract(_: &str) -> Result<Vec<symbols::Symbol>, String> {
    Err("no extractor registered for rb".to_string())
}

#[cfg(feature = "tree-sitter-swift")]
fn swift_extract(source: &str) -> Result<Vec<symbols::Symbol>, String> {
    use tree_sitter::Parser;

    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_swift::LANGUAGE.into())
        .map_err(|_| "failed to load Swift parser".to_string())?;

    let tree = parser
        .parse(source, None)
        .ok_or_else(|| "failed to parse source".to_string())?;

    let bytes = source.as_bytes();
    let mut found = Vec::new();
    let mut stack = vec![tree.root_node()];

    while let Some(node) = stack.pop() {
        let kind = node.kind();
        if matches!(
            kind,
            "function_declaration"
                | "class_declaration"
                | "struct_declaration"
                | "enum_declaration"
                | "protocol_declaration"
                | "extension_declaration"
        ) {
            if let Some(name) = node_name(node, bytes) {
                found.push(symbols::Symbol {
                    name,
                    kind: kind.to_string(),
                    start_line: node.start_position().row + 1,
                    end_line: node.end_position().row + 1,
                    ..Default::default()
                });
            }
        }
        let mut i = 0;
        while let Some(c) = node.named_child(i) {
            stack.push(c);
            i += 1;
        }
    }

    Ok(found)
}

#[cfg(not(feature = "tree-sitter-swift"))]
fn swift_extract(_: &str) -> Result<Vec<symbols::Symbol>, String> {
    Err("no extractor registered for swift".to_string())
}

#[cfg(feature = "tree-sitter-scala")]
fn scala_extract(source: &str) -> Result<Vec<symbols::Symbol>, String> {
    use tree_sitter::Parser;

    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_scala::LANGUAGE.into())
        .map_err(|_| "failed to load Scala parser".to_string())?;

    let tree = parser
        .parse(source, None)
        .ok_or_else(|| "failed to parse source".to_string())?;

    let bytes = source.as_bytes();
    let mut found = Vec::new();
    let mut stack = vec![tree.root_node()];

    while let Some(node) = stack.pop() {
        let kind = node.kind();
        if matches!(
            kind,
            "class_definition"
                | "object_definition"
                | "trait_definition"
                | "function_definition"
                | "val_definition"
        ) {
            if let Some(name) = node_name(node, bytes) {
                found.push(symbols::Symbol {
                    name,
                    kind: kind.to_string(),
                    start_line: node.start_position().row + 1,
                    end_line: node.end_position().row + 1,
                    ..Default::default()
                });
            }
        }
        let mut i = 0;
        while let Some(c) = node.named_child(i) {
            stack.push(c);
            i += 1;
        }
    }

    Ok(found)
}

#[cfg(not(feature = "tree-sitter-scala"))]
fn scala_extract(_: &str) -> Result<Vec<symbols::Symbol>, String> {
    Err("no extractor registered for scala".to_string())
}

#[cfg(feature = "tree-sitter-elixir")]
fn elixir_extract(source: &str) -> Result<Vec<symbols::Symbol>, String> {
    use tree_sitter::Parser;

    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_elixir::LANGUAGE.into())
        .map_err(|_| "failed to load Elixir parser".to_string())?;

    let tree = parser
        .parse(source, None)
        .ok_or_else(|| "failed to parse source".to_string())?;

    let bytes = source.as_bytes();
    let mut found = Vec::new();
    let mut stack = vec![tree.root_node()];

    while let Some(node) = stack.pop() {
        let kind = node.kind();
        if matches!(kind, "call") {
            if let Some(fn_node) = node.child_by_field_name("target") {
                let fn_name = fn_node.utf8_text(bytes).unwrap_or("");
                if matches!(
                    fn_name,
                    "def" | "defp" | "defmodule" | "defmacro" | "defmacrop"
                ) {
                    if let Some(args) = node.child_by_field_name("arguments") {
                        if let Some(first) = args.named_child(0) {
                            if let Ok(name) = first.utf8_text(bytes) {
                                found.push(symbols::Symbol {
                                    name: name.to_string(),
                                    kind: fn_name.to_string(),
                                    start_line: node.start_position().row + 1,
                                    end_line: node.end_position().row + 1,
                                    ..Default::default()
                                });
                            }
                        }
                    }
                }
            }
        }
        let mut i = 0;
        while let Some(c) = node.named_child(i) {
            stack.push(c);
            i += 1;
        }
    }

    Ok(found)
}

#[cfg(not(feature = "tree-sitter-elixir"))]
fn elixir_extract(_: &str) -> Result<Vec<symbols::Symbol>, String> {
    Err("no extractor registered for ex".to_string())
}

#[cfg(feature = "tree-sitter-lua")]
fn lua_extract(source: &str) -> Result<Vec<symbols::Symbol>, String> {
    use tree_sitter::Parser;

    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_lua::LANGUAGE.into())
        .map_err(|_| "failed to load Lua parser".to_string())?;

    let tree = parser
        .parse(source, None)
        .ok_or_else(|| "failed to parse source".to_string())?;

    let bytes = source.as_bytes();
    let mut found = Vec::new();
    let mut stack = vec![tree.root_node()];

    while let Some(node) = stack.pop() {
        let kind = node.kind();
        if matches!(kind, "function_declaration" | "local_function") {
            if let Some(name) = node_name(node, bytes) {
                found.push(symbols::Symbol {
                    name,
                    kind: kind.to_string(),
                    start_line: node.start_position().row + 1,
                    end_line: node.end_position().row + 1,
                    ..Default::default()
                });
            }
        }
        let mut i = 0;
        while let Some(c) = node.named_child(i) {
            stack.push(c);
            i += 1;
        }
    }

    Ok(found)
}

#[cfg(not(feature = "tree-sitter-lua"))]
fn lua_extract(_: &str) -> Result<Vec<symbols::Symbol>, String> {
    Err("no extractor registered for lua".to_string())
}

#[cfg(feature = "tree-sitter-haskell")]
fn haskell_extract(source: &str) -> Result<Vec<symbols::Symbol>, String> {
    use tree_sitter::Parser;

    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_haskell::LANGUAGE.into())
        .map_err(|_| "failed to load Haskell parser".to_string())?;

    let tree = parser
        .parse(source, None)
        .ok_or_else(|| "failed to parse source".to_string())?;

    let bytes = source.as_bytes();
    let mut found = Vec::new();
    let mut stack = vec![tree.root_node()];

    while let Some(node) = stack.pop() {
        let kind = node.kind();
        if matches!(
            kind,
            "function"
                | "data_type"
                | "newtype"
                | "type_synonym"
                | "class_declaration"
                | "instance"
        ) {
            if let Some(name) = node_name(node, bytes) {
                found.push(symbols::Symbol {
                    name,
                    kind: kind.to_string(),
                    start_line: node.start_position().row + 1,
                    end_line: node.end_position().row + 1,
                    ..Default::default()
                });
            }
        }
        let mut i = 0;
        while let Some(c) = node.named_child(i) {
            stack.push(c);
            i += 1;
        }
    }

    Ok(found)
}

#[cfg(not(feature = "tree-sitter-haskell"))]
fn haskell_extract(_: &str) -> Result<Vec<symbols::Symbol>, String> {
    Err("no extractor registered for hs".to_string())
}

#[derive(Debug, Clone, Default)]
pub struct Symbol {
    pub name: String,
    pub kind: String,
    pub start_line: usize,
    pub end_line: usize,
    pub signature: Option<String>,
    pub return_type: Option<String>,
    pub visibility: Option<String>,
    pub is_async: bool,
    pub complexity: Option<i64>,
    pub decorators: Vec<String>,
}

#[cfg(feature = "tree-sitter-rust")]
pub fn extract_rust_symbols(source: &str) -> Result<Vec<Symbol>, String> {
    use tree_sitter::Parser;
    use tree_sitter_rust::LANGUAGE;

    let mut parser = Parser::new();
    parser
        .set_language(&LANGUAGE.into())
        .map_err(|_| "failed to load tree-sitter rust language".to_string())?;

    let tree = parser
        .parse(source, None)
        .ok_or_else(|| "failed to parse source".to_string())?;

    let root = tree.root_node();
    let bytes = source.as_bytes();
    let mut found = Vec::new();

    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        let kind = node.kind();
        let is_symbol = matches!(
            kind,
            "function_item"
                | "struct_item"
                | "enum_item"
                | "trait_item"
                | "impl_item"
                | "mod_item"
                | "const_item"
                | "type_item"
        );

        if is_symbol {
            let name = node
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(bytes).ok())
                .or_else(|| {
                    node.named_children(&mut node.walk())
                        .find_map(|c| match c.kind() {
                            "identifier" | "type_identifier" => c.utf8_text(bytes).ok(),
                            _ => None,
                        })
                })
                .unwrap_or("")
                .to_string();

            if !name.is_empty() {
                // Visibility: look for a `visibility_modifier` child node
                let visibility = node
                    .named_children(&mut node.walk())
                    .find(|c| c.kind() == "visibility_modifier")
                    .and_then(|c| c.utf8_text(bytes).ok())
                    .map(|s| s.to_string());

                // is_async: check if the function source text contains "async fn"
                let is_async = if kind == "function_item" {
                    let node_text = node.utf8_text(bytes).unwrap_or("");
                    // Look for "async" before the "fn" keyword at the start
                    let fn_pos = node_text.find("fn ").unwrap_or(0);
                    let prefix = &node_text[..fn_pos];
                    prefix.split_whitespace().any(|w| w == "async")
                } else {
                    false
                };

                // Parameters → signature
                let signature = node
                    .child_by_field_name("parameters")
                    .and_then(|c| c.utf8_text(bytes).ok())
                    .map(|s| s.to_string());

                // Return type
                let return_type = node
                    .child_by_field_name("return_type")
                    .and_then(|c| c.utf8_text(bytes).ok())
                    .map(|s| s.to_string());

                // Decorators: attributes (#[...]) are siblings that precede the node — skip for now
                // (tree-sitter-rust represents attributes as separate top-level nodes)
                let decorators = vec![];

                // Complexity: count branching nodes inside the body
                let complexity = if kind == "function_item" {
                    let body = node.child_by_field_name("body");
                    Some(1 + body.map(|b| count_complexity(&b)).unwrap_or(0))
                } else {
                    None
                };

                found.push(Symbol {
                    name,
                    kind: kind.to_string(),
                    start_line: node.start_position().row + 1,
                    end_line: node.end_position().row + 1,
                    visibility,
                    is_async,
                    signature,
                    return_type,
                    decorators,
                    complexity,
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

/// Count branching nodes (if, match, for, while, loop) in a subtree.
/// Baseline 1 is added by the caller.
fn count_complexity(node: &tree_sitter::Node) -> i64 {
    let mut count = 0i64;
    let mut stack = vec![*node];
    while let Some(n) = stack.pop() {
        if matches!(
            n.kind(),
            "if_expression"
                | "match_expression"
                | "for_expression"
                | "while_expression"
                | "loop_expression"
        ) {
            count += 1;
        }
        let mut i = 0;
        while let Some(c) = n.named_child(i) {
            stack.push(c);
            i += 1;
        }
    }
    count
}

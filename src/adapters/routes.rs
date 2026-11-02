use tree_sitter::Node;

/// A route extracted from source code.
#[derive(Debug, Clone)]
pub struct ExtractedRoute {
    pub method: Option<String>,
    pub path: String,
    pub handler_name: Option<String>,
    pub framework: Option<String>,
    pub line: usize,
}

/// Try to extract routes from source content based on file extension.
/// Returns an empty vec if the extension is not supported.
pub fn extract_routes_for_extension(ext: &str, content: &str) -> Vec<ExtractedRoute> {
    match ext {
        "js" | "mjs" | "cjs" => extract_javascript_routes(content),
        "ts" | "tsx" => extract_typescript_routes(content),
        "py" => extract_python_routes(content),
        "rs" => extract_rust_routes(content),
        "java" => extract_java_routes(content),
        "go" => extract_go_routes(content),
        "cs" => extract_csharp_routes(content),
        "rb" => extract_ruby_routes(content),
        "php" => extract_php_routes(content),
        _ => vec![],
    }
}

/// Strip the surrounding quote characters from a string-literal node's text.
fn string_literal_text(node: Node, bytes: &[u8]) -> Option<String> {
    let raw = node.utf8_text(bytes).ok()?;
    Some(
        raw.trim()
            .trim_matches('"')
            .trim_matches('\'')
            .trim_matches('`')
            .to_string(),
    )
}

// ---------------------------------------------------------------------------
// Express (JavaScript / TypeScript)
// ---------------------------------------------------------------------------

#[cfg(feature = "tree-sitter-javascript")]
pub fn extract_javascript_routes(source: &str) -> Vec<ExtractedRoute> {
    use tree_sitter::Parser;
    use tree_sitter_javascript::LANGUAGE;

    let mut parser = Parser::new();
    if parser.set_language(&LANGUAGE.into()).is_err() {
        return vec![];
    }
    let tree = match parser.parse(source, None) {
        Some(t) => t,
        None => return vec![],
    };
    let bytes = source.as_bytes();
    let mut out = Vec::new();
    collect_express_routes(tree.root_node(), bytes, &mut out);
    out
}

#[cfg(not(feature = "tree-sitter-javascript"))]
pub fn extract_javascript_routes(_source: &str) -> Vec<ExtractedRoute> {
    vec![]
}

#[cfg(feature = "tree-sitter-typescript")]
pub fn extract_typescript_routes(source: &str) -> Vec<ExtractedRoute> {
    use tree_sitter::Parser;
    use tree_sitter_typescript::LANGUAGE_TYPESCRIPT;

    let mut parser = Parser::new();
    if parser.set_language(&LANGUAGE_TYPESCRIPT.into()).is_err() {
        return vec![];
    }
    let tree = match parser.parse(source, None) {
        Some(t) => t,
        None => return vec![],
    };
    let bytes = source.as_bytes();
    let mut out = Vec::new();
    collect_express_routes(tree.root_node(), bytes, &mut out);
    out
}

#[cfg(not(feature = "tree-sitter-typescript"))]
pub fn extract_typescript_routes(_source: &str) -> Vec<ExtractedRoute> {
    vec![]
}

#[cfg(any(feature = "tree-sitter-javascript", feature = "tree-sitter-typescript"))]
fn collect_express_routes(node: Node, bytes: &[u8], out: &mut Vec<ExtractedRoute>) {
    if node.kind() == "call_expression" {
        if let Some(route) = express_route_from_call(node, bytes) {
            out.push(route);
        }
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_express_routes(child, bytes, out);
    }
}

#[cfg(any(feature = "tree-sitter-javascript", feature = "tree-sitter-typescript"))]
fn express_route_from_call(node: Node, bytes: &[u8]) -> Option<ExtractedRoute> {
    const METHODS: &[&str] = &["get", "post", "put", "delete", "patch", "use"];

    let func = node.child_by_field_name("function")?;
    if func.kind() != "member_expression" {
        return None;
    }
    let object = func.child_by_field_name("object")?;
    if !matches!(object.kind(), "identifier" | "this") {
        return None;
    }
    let property = func.child_by_field_name("property")?;
    let method_name = property.utf8_text(bytes).ok()?;
    if !METHODS.contains(&method_name) {
        return None;
    }

    let args = node.child_by_field_name("arguments")?;
    let mut cursor = args.walk();
    let arg_nodes: Vec<Node> = args.named_children(&mut cursor).collect();
    let path_node = arg_nodes.first()?;
    if path_node.kind() != "string" {
        return None;
    }
    let path = string_literal_text(*path_node, bytes)?;

    let handler_name = arg_nodes.iter().skip(1).rev().find_map(|a| match a.kind() {
        "identifier" => a.utf8_text(bytes).ok().map(str::to_string),
        _ => None,
    });

    let method = if method_name == "use" {
        Some("*".to_string())
    } else {
        Some(method_name.to_uppercase())
    };

    Some(ExtractedRoute {
        method,
        path,
        handler_name,
        framework: Some("express".to_string()),
        line: node.start_position().row + 1,
    })
}

// ---------------------------------------------------------------------------
// FastAPI / Flask (Python)
// ---------------------------------------------------------------------------

#[cfg(feature = "tree-sitter-python")]
pub fn extract_python_routes(source: &str) -> Vec<ExtractedRoute> {
    use tree_sitter::Parser;
    use tree_sitter_python::LANGUAGE;

    let mut parser = Parser::new();
    if parser.set_language(&LANGUAGE.into()).is_err() {
        return vec![];
    }
    let tree = match parser.parse(source, None) {
        Some(t) => t,
        None => return vec![],
    };
    let bytes = source.as_bytes();
    let mut out = Vec::new();
    collect_python_routes(tree.root_node(), bytes, &mut out);
    out
}

#[cfg(not(feature = "tree-sitter-python"))]
pub fn extract_python_routes(_source: &str) -> Vec<ExtractedRoute> {
    vec![]
}

#[cfg(feature = "tree-sitter-python")]
fn collect_python_routes(node: Node, bytes: &[u8], out: &mut Vec<ExtractedRoute>) {
    if node.kind() == "decorated_definition" {
        let handler_name = node
            .child_by_field_name("definition")
            .and_then(|def| def.child_by_field_name("name"))
            .and_then(|n| n.utf8_text(bytes).ok())
            .map(str::to_string);

        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if child.kind() == "decorator" {
                if let Some(route) =
                    python_route_from_decorator(child, bytes, handler_name.clone())
                {
                    out.push(route);
                }
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_python_routes(child, bytes, out);
    }
}

#[cfg(feature = "tree-sitter-python")]
fn python_route_from_decorator(
    deco: Node,
    bytes: &[u8],
    handler_name: Option<String>,
) -> Option<ExtractedRoute> {
    const METHODS: &[&str] = &["get", "post", "put", "delete", "patch"];

    let expr = deco.named_child(0)?;
    if expr.kind() != "call" {
        return None;
    }
    let func = expr.child_by_field_name("function")?;
    if func.kind() != "attribute" {
        return None;
    }
    let attr = func.child_by_field_name("attribute")?;
    let attr_name = attr.utf8_text(bytes).ok()?;

    let args = expr.child_by_field_name("arguments")?;
    let mut cursor = args.walk();
    let path = args
        .named_children(&mut cursor)
        .find(|a| a.kind() == "string")
        .and_then(|s| string_literal_text(s, bytes))?;
    let line = deco.start_position().row + 1;

    if METHODS.contains(&attr_name) {
        return Some(ExtractedRoute {
            method: Some(attr_name.to_uppercase()),
            path,
            handler_name,
            framework: Some("fastapi".to_string()),
            line,
        });
    }

    if attr_name == "route" {
        let mut cursor = args.walk();
        let method = args
            .named_children(&mut cursor)
            .find(|a| a.kind() == "keyword_argument")
            .and_then(|kw| {
                let name = kw.child_by_field_name("name")?.utf8_text(bytes).ok()?;
                if name != "methods" {
                    return None;
                }
                let value = kw.child_by_field_name("value")?;
                let mut vc = value.walk();
                let found = value
                    .named_children(&mut vc)
                    .find(|v| v.kind() == "string")
                    .and_then(|s| string_literal_text(s, bytes));
                found
            })
            .map(|m| m.to_uppercase())
            .unwrap_or_else(|| "*".to_string());
        return Some(ExtractedRoute {
            method: Some(method),
            path,
            handler_name,
            framework: Some("flask".to_string()),
            line,
        });
    }

    None
}

// ---------------------------------------------------------------------------
// Actix-web (Rust)
// ---------------------------------------------------------------------------

#[cfg(feature = "tree-sitter-rust")]
pub fn extract_rust_routes(source: &str) -> Vec<ExtractedRoute> {
    use tree_sitter::Parser;
    use tree_sitter_rust::LANGUAGE;

    let mut parser = Parser::new();
    if parser.set_language(&LANGUAGE.into()).is_err() {
        return vec![];
    }
    let tree = match parser.parse(source, None) {
        Some(t) => t,
        None => return vec![],
    };
    let bytes = source.as_bytes();
    // `#[get("/x")]` is the same syntax in Actix-web and Rocket; only the imports differ.
    let attr_framework = if source.contains("rocket") {
        "rocket"
    } else {
        "actix"
    };
    let mut out = Vec::new();
    collect_rust_routes(tree.root_node(), bytes, attr_framework, &mut out);
    out
}

#[cfg(not(feature = "tree-sitter-rust"))]
pub fn extract_rust_routes(_source: &str) -> Vec<ExtractedRoute> {
    vec![]
}

#[cfg(feature = "tree-sitter-rust")]
fn collect_rust_routes(
    node: Node,
    bytes: &[u8],
    attr_framework: &str,
    out: &mut Vec<ExtractedRoute>,
) {
    if node.kind() == "attribute_item" {
        if let Some(route) = rust_route_from_attribute_item(node, bytes, attr_framework) {
            out.push(route);
        }
    } else if node.kind() == "call_expression" {
        out.extend(rust_routes_from_call(node, bytes));
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_rust_routes(child, bytes, attr_framework, out);
    }
}

#[cfg(feature = "tree-sitter-rust")]
fn rust_route_from_attribute_item(
    item: Node,
    bytes: &[u8],
    attr_framework: &str,
) -> Option<ExtractedRoute> {
    const METHODS: &[&str] = &["get", "post", "put", "delete", "patch"];

    let attr = item.named_child(0).filter(|n| n.kind() == "attribute")?;
    let path_node = attr.named_child(0)?;
    let method_name = path_node.utf8_text(bytes).ok()?;
    if !METHODS.contains(&method_name) {
        return None;
    }

    let args = attr.child_by_field_name("arguments")?;
    let mut cursor = args.walk();
    let path = args
        .named_children(&mut cursor)
        .find(|a| matches!(a.kind(), "string_literal" | "raw_string_literal"))
        .and_then(|s| string_literal_text(s, bytes))?;

    // The next sibling item (skipping further attributes) should be the handler fn.
    let mut sibling = item.next_named_sibling();
    let mut handler_name = None;
    while let Some(s) = sibling {
        if s.kind() == "attribute_item" {
            sibling = s.next_named_sibling();
            continue;
        }
        if matches!(s.kind(), "function_item") {
            handler_name = s
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(bytes).ok())
                .map(str::to_string);
        }
        break;
    }

    Some(ExtractedRoute {
        method: Some(method_name.to_uppercase()),
        path,
        handler_name,
        framework: Some(attr_framework.to_string()),
        line: item.start_position().row + 1,
    })
}

#[cfg(feature = "tree-sitter-rust")]
const RUST_VERBS: &[&str] = &["get", "post", "put", "delete", "patch", "head", "options", "trace"];

/// Builder-style routes: Axum `.route("/p", get(h).post(c))` and Actix-web
/// `.route("/p", web::get().to(h))`. One route per HTTP verb in the chain.
#[cfg(feature = "tree-sitter-rust")]
fn rust_routes_from_call(call: Node, bytes: &[u8]) -> Vec<ExtractedRoute> {
    let Some(func) = call.child_by_field_name("function") else {
        return vec![];
    };
    if func.kind() != "field_expression" {
        return vec![];
    }
    let Some(field) = func.child_by_field_name("field") else {
        return vec![];
    };
    if field.utf8_text(bytes).ok() != Some("route") {
        return vec![];
    }
    let Some(args) = call.child_by_field_name("arguments") else {
        return vec![];
    };
    let mut cursor = args.walk();
    let mut positional = args.named_children(&mut cursor);
    let Some(path) = positional
        .next()
        .filter(|a| a.kind() == "string_literal")
        .and_then(|s| string_literal_text(s, bytes))
    else {
        return vec![];
    };
    let Some(router) = positional.next() else {
        return vec![];
    };

    let mut verbs: Vec<(String, Option<String>, &'static str)> = Vec::new();
    rust_method_router(router, bytes, &mut verbs);
    verbs
        .into_iter()
        .map(|(verb, handler_name, framework)| ExtractedRoute {
            method: Some(verb.to_uppercase()),
            path: path.clone(),
            handler_name,
            framework: Some(framework.to_string()),
            line: field.start_position().row + 1,
        })
        .collect()
}

/// Walk a method-router expression collecting `(verb, handler, framework)`:
/// `get(h)`, `routing::get(h).post(c)` (Axum) and `web::get().to(h)` (Actix).
#[cfg(feature = "tree-sitter-rust")]
fn rust_method_router(
    expr: Node,
    bytes: &[u8],
    out: &mut Vec<(String, Option<String>, &'static str)>,
) {
    if expr.kind() != "call_expression" {
        return;
    }
    let (Some(func), Some(args)) = (
        expr.child_by_field_name("function"),
        expr.child_by_field_name("arguments"),
    ) else {
        return;
    };
    let first_arg_handler = || {
        args.named_child(0)
            .and_then(|a| rust_handler_name(a, bytes))
    };
    match func.kind() {
        // `get(h)` / `routing::get(h)`
        "identifier" | "scoped_identifier" => {
            let verb = if func.kind() == "identifier" {
                func.utf8_text(bytes).ok()
            } else {
                func.child_by_field_name("name")
                    .and_then(|n| n.utf8_text(bytes).ok())
            };
            if let Some(verb) = verb.filter(|v| RUST_VERBS.contains(v)) {
                out.push((verb.to_string(), first_arg_handler(), "axum"));
            }
        }
        "field_expression" => {
            let name = func
                .child_by_field_name("field")
                .and_then(|n| n.utf8_text(bytes).ok());
            let Some(receiver) = func.child_by_field_name("value") else {
                return;
            };
            match name {
                // `<router>.post(c)`: another verb chained on an Axum method router.
                Some(verb) if RUST_VERBS.contains(&verb) => {
                    out.push((verb.to_string(), first_arg_handler(), "axum"));
                    rust_method_router(receiver, bytes, out);
                }
                // `web::get().to(h)`: the verb is on the receiver, the handler on `.to`.
                Some("to") => {
                    let verb = receiver
                        .child_by_field_name("function")
                        .filter(|f| f.kind() == "scoped_identifier")
                        .and_then(|f| {
                            let path = f.child_by_field_name("path")?.utf8_text(bytes).ok()?;
                            let name = f.child_by_field_name("name")?.utf8_text(bytes).ok()?;
                            (path == "web" && RUST_VERBS.contains(&name)).then_some(name)
                        });
                    if let Some(verb) = verb {
                        out.push((verb.to_string(), first_arg_handler(), "actix"));
                    }
                }
                _ => {}
            }
        }
        _ => {}
    }
}

/// A handler passed by name: `list` or `handlers::list`. Closures have none.
#[cfg(feature = "tree-sitter-rust")]
fn rust_handler_name(arg: Node, bytes: &[u8]) -> Option<String> {
    let node = match arg.kind() {
        "identifier" => arg,
        "scoped_identifier" => arg.child_by_field_name("name")?,
        _ => return None,
    };
    node.utf8_text(bytes).ok().map(str::to_string)
}

// ---------------------------------------------------------------------------
// Spring MVC (Java)
// ---------------------------------------------------------------------------

#[cfg(feature = "tree-sitter-java")]
pub fn extract_java_routes(source: &str) -> Vec<ExtractedRoute> {
    use tree_sitter::Parser;
    use tree_sitter_java::LANGUAGE;

    let mut parser = Parser::new();
    if parser.set_language(&LANGUAGE.into()).is_err() {
        return vec![];
    }
    let tree = match parser.parse(source, None) {
        Some(t) => t,
        None => return vec![],
    };
    let bytes = source.as_bytes();
    let mut out = Vec::new();
    collect_java_routes(tree.root_node(), bytes, "", &mut out);
    out
}

#[cfg(not(feature = "tree-sitter-java"))]
pub fn extract_java_routes(_source: &str) -> Vec<ExtractedRoute> {
    vec![]
}

#[cfg(feature = "tree-sitter-java")]
fn collect_java_routes(node: Node, bytes: &[u8], class_prefix: &str, out: &mut Vec<ExtractedRoute>) {
    if node.kind() == "class_declaration" {
        let mut prefix = String::new();
        if let Some(mods) = java_modifiers(node) {
            let mut cursor = mods.walk();
            for anno in mods.named_children(&mut cursor) {
                if let Some((name, p, _)) = java_mapping_annotation(anno, bytes) {
                    if name == "RequestMapping" {
                        if let Some(p) = p {
                            prefix = p;
                        }
                    }
                }
            }
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            collect_java_routes(child, bytes, &prefix, out);
        }
        return;
    }

    if node.kind() == "method_declaration" {
        if let Some(mods) = java_modifiers(node) {
            let mut cursor = mods.walk();
            for anno in mods.named_children(&mut cursor) {
                if let Some((name, path, method)) = java_mapping_annotation(anno, bytes) {
                    let Some(http_method) = spring_annotation_method(&name, method.as_deref())
                    else {
                        continue;
                    };
                    let suffix = path.unwrap_or_default();
                    let full_path = join_path(class_prefix, &suffix);
                    let handler_name = node
                        .child_by_field_name("name")
                        .and_then(|n| n.utf8_text(bytes).ok())
                        .map(str::to_string);
                    out.push(ExtractedRoute {
                        method: Some(http_method),
                        path: full_path,
                        handler_name,
                        framework: Some("spring".to_string()),
                        line: anno.start_position().row + 1,
                    });
                }
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_java_routes(child, bytes, class_prefix, out);
    }
}

/// Find the `modifiers` child of a class/method declaration, if present.
#[cfg(feature = "tree-sitter-java")]
fn java_modifiers(node: Node) -> Option<Node> {
    let mut cursor = node.walk();
    let found = node.children(&mut cursor).find(|c| c.kind() == "modifiers");
    found
}

/// Parse a Spring mapping annotation node into `(annotation_name, path, method_enum)`.
/// `method_enum` is only populated for `@RequestMapping(method = RequestMethod.GET)`.
#[cfg(feature = "tree-sitter-java")]
fn java_mapping_annotation(anno: Node, bytes: &[u8]) -> Option<(String, Option<String>, Option<String>)> {
    if !matches!(anno.kind(), "annotation" | "marker_annotation") {
        return None;
    }
    let name = anno
        .child_by_field_name("name")
        .and_then(|n| n.utf8_text(bytes).ok())?
        .to_string();
    if !name.ends_with("Mapping") {
        return None;
    }

    let Some(args) = anno.child_by_field_name("arguments") else {
        return Some((name, None, None));
    };

    let mut path = None;
    let mut method = None;
    let mut cursor = args.walk();
    for arg in args.named_children(&mut cursor) {
        match arg.kind() {
            "string_literal" => {
                path = string_literal_text(arg, bytes);
            }
            "element_value_pair" => {
                let key = arg
                    .child_by_field_name("key")
                    .and_then(|n| n.utf8_text(bytes).ok())
                    .unwrap_or("");
                let value = arg.child_by_field_name("value");
                match key {
                    "value" | "path" => {
                        path = value.and_then(|v| java_first_string(v, bytes));
                    }
                    "method" => {
                        method = value.and_then(|v| java_request_method_name(v, bytes));
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    Some((name, path, method))
}

/// Extract the first string literal from a value node, whether it's a bare
/// string or a `{"...", ...}` array initializer.
#[cfg(feature = "tree-sitter-java")]
fn java_first_string(node: Node, bytes: &[u8]) -> Option<String> {
    if node.kind() == "string_literal" {
        return string_literal_text(node, bytes);
    }
    let mut cursor = node.walk();
    let found = node
        .named_children(&mut cursor)
        .find(|c| c.kind() == "string_literal")
        .and_then(|s| string_literal_text(s, bytes));
    found
}

/// Pull the trailing identifier out of a `RequestMethod.GET`-style field access
/// (also handles a bare `GET` and an array like `{RequestMethod.GET}`).
#[cfg(feature = "tree-sitter-java")]
fn java_request_method_name(node: Node, bytes: &[u8]) -> Option<String> {
    match node.kind() {
        "field_access" => node
            .child_by_field_name("field")
            .and_then(|n| n.utf8_text(bytes).ok())
            .map(str::to_string),
        "identifier" => node.utf8_text(bytes).ok().map(str::to_string),
        _ => {
            let mut cursor = node.walk();
            let found = node
                .named_children(&mut cursor)
                .find_map(|c| java_request_method_name(c, bytes));
            found
        }
    }
}

#[cfg(feature = "tree-sitter-java")]
fn spring_annotation_method(annotation_name: &str, explicit_method: Option<&str>) -> Option<String> {
    match annotation_name {
        "GetMapping" => Some("GET".to_string()),
        "PostMapping" => Some("POST".to_string()),
        "PutMapping" => Some("PUT".to_string()),
        "DeleteMapping" => Some("DELETE".to_string()),
        "PatchMapping" => Some("PATCH".to_string()),
        "RequestMapping" => Some(
            explicit_method
                .map(|m| m.to_uppercase())
                .unwrap_or_else(|| "*".to_string()),
        ),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// ASP.NET Core (C#): attribute-routed controllers and minimal APIs
// ---------------------------------------------------------------------------

#[cfg(feature = "tree-sitter-c-sharp")]
pub fn extract_csharp_routes(source: &str) -> Vec<ExtractedRoute> {
    use tree_sitter::Parser;
    use tree_sitter_c_sharp::LANGUAGE;

    let mut parser = Parser::new();
    if parser.set_language(&LANGUAGE.into()).is_err() {
        return vec![];
    }
    let tree = match parser.parse(source, None) {
        Some(t) => t,
        None => return vec![],
    };
    let mut out = Vec::new();
    collect_csharp_routes(tree.root_node(), source.as_bytes(), "", &mut out);
    out
}

#[cfg(not(feature = "tree-sitter-c-sharp"))]
pub fn extract_csharp_routes(_source: &str) -> Vec<ExtractedRoute> {
    vec![]
}

/// `(attribute name without any `Attribute` suffix, first positional string argument)`
/// for every attribute on a declaration.
#[cfg(feature = "tree-sitter-c-sharp")]
fn csharp_attributes(decl: Node, bytes: &[u8]) -> Vec<(String, Option<String>, usize)> {
    let mut out = Vec::new();
    let mut cursor = decl.walk();
    for list in decl
        .named_children(&mut cursor)
        .filter(|c| c.kind() == "attribute_list")
    {
        let mut inner = list.walk();
        for attr in list
            .named_children(&mut inner)
            .filter(|c| c.kind() == "attribute")
        {
            let Some(name) = attr
                .named_child(0)
                .and_then(|n| n.utf8_text(bytes).ok())
                .map(|n| n.rsplit('.').next().unwrap_or(n))
                .map(|n| n.strip_suffix("Attribute").unwrap_or(n).to_string())
            else {
                continue;
            };
            // First positional string argument; `Name = "x"` style arguments are skipped.
            let mut path = None;
            let mut args_cursor = attr.walk();
            for args in attr
                .named_children(&mut args_cursor)
                .filter(|c| c.kind() == "attribute_argument_list")
            {
                let mut a = args.walk();
                path = args
                    .named_children(&mut a)
                    .filter(|arg| arg.named_child_count() == 1)
                    .filter_map(|arg| arg.named_child(0))
                    .find(|n| n.kind() == "string_literal")
                    .and_then(|s| string_literal_text(s, bytes));
            }
            out.push((name, path, attr.start_position().row + 1));
        }
    }
    out
}

#[cfg(feature = "tree-sitter-c-sharp")]
fn collect_csharp_routes(
    node: Node,
    bytes: &[u8],
    class_prefix: &str,
    out: &mut Vec<ExtractedRoute>,
) {
    if node.kind() == "class_declaration" {
        let controller = node
            .child_by_field_name("name")
            .and_then(|n| n.utf8_text(bytes).ok())
            .map(|n| n.strip_suffix("Controller").unwrap_or(n).to_string())
            .unwrap_or_default();
        let prefix = csharp_attributes(node, bytes)
            .into_iter()
            .find(|(name, _, _)| name == "Route")
            .and_then(|(_, path, _)| path)
            .unwrap_or_default()
            .replace("[controller]", &controller);
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            collect_csharp_routes(child, bytes, &prefix, out);
        }
        return;
    }

    if node.kind() == "method_declaration" {
        let handler_name = node
            .child_by_field_name("name")
            .and_then(|n| n.utf8_text(bytes).ok())
            .map(str::to_string);
        let attrs = csharp_attributes(node, bytes);
        // A verb attribute without its own path falls back to a sibling `[Route]`.
        let route_path = attrs
            .iter()
            .find(|(name, _, _)| name == "Route")
            .and_then(|(_, path, _)| path.clone());
        for (name, path, line) in &attrs {
            let Some(verb) = name.strip_prefix("Http").map(str::to_uppercase) else {
                continue;
            };
            if !["GET", "POST", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS"].contains(&verb.as_str())
            {
                continue;
            }
            let suffix = path.clone().or_else(|| route_path.clone()).unwrap_or_default();
            let suffix = suffix.replace("[action]", handler_name.as_deref().unwrap_or(""));
            // A leading `/` (or `~/`) makes the method route absolute: the class prefix is dropped.
            let full_path = if suffix.starts_with('/') || suffix.starts_with("~/") {
                format!("/{}", suffix.trim_start_matches("~/").trim_start_matches('/'))
            } else {
                join_path(class_prefix, &suffix)
            };
            out.push(ExtractedRoute {
                method: Some(verb),
                path: full_path,
                handler_name: handler_name.clone(),
                framework: Some("aspnet".to_string()),
                line: *line,
            });
        }
    }

    if node.kind() == "invocation_expression" {
        if let Some(route) = csharp_minimal_api_route(node, bytes) {
            out.push(route);
        }
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_csharp_routes(child, bytes, class_prefix, out);
    }
}

/// Minimal API: `app.MapGet("/p", Handler)`, `endpoints.MapPost("/p", () => ...)`.
/// Routes registered through a `MapGroup(..)` variable are not composed: that
/// would need data flow, not syntax.
#[cfg(feature = "tree-sitter-c-sharp")]
fn csharp_minimal_api_route(call: Node, bytes: &[u8]) -> Option<ExtractedRoute> {
    let func = call
        .child_by_field_name("function")
        .filter(|f| f.kind() == "member_access_expression")?;
    let member = func.child_by_field_name("name")?.utf8_text(bytes).ok()?;
    let verb = member.strip_prefix("Map")?.to_uppercase();
    if !["GET", "POST", "PUT", "DELETE", "PATCH"].contains(&verb.as_str()) {
        return None;
    }
    let args = call.child_by_field_name("arguments")?;
    let mut cursor = args.walk();
    let mut positional = args
        .named_children(&mut cursor)
        .filter(|a| a.kind() == "argument")
        .filter_map(|a| a.named_child(0));
    let path = positional
        .next()
        .filter(|n| n.kind() == "string_literal")
        .and_then(|s| string_literal_text(s, bytes))?;
    let handler_name = positional.next().and_then(|h| match h.kind() {
        "identifier" => h.utf8_text(bytes).ok().map(str::to_string),
        "member_access_expression" => h
            .child_by_field_name("name")
            .and_then(|n| n.utf8_text(bytes).ok())
            .map(str::to_string),
        _ => None,
    });
    Some(ExtractedRoute {
        method: Some(verb),
        path,
        handler_name,
        framework: Some("aspnet".to_string()),
        line: call.start_position().row + 1,
    })
}

// ---------------------------------------------------------------------------
// Ruby: Rails routing DSL and Sinatra
// ---------------------------------------------------------------------------

#[cfg(feature = "tree-sitter-ruby")]
pub fn extract_ruby_routes(source: &str) -> Vec<ExtractedRoute> {
    use tree_sitter::Parser;
    use tree_sitter_ruby::LANGUAGE;

    let mut parser = Parser::new();
    if parser.set_language(&LANGUAGE.into()).is_err() {
        return vec![];
    }
    let tree = match parser.parse(source, None) {
        Some(t) => t,
        None => return vec![],
    };
    let ctx = RubyCtx {
        prefix: String::new(),
        in_draw: false,
        resource: None,
        active: None,
        // `get '/x' do ... end` is Sinatra, but only trust it when the file says so.
        sinatra: source.contains("sinatra") || source.contains("Sinatra"),
    };
    let mut out = Vec::new();
    collect_ruby_routes(tree.root_node(), source.as_bytes(), &ctx, &mut out);
    out
}

#[cfg(not(feature = "tree-sitter-ruby"))]
pub fn extract_ruby_routes(_source: &str) -> Vec<ExtractedRoute> {
    vec![]
}

/// Lexical routing context while walking a Ruby file.
#[cfg(feature = "tree-sitter-ruby")]
#[derive(Clone)]
struct RubyCtx {
    /// Accumulated `namespace` / `scope` / nested-resource path prefix.
    prefix: String,
    /// Inside a `routes.draw do ... end` block. Rails verbs count only here:
    /// `get '/users'` also appears in request specs and HTTP clients.
    in_draw: bool,
    /// `(collection path, member path)` of the enclosing `resources`, for `member` / `collection`.
    resource: Option<(String, String)>,
    /// The prefix a verb extends inside `member do ... end` / `collection do ... end`.
    active: Option<String>,
    sinatra: bool,
}

#[cfg(feature = "tree-sitter-ruby")]
const RAILS_VERBS: &[&str] = &["get", "post", "put", "patch", "delete"];

/// `(action, HTTP verb, path suffix)` for `resources :things`.
#[cfg(feature = "tree-sitter-ruby")]
const RAILS_RESOURCES: &[(&str, &str, &str)] = &[
    ("index", "GET", ""),
    ("create", "POST", ""),
    ("new", "GET", "/new"),
    ("edit", "GET", "/:id/edit"),
    ("show", "GET", "/:id"),
    ("update", "PATCH", "/:id"),
    ("update", "PUT", "/:id"),
    ("destroy", "DELETE", "/:id"),
];

/// The same for singular `resource :thing`.
#[cfg(feature = "tree-sitter-ruby")]
const RAILS_SINGULAR_RESOURCE: &[(&str, &str, &str)] = &[
    ("create", "POST", ""),
    ("new", "GET", "/new"),
    ("edit", "GET", "/edit"),
    ("show", "GET", ""),
    ("update", "PATCH", ""),
    ("update", "PUT", ""),
    ("destroy", "DELETE", ""),
];

/// Text of a string or symbol node, without quotes or the leading `:`.
#[cfg(feature = "tree-sitter-ruby")]
fn ruby_text(node: Node, bytes: &[u8]) -> Option<String> {
    let raw = node.utf8_text(bytes).ok()?;
    match node.kind() {
        "string" if !raw.contains("#{") => string_literal_text(node, bytes),
        "simple_symbol" => Some(raw.trim_start_matches(':').to_string()),
        "hash_key_symbol" => Some(raw.to_string()),
        _ => None,
    }
}

#[cfg(feature = "tree-sitter-ruby")]
fn ruby_call_args(call: Node) -> Vec<Node> {
    call.child_by_field_name("arguments")
        .map(|args| {
            let mut cursor = args.walk();
            args.named_children(&mut cursor).collect()
        })
        .unwrap_or_default()
}

/// The value of a `key: value` / `:key => value` option among a call's arguments.
#[cfg(feature = "tree-sitter-ruby")]
fn ruby_option<'a>(args: &[Node<'a>], key: &str, bytes: &[u8]) -> Option<Node<'a>> {
    args.iter()
        .filter(|a| a.kind() == "pair")
        .find(|pair| {
            pair.child_by_field_name("key")
                .and_then(|k| match k.kind() {
                    "hash_key_symbol" | "simple_symbol" | "string" => ruby_text(k, bytes),
                    _ => None,
                })
                .as_deref()
                == Some(key)
        })
        .and_then(|pair| pair.child_by_field_name("value"))
}

/// Action names from an `only:` / `except:` option: a symbol or an array of them.
#[cfg(feature = "tree-sitter-ruby")]
fn ruby_action_list(value: Node, bytes: &[u8]) -> Vec<String> {
    if value.kind() == "array" {
        let mut cursor = value.walk();
        value
            .named_children(&mut cursor)
            .filter_map(|n| ruby_text(n, bytes))
            .collect()
    } else {
        ruby_text(value, bytes).into_iter().collect()
    }
}

/// `'pages#about'` -> `about`.
#[cfg(feature = "tree-sitter-ruby")]
fn rails_action(target: &str) -> Option<String> {
    target
        .split_once('#')
        .map(|(_, action)| action.to_string())
        .filter(|a| !a.is_empty())
}

/// Join a path prefix and a segment into an absolute route path.
#[cfg(any(feature = "tree-sitter-ruby", feature = "tree-sitter-php"))]
fn join_abs(prefix: &str, segment: &str) -> String {
    let segment = segment.trim_matches('/');
    match (prefix.is_empty(), segment.is_empty()) {
        (true, true) => "/".to_string(),
        (true, false) => format!("/{segment}"),
        (false, true) => prefix.to_string(),
        (false, false) => format!("{}/{}", prefix.trim_end_matches('/'), segment),
    }
}

/// Naive singular for nested-resource params (`posts` -> `post`, `stories` -> `story`).
#[cfg(any(feature = "tree-sitter-ruby", feature = "tree-sitter-php"))]
fn ruby_singular(plural: &str) -> String {
    if let Some(stem) = plural.strip_suffix("ies") {
        format!("{stem}y")
    } else {
        plural.strip_suffix('s').unwrap_or(plural).to_string()
    }
}

/// `(path, handler)` of a verb call: `get '/x', to: 'c#a'`, `get 'x' => 'c#a'`, `get :name`.
#[cfg(feature = "tree-sitter-ruby")]
fn ruby_verb_target(args: &[Node], bytes: &[u8]) -> (Option<String>, Option<String>) {
    let mut path = None;
    let mut handler = None;
    for arg in args {
        match arg.kind() {
            "string" | "simple_symbol" if path.is_none() => path = ruby_text(*arg, bytes),
            // `get 'x' => 'c#a'`
            "pair" => {
                let key = arg.child_by_field_name("key");
                let value = arg.child_by_field_name("value");
                if let Some(key) = key.filter(|k| k.kind() == "string") {
                    path = path.or_else(|| ruby_text(key, bytes));
                    handler = value
                        .and_then(|v| ruby_text(v, bytes))
                        .and_then(|t| rails_action(&t));
                }
            }
            _ => {}
        }
    }
    if let Some(to) = ruby_option(args, "to", bytes).and_then(|v| ruby_text(v, bytes)) {
        handler = rails_action(&to).or(Some(to).filter(|t| !t.contains('#')));
    }
    if let Some(action) = ruby_option(args, "action", bytes).and_then(|v| ruby_text(v, bytes)) {
        handler = Some(action);
    }
    (path, handler)
}

/// Statements of a `do ... end` / `{ ... }` block.
#[cfg(feature = "tree-sitter-ruby")]
fn ruby_block_statements(block: Node) -> Vec<Node> {
    let mut out = Vec::new();
    let mut cursor = block.walk();
    for child in block.named_children(&mut cursor) {
        if matches!(child.kind(), "body_statement" | "block_body") {
            let mut inner = child.walk();
            out.extend(child.named_children(&mut inner));
        }
    }
    out
}

#[cfg(feature = "tree-sitter-ruby")]
fn collect_ruby_routes(node: Node, bytes: &[u8], ctx: &RubyCtx, out: &mut Vec<ExtractedRoute>) {
    if node.kind() != "call" {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            collect_ruby_routes(child, bytes, ctx, out);
        }
        return;
    }

    let method = node
        .child_by_field_name("method")
        .and_then(|m| m.utf8_text(bytes).ok())
        .unwrap_or("");
    let block = node.child_by_field_name("block");
    let args = ruby_call_args(node);
    let line = node.start_position().row + 1;
    let emit = |verb: &str, path: String, handler: Option<String>, framework: &str| ExtractedRoute {
        method: Some(verb.to_string()),
        path,
        handler_name: handler,
        framework: Some(framework.to_string()),
        line,
    };
    let recurse_block = |ctx: &RubyCtx, out: &mut Vec<ExtractedRoute>| {
        if let Some(block) = block {
            for stmt in ruby_block_statements(block) {
                collect_ruby_routes(stmt, bytes, ctx, out);
            }
        }
    };

    if method == "draw" && block.is_some() {
        let inner = RubyCtx {
            in_draw: true,
            prefix: String::new(),
            resource: None,
            active: None,
            ..ctx.clone()
        };
        recurse_block(&inner, out);
        return;
    }

    if ctx.in_draw {
        match method {
            "root" => {
                let (_, handler) = ruby_verb_target(&args, bytes);
                let handler = handler.or_else(|| {
                    args.first()
                        .filter(|a| a.kind() == "string")
                        .and_then(|a| ruby_text(*a, bytes))
                        .and_then(|t| rails_action(&t))
                });
                out.push(emit("GET", join_abs(&ctx.prefix, ""), handler, "rails"));
            }
            m if RAILS_VERBS.contains(&m) => {
                let (path, handler) = ruby_verb_target(&args, bytes);
                let Some(path) = path else { return };
                // Inside `member` / `collection`, `get :preview` extends that prefix
                // and names the action.
                let prefix = ctx.active.as_deref().unwrap_or(&ctx.prefix);
                let handler = handler.or_else(|| ctx.active.is_some().then(|| path.clone()));
                out.push(emit(&m.to_uppercase(), join_abs(prefix, &path), handler, "rails"));
            }
            "namespace" | "scope" => {
                let segment = args
                    .iter()
                    .find(|a| matches!(a.kind(), "string" | "simple_symbol"))
                    .and_then(|a| ruby_text(*a, bytes))
                    .or_else(|| {
                        ruby_option(&args, "path", bytes).and_then(|v| ruby_text(v, bytes))
                    })
                    // `scope module: 'api'` does not change the path.
                    .unwrap_or_default();
                let joined = join_abs(&ctx.prefix, &segment);
                let inner = RubyCtx {
                    // `join_abs` yields "/" only when both sides are empty.
                    prefix: if joined == "/" { String::new() } else { joined },
                    ..ctx.clone()
                };
                recurse_block(&inner, out);
            }
            "resources" | "resource" => {
                let singular = method == "resource";
                let only = ruby_option(&args, "only", bytes).map(|v| ruby_action_list(v, bytes));
                let except =
                    ruby_option(&args, "except", bytes).map(|v| ruby_action_list(v, bytes));
                let table = if singular { RAILS_SINGULAR_RESOURCE } else { RAILS_RESOURCES };
                for name in args
                    .iter()
                    .filter(|a| matches!(a.kind(), "simple_symbol" | "string"))
                    .filter_map(|a| ruby_text(*a, bytes))
                {
                    let base = join_abs(&ctx.prefix, &name);
                    for (action, verb, suffix) in table {
                        let wanted = only.as_ref().is_none_or(|o| o.iter().any(|a| a == action))
                            && except.as_ref().is_none_or(|e| !e.iter().any(|a| a == action));
                        if wanted {
                            out.push(emit(
                                verb,
                                format!("{base}{suffix}"),
                                Some(action.to_string()),
                                "rails",
                            ));
                        }
                    }
                    let member = if singular { base.clone() } else { format!("{base}/:id") };
                    let inner = RubyCtx {
                        // Routes nested directly in the block hang off the parent's id param.
                        prefix: if singular {
                            base.clone()
                        } else {
                            format!("{base}/:{}_id", ruby_singular(&name))
                        },
                        resource: Some((base.clone(), member)),
                        active: None,
                        ..ctx.clone()
                    };
                    recurse_block(&inner, out);
                }
            }
            // `member do ... end` / `collection do ... end` extend the resource's own paths.
            "member" | "collection" => {
                let active = ctx.resource.as_ref().map(|(collection, member)| {
                    if method == "member" { member.clone() } else { collection.clone() }
                });
                recurse_block(&RubyCtx { active, ..ctx.clone() }, out);
            }
            // `constraints`, `concern`, `defaults`, ...: routes inside keep the same context.
            _ => recurse_block(ctx, out),
        }
        return;
    }

    // Sinatra: `get '/hello' do ... end` outside any Rails routes block.
    if ctx.sinatra && RAILS_VERBS.contains(&method) && block.is_some() {
        if let Some(path) = args
            .first()
            .filter(|a| a.kind() == "string")
            .and_then(|a| ruby_text(*a, bytes))
            .filter(|p| p.starts_with('/'))
        {
            out.push(emit(&method.to_uppercase(), path, None, "sinatra"));
            return;
        }
    }

    // Not a routing call: keep looking for a `routes.draw` block further down.
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_ruby_routes(child, bytes, ctx, out);
    }
}

// ---------------------------------------------------------------------------
// PHP: Laravel
// ---------------------------------------------------------------------------

#[cfg(feature = "tree-sitter-php")]
pub fn extract_php_routes(source: &str) -> Vec<ExtractedRoute> {
    use tree_sitter::Parser;
    use tree_sitter_php::LANGUAGE_PHP;

    let mut parser = Parser::new();
    if parser.set_language(&LANGUAGE_PHP.into()).is_err() {
        return vec![];
    }
    let tree = match parser.parse(source, None) {
        Some(t) => t,
        None => return vec![],
    };
    let mut out = Vec::new();
    collect_php_routes(
        tree.root_node(),
        source.as_bytes(),
        &PhpCtx { prefix: String::new(), controller: false },
        &mut out,
    );
    out
}

#[cfg(not(feature = "tree-sitter-php"))]
pub fn extract_php_routes(_source: &str) -> Vec<ExtractedRoute> {
    vec![]
}

#[cfg(feature = "tree-sitter-php")]
#[derive(Clone)]
struct PhpCtx {
    /// Accumulated `Route::prefix(..)` / `['prefix' => ..]` group prefix.
    prefix: String,
    /// Inside `Route::controller(X::class)->group(..)`: bare `'index'` handlers are actions.
    controller: bool,
}

#[cfg(feature = "tree-sitter-php")]
const LARAVEL_VERBS: &[&str] = &["get", "post", "put", "patch", "delete", "options"];

/// `(action, HTTP verb, path suffix)` for `Route::resource('photos', ..)`; `{id}` becomes `{photo}`.
#[cfg(feature = "tree-sitter-php")]
const LARAVEL_RESOURCE: &[(&str, &str, &str)] = &[
    ("index", "GET", ""),
    ("create", "GET", "/create"),
    ("store", "POST", ""),
    ("show", "GET", "/{id}"),
    ("edit", "GET", "/{id}/edit"),
    ("update", "PUT", "/{id}"),
    ("update", "PATCH", "/{id}"),
    ("destroy", "DELETE", "/{id}"),
];

/// Text of a plain PHP string literal (no interpolation).
#[cfg(feature = "tree-sitter-php")]
fn php_string(node: Node, bytes: &[u8]) -> Option<String> {
    if !matches!(node.kind(), "string" | "encapsed_string") {
        return None;
    }
    let raw = node.utf8_text(bytes).ok()?;
    (!raw.contains('$')).then(|| string_literal_text(node, bytes)).flatten()
}

/// The expression of each positional argument.
#[cfg(feature = "tree-sitter-php")]
fn php_args(call: Node) -> Vec<Node> {
    call.child_by_field_name("arguments")
        .map(|args| {
            let mut cursor = args.walk();
            args.named_children(&mut cursor)
                .filter(|a| a.kind() == "argument")
                .filter_map(|a| a.named_child(0))
                .collect()
        })
        .unwrap_or_default()
}

/// String elements of an array literal: `['get', 'post']`.
#[cfg(feature = "tree-sitter-php")]
fn php_array_strings(array: Node, bytes: &[u8]) -> Vec<String> {
    let mut cursor = array.walk();
    array
        .named_children(&mut cursor)
        .filter(|e| e.kind() == "array_element_initializer")
        .filter_map(|e| e.named_child(0))
        .filter_map(|n| php_string(n, bytes))
        .collect()
}

/// `['prefix' => 'api', ...]`: the string value stored under `key`.
#[cfg(feature = "tree-sitter-php")]
fn php_array_option(array: Node, key: &str, bytes: &[u8]) -> Option<String> {
    let mut cursor = array.walk();
    let value = array
        .named_children(&mut cursor)
        .filter(|e| e.kind() == "array_element_initializer" && e.named_child_count() == 2)
        .find(|e| {
            e.named_child(0).and_then(|k| php_string(k, bytes)).as_deref() == Some(key)
        })
        .and_then(|e| e.named_child(1))
        .and_then(|v| php_string(v, bytes));
    value
}

/// The controller action a route handler argument names.
#[cfg(feature = "tree-sitter-php")]
fn php_handler(handler: Option<Node>, controller: bool, bytes: &[u8]) -> Option<String> {
    let handler = handler?;
    match handler.kind() {
        // `[UserController::class, 'index']`
        "array_creation_expression" => {
            let mut cursor = handler.walk();
            let elements: Vec<_> = handler
                .named_children(&mut cursor)
                .filter(|e| e.kind() == "array_element_initializer")
                .collect();
            elements
                .get(1)
                .and_then(|e| e.named_child(0))
                .and_then(|n| php_string(n, bytes))
        }
        // `'UserController@index'`, or a bare `'index'` inside `Route::controller(..)->group`
        "string" | "encapsed_string" => {
            let text = php_string(handler, bytes)?;
            match text.split_once('@') {
                Some((_, action)) => Some(action.to_string()),
                None => controller.then_some(text),
            }
        }
        // An invokable controller: `UserController::class`
        "class_constant_access_expression" => Some("__invoke".to_string()),
        _ => None,
    }
}

/// The name a `scoped_call_expression` / `member_call_expression` invokes.
#[cfg(feature = "tree-sitter-php")]
fn php_call_name<'a>(call: Node, bytes: &'a [u8]) -> Option<&'a str> {
    call.child_by_field_name("name")?.utf8_text(bytes).ok()
}

/// True for `Route::<anything>(..)`.
#[cfg(feature = "tree-sitter-php")]
fn php_is_route_facade(call: Node, bytes: &[u8]) -> bool {
    call.kind() == "scoped_call_expression"
        && call
            .child_by_field_name("scope")
            .and_then(|s| s.utf8_text(bytes).ok())
            == Some("Route")
}

/// Walk down a `Route::a()->b()->c()` chain to its `Route::` root, returning every
/// call from the outside in.
#[cfg(feature = "tree-sitter-php")]
fn php_chain<'a>(outer: Node<'a>, bytes: &[u8]) -> Option<Vec<Node<'a>>> {
    let mut calls = vec![outer];
    let mut node = outer;
    while node.kind() == "member_call_expression" {
        node = node.child_by_field_name("object")?;
        calls.push(node);
    }
    php_is_route_facade(node, bytes).then_some(calls)
}

#[cfg(feature = "tree-sitter-php")]
fn collect_php_routes(node: Node, bytes: &[u8], ctx: &PhpCtx, out: &mut Vec<ExtractedRoute>) {
    let line = node.start_position().row + 1;
    let mk = |verb: &str, path: &str, handler: Option<String>| ExtractedRoute {
        method: Some(verb.to_string()),
        path: join_abs(&ctx.prefix, path),
        handler_name: handler,
        framework: Some("laravel".to_string()),
        line,
    };

    // A fluent chain rooted at `Route::`: groups and resources need the whole chain.
    if node.kind() == "member_call_expression" {
        if let Some(calls) = php_chain(node, bytes) {
            let root = *calls.last().unwrap();
            let root_name = php_call_name(root, bytes).unwrap_or("");

            // `Route::prefix('api')->middleware(..)->group(function () { ... })`
            if php_call_name(node, bytes) == Some("group") {
                let mut inner = ctx.clone();
                let mut prefixes = Vec::new();
                for call in &calls {
                    match php_call_name(*call, bytes) {
                        Some("prefix") => {
                            if let Some(p) = php_args(*call).first().and_then(|a| php_string(*a, bytes)) {
                                prefixes.push(p);
                            }
                        }
                        Some("controller") => inner.controller = true,
                        _ => {}
                    }
                }
                for p in prefixes.iter().rev() {
                    inner.prefix = join_abs(&inner.prefix, p);
                }
                for arg in php_args(node) {
                    collect_php_routes(arg, bytes, &inner, out);
                }
                return;
            }

            // `Route::resource('photos', C::class)->only([..])->names(..)`
            if matches!(root_name, "resource" | "apiResource") {
                let mut only = None;
                let mut except = None;
                for call in &calls {
                    let list = php_args(*call)
                        .first()
                        .filter(|a| a.kind() == "array_creation_expression")
                        .map(|a| php_array_strings(*a, bytes));
                    match php_call_name(*call, bytes) {
                        Some("only") => only = list,
                        Some("except") => except = list,
                        _ => {}
                    }
                }
                php_resource_routes(root, bytes, only, except, &mk, out);
                return;
            }
        }
    }

    if php_is_route_facade(node, bytes) {
        let name = php_call_name(node, bytes).unwrap_or("");
        let args = php_args(node);
        let path_of = |n: Option<&Node>| n.and_then(|n| php_string(*n, bytes));
        match name {
            v if LARAVEL_VERBS.contains(&v) => {
                if let Some(path) = path_of(args.first()) {
                    let handler = php_handler(args.get(1).copied(), ctx.controller, bytes);
                    out.push(mk(&v.to_uppercase(), &path, handler));
                }
            }
            "any" => {
                if let Some(path) = path_of(args.first()) {
                    let handler = php_handler(args.get(1).copied(), ctx.controller, bytes);
                    out.push(mk("*", &path, handler));
                }
            }
            "match" => {
                let verbs = args
                    .first()
                    .filter(|a| a.kind() == "array_creation_expression")
                    .map(|a| php_array_strings(*a, bytes))
                    .unwrap_or_default();
                if let Some(path) = path_of(args.get(1)) {
                    let handler = php_handler(args.get(2).copied(), ctx.controller, bytes);
                    for verb in verbs {
                        out.push(mk(&verb.to_uppercase(), &path, handler.clone()));
                    }
                }
            }
            "view" => {
                if let Some(path) = path_of(args.first()) {
                    out.push(mk("GET", &path, None));
                }
            }
            "resource" | "apiResource" => php_resource_routes(node, bytes, None, None, &mk, out),
            // `Route::group(['prefix' => 'api'], function () { ... })`
            "group" => {
                let mut inner = ctx.clone();
                if let Some(prefix) = args
                    .first()
                    .filter(|a| a.kind() == "array_creation_expression")
                    .and_then(|a| php_array_option(*a, "prefix", bytes))
                {
                    inner.prefix = join_abs(&ctx.prefix, &prefix);
                }
                for arg in &args {
                    collect_php_routes(*arg, bytes, &inner, out);
                }
                return;
            }
            _ => {}
        }
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_php_routes(child, bytes, ctx, out);
    }
}

/// Expand `Route::resource('photos', PhotoController::class)` (or `apiResource`, which
/// has no `create` / `edit`), honouring `only` / `except`.
#[cfg(feature = "tree-sitter-php")]
fn php_resource_routes(
    call: Node,
    bytes: &[u8],
    only: Option<Vec<String>>,
    except: Option<Vec<String>>,
    mk: &dyn Fn(&str, &str, Option<String>) -> ExtractedRoute,
    out: &mut Vec<ExtractedRoute>,
) {
    let api = php_call_name(call, bytes) == Some("apiResource");
    let Some(name) = php_args(call).first().and_then(|a| php_string(*a, bytes)) else {
        return;
    };
    // `photos` -> `{photo}`; nested `admin.photos` -> `admin/{admin}/photos/{photo}` is not modelled.
    let base = name.trim_matches('/');
    let param = ruby_singular(base.rsplit('/').next().unwrap_or(base));
    for (action, verb, suffix) in LARAVEL_RESOURCE {
        if api && matches!(*action, "create" | "edit") {
            continue;
        }
        let wanted = only.as_ref().is_none_or(|o| o.iter().any(|a| a == action))
            && except.as_ref().is_none_or(|e| !e.iter().any(|a| a == action));
        if wanted {
            let path = format!("{base}{}", suffix.replace("{id}", &format!("{{{param}}}")));
            out.push(mk(verb, &path, Some(action.to_string())));
        }
    }
}

/// Compose a class-level prefix and a method-level path into one route path.
fn join_path(prefix: &str, suffix: &str) -> String {
    if prefix.is_empty() {
        return suffix.to_string();
    }
    if suffix.is_empty() {
        return prefix.to_string();
    }
    format!(
        "{}/{}",
        prefix.trim_end_matches('/'),
        suffix.trim_start_matches('/')
    )
}

// ---------------------------------------------------------------------------
// Go (net/http, Gin, chi)
// ---------------------------------------------------------------------------

#[cfg(feature = "tree-sitter-go")]
pub fn extract_go_routes(source: &str) -> Vec<ExtractedRoute> {
    use tree_sitter::Parser;
    use tree_sitter_go::LANGUAGE;

    let mut parser = Parser::new();
    if parser.set_language(&LANGUAGE.into()).is_err() {
        return vec![];
    }
    let tree = match parser.parse(source, None) {
        Some(t) => t,
        None => return vec![],
    };
    let bytes = source.as_bytes();
    let mut out = Vec::new();
    collect_go_routes(tree.root_node(), bytes, "", &mut out);
    out
}

#[cfg(not(feature = "tree-sitter-go"))]
pub fn extract_go_routes(_source: &str) -> Vec<ExtractedRoute> {
    vec![]
}

#[cfg(feature = "tree-sitter-go")]
fn collect_go_routes(node: Node, bytes: &[u8], path_prefix: &str, out: &mut Vec<ExtractedRoute>) {
    if node.kind() == "call_expression" {
        if let Some(route) = go_route_from_call(node, bytes, path_prefix) {
            out.push(route);
        }
        // chi's `r.Route("/prefix", func(r chi.Router) { ... })` nests a group
        // under a composed path prefix.
        if let Some((prefix, body)) = go_chi_route_group(node, bytes, path_prefix) {
            collect_go_routes(body, bytes, &prefix, out);
            return;
        }
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_go_routes(child, bytes, path_prefix, out);
    }
}

#[cfg(feature = "tree-sitter-go")]
fn go_call_parts(node: Node, bytes: &[u8]) -> Option<(String, String)> {
    let func = node.child_by_field_name("function")?;
    if func.kind() != "selector_expression" {
        return None;
    }
    let receiver = func
        .child_by_field_name("operand")
        .and_then(|n| n.utf8_text(bytes).ok())?
        .to_string();
    let method = func
        .child_by_field_name("field")
        .and_then(|n| n.utf8_text(bytes).ok())?
        .to_string();
    Some((receiver, method))
}

#[cfg(feature = "tree-sitter-go")]
fn go_route_from_call(node: Node, bytes: &[u8], path_prefix: &str) -> Option<ExtractedRoute> {
    const GIN_CHI_METHODS: &[&str] = &["GET", "POST", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS"];

    let (receiver, method) = go_call_parts(node, bytes)?;
    let args = node.child_by_field_name("arguments")?;
    let mut cursor = args.walk();
    let arg_nodes: Vec<Node> = args.named_children(&mut cursor).collect();

    let framework;
    let http_method;
    if method == "HandleFunc" {
        framework = "net/http";
        http_method = "*".to_string();
    } else if GIN_CHI_METHODS.contains(&method.to_uppercase().as_str())
        && method.chars().all(|c| c.is_ascii_uppercase())
    {
        // Gin: `router.GET(...)` (all-caps method name).
        framework = "gin";
        http_method = method.clone();
    } else {
        let lower = method.to_lowercase();
        let normalized = lower.to_uppercase();
        if GIN_CHI_METHODS.contains(&normalized.as_str())
            && method.chars().next().is_some_and(|c| c.is_ascii_uppercase())
            && method.chars().skip(1).all(|c| c.is_ascii_lowercase())
        {
            // chi: `r.Get(...)`, `r.Post(...)` (capitalized method name).
            framework = "chi";
            http_method = normalized;
        } else {
            return None;
        }
    }
    let _ = receiver;

    let path_node = arg_nodes.first()?;
    if !matches!(
        path_node.kind(),
        "interpreted_string_literal" | "raw_string_literal"
    ) {
        return None;
    }
    let suffix = string_literal_text(*path_node, bytes)?;
    let path = join_path(path_prefix, &suffix);

    let handler_name = arg_nodes.get(1).and_then(|a| match a.kind() {
        "identifier" | "selector_expression" => a.utf8_text(bytes).ok().map(str::to_string),
        _ => None,
    });

    Some(ExtractedRoute {
        method: Some(http_method),
        path,
        handler_name,
        framework: Some(framework.to_string()),
        line: node.start_position().row + 1,
    })
}

/// Recognize a chi `r.Route("/prefix", func(r chi.Router) { ... })` call and
/// return the composed path prefix plus the closure body to recurse into.
#[cfg(feature = "tree-sitter-go")]
fn go_chi_route_group<'a>(
    node: Node<'a>,
    bytes: &[u8],
    path_prefix: &str,
) -> Option<(String, Node<'a>)> {
    let (_, method) = go_call_parts(node, bytes)?;
    if method != "Route" {
        return None;
    }
    let args = node.child_by_field_name("arguments")?;
    let mut cursor = args.walk();
    let arg_nodes: Vec<Node> = args.named_children(&mut cursor).collect();
    let path_node = arg_nodes.first()?;
    if !matches!(
        path_node.kind(),
        "interpreted_string_literal" | "raw_string_literal"
    ) {
        return None;
    }
    let suffix = string_literal_text(*path_node, bytes)?;
    let func_lit = arg_nodes
        .get(1)
        .filter(|n| n.kind() == "func_literal")?;
    let body = func_lit.child_by_field_name("body")?;
    Some((join_path(path_prefix, &suffix), body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn express_get_route() {
        let content = "router.get('/health', healthCheck);\n";
        let routes = extract_routes_for_extension("js", content);
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].method.as_deref(), Some("GET"));
        assert_eq!(routes[0].path, "/health");
        assert_eq!(routes[0].handler_name.as_deref(), Some("healthCheck"));
        assert_eq!(routes[0].framework.as_deref(), Some("express"));
    }

    #[test]
    fn express_post_route() {
        let content = r#"app.post("/users", createUser);"#;
        let routes = extract_routes_for_extension("ts", content);
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].method.as_deref(), Some("POST"));
        assert_eq!(routes[0].path, "/users");
    }

    #[test]
    fn express_use_becomes_star() {
        let content = r#"app.use('/api', apiRouter);"#;
        let routes = extract_routes_for_extension("js", content);
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].method.as_deref(), Some("*"));
    }

    #[test]
    fn express_multiline_call() {
        let content = "router.get(\n  '/health',\n  healthCheck\n);\n";
        let routes = extract_routes_for_extension("js", content);
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].path, "/health");
        assert_eq!(routes[0].handler_name.as_deref(), Some("healthCheck"));
    }

    #[test]
    fn express_inline_handler_has_no_name() {
        let content = "app.get('/ping', (req, res) => { res.send('pong'); });\n";
        let routes = extract_routes_for_extension("js", content);
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].handler_name, None);
    }

    #[test]
    fn fastapi_decorator() {
        let content = "@app.get('/items')\nasync def list_items():\n    pass\n";
        let routes = extract_routes_for_extension("py", content);
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].method.as_deref(), Some("GET"));
        assert_eq!(routes[0].path, "/items");
        assert_eq!(routes[0].framework.as_deref(), Some("fastapi"));
        assert_eq!(routes[0].handler_name.as_deref(), Some("list_items"));
    }

    #[test]
    fn flask_route_with_methods() {
        let content = "@app.route('/login', methods=['GET', 'POST'])\ndef login():\n    pass\n";
        let routes = extract_routes_for_extension("py", content);
        let flask_route = routes.iter().find(|r| r.framework.as_deref() == Some("flask"));
        assert!(flask_route.is_some());
        assert_eq!(flask_route.unwrap().path, "/login");
        assert_eq!(flask_route.unwrap().method.as_deref(), Some("GET"));
        assert_eq!(flask_route.unwrap().handler_name.as_deref(), Some("login"));
    }

    #[test]
    fn actix_handler() {
        let content = "#[get(\"/ping\")]\nasync fn ping() -> &'static str { \"pong\" }\n";
        let routes = extract_routes_for_extension("rs", content);
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].method.as_deref(), Some("GET"));
        assert_eq!(routes[0].path, "/ping");
        assert_eq!(routes[0].handler_name.as_deref(), Some("ping"));
        assert_eq!(routes[0].framework.as_deref(), Some("actix"));
    }

    #[test]
    fn axum_route_chains_every_verb() {
        let content = "fn app() -> Router {\n    Router::new()\n        .route(\"/items\", get(list).post(create))\n        .route(\"/items/:id\", routing::delete(handlers::remove))\n}\n";
        let mut routes = extract_routes_for_extension("rs", content);
        routes.sort_by(|a, b| (&a.path, &a.method).cmp(&(&b.path, &b.method)));
        let got: Vec<_> = routes
            .iter()
            .map(|r| (r.method.as_deref().unwrap(), r.path.as_str(), r.handler_name.as_deref()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("GET", "/items", Some("list")),
                ("POST", "/items", Some("create")),
                ("DELETE", "/items/:id", Some("remove")),
            ]
        );
        assert!(routes.iter().all(|r| r.framework.as_deref() == Some("axum")));
        // Each route is attributed to its own `.route(` line, not the chain's first line.
        assert_eq!(routes.iter().find(|r| r.path == "/items/:id").unwrap().line, 4);
    }

    #[test]
    fn actix_builder_route() {
        let content = "cfg.route(\"/health\", web::get().to(health));\n";
        let routes = extract_routes_for_extension("rs", content);
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].method.as_deref(), Some("GET"));
        assert_eq!(routes[0].handler_name.as_deref(), Some("health"));
        assert_eq!(routes[0].framework.as_deref(), Some("actix"));
    }

    #[test]
    fn axum_closure_handler_has_no_name_and_non_literal_path_is_skipped() {
        let content = "fn a() { r.route(\"/x\", get(|| async { 1 })); r.route(PATH, get(h)); other.route(1, 2); }\n";
        let routes = extract_routes_for_extension("rs", content);
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].path, "/x");
        assert_eq!(routes[0].handler_name, None);
    }

    #[test]
    fn rocket_attribute_is_not_labelled_actix() {
        let content = "use rocket::get;\n#[get(\"/hello/<name>\")]\nfn hello(name: &str) -> String { name.into() }\n";
        let routes = extract_routes_for_extension("rs", content);
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].path, "/hello/<name>");
        assert_eq!(routes[0].handler_name.as_deref(), Some("hello"));
        assert_eq!(routes[0].framework.as_deref(), Some("rocket"));
    }

    #[test]
    fn spring_class_and_method_mapping_compose() {
        let content = r#"
@RestController
@RequestMapping("/api/users")
class UserController {
    @GetMapping("/{id}")
    public User getUser() { return null; }

    @PostMapping
    public User createUser() { return null; }
}
"#;
        let routes = extract_routes_for_extension("java", content);
        assert_eq!(routes.len(), 2);
        let get = routes.iter().find(|r| r.method.as_deref() == Some("GET")).unwrap();
        assert_eq!(get.path, "/api/users/{id}");
        assert_eq!(get.handler_name.as_deref(), Some("getUser"));
        assert_eq!(get.framework.as_deref(), Some("spring"));
        let post = routes.iter().find(|r| r.method.as_deref() == Some("POST")).unwrap();
        assert_eq!(post.path, "/api/users");
        assert_eq!(post.handler_name.as_deref(), Some("createUser"));
    }

    #[test]
    fn aspnet_controller_tokens_and_verbs_compose() {
        let content = "[ApiController]\n[Route(\"api/[controller]\")]\npublic class UsersController : ControllerBase {\n    [HttpGet(\"{id}\")]\n    public IActionResult Get(int id) => Ok();\n    [HttpPost]\n    public IActionResult Create() => Ok();\n    [HttpDelete(\"/admin/purge\")]\n    public IActionResult Purge() => Ok();\n}\n";
        let routes = extract_routes_for_extension("cs", content);
        let got: Vec<_> = routes
            .iter()
            .map(|r| (r.method.as_deref().unwrap(), r.path.as_str(), r.handler_name.as_deref().unwrap()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("GET", "api/Users/{id}", "Get"),
                ("POST", "api/Users", "Create"),
                // An absolute method route drops the class prefix.
                ("DELETE", "/admin/purge", "Purge"),
            ]
        );
        assert!(routes.iter().all(|r| r.framework.as_deref() == Some("aspnet")));
    }

    #[test]
    fn aspnet_verb_without_path_uses_sibling_route_attribute() {
        let content = "[Route(\"api/items\")]\nclass ItemsController {\n    [HttpGet]\n    [Route(\"[action]\")]\n    public IActionResult Search() => Ok();\n}\n";
        let routes = extract_routes_for_extension("cs", content);
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].path, "api/items/Search");
    }

    #[test]
    fn aspnet_minimal_api() {
        let content = "app.MapGet(\"/ping\", Ping);\napp.MapPost(\"/x\", () => 1);\napp.MapDelete(\"/y\", Handlers.Remove);\napp.MapGet(pathVar, H);\napp.Use(x);\n";
        let routes = extract_routes_for_extension("cs", content);
        let got: Vec<_> = routes
            .iter()
            .map(|r| (r.method.as_deref().unwrap(), r.path.as_str(), r.handler_name.as_deref()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("GET", "/ping", Some("Ping")),
                ("POST", "/x", None),
                ("DELETE", "/y", Some("Remove")),
            ]
        );
    }

    #[test]
    fn rails_routes_dsl() {
        let content = "Rails.application.routes.draw do\n  root 'home#index'\n  get '/about', to: 'pages#about'\n  post 'login' => 'sessions#create'\n  namespace :admin do\n    resources :users, only: [:index, :show]\n  end\nend\n";
        let routes = extract_routes_for_extension("rb", content);
        let got: Vec<_> = routes
            .iter()
            .map(|r| (r.method.as_deref().unwrap(), r.path.as_str(), r.handler_name.as_deref()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("GET", "/", Some("index")),
                ("GET", "/about", Some("about")),
                ("POST", "/login", Some("create")),
                ("GET", "/admin/users", Some("index")),
                ("GET", "/admin/users/:id", Some("show")),
            ]
        );
        assert!(routes.iter().all(|r| r.framework.as_deref() == Some("rails")));
    }

    #[test]
    fn rails_resources_expand_and_filter() {
        let all = extract_routes_for_extension(
            "rb",
            "Rails.application.routes.draw do\n  resources :posts\n  resource :profile, except: [:destroy, :new]\nend\n",
        );
        // Full REST set for resources (update is both PATCH and PUT), minus the excepted actions.
        assert_eq!(all.iter().filter(|r| r.path.starts_with("/posts")).count(), 8);
        let profile: Vec<_> = all
            .iter()
            .filter(|r| r.path.starts_with("/profile"))
            .map(|r| (r.method.as_deref().unwrap(), r.path.as_str()))
            .collect();
        assert_eq!(
            profile,
            vec![("POST", "/profile"), ("GET", "/profile/edit"), ("GET", "/profile"), ("PATCH", "/profile"), ("PUT", "/profile")]
        );
    }

    #[test]
    fn rails_nested_member_and_collection() {
        let content = "Rails.application.routes.draw do\n  resources :posts, only: [] do\n    resources :comments, only: [:index]\n    member { get :preview }\n    collection do\n      get :search\n    end\n  end\nend\n";
        let routes = extract_routes_for_extension("rb", content);
        let got: Vec<_> = routes
            .iter()
            .map(|r| (r.path.as_str(), r.handler_name.as_deref().unwrap()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("/posts/:post_id/comments", "index"),
                ("/posts/:id/preview", "preview"),
                ("/posts/search", "search"),
            ]
        );
    }

    #[test]
    fn rails_verbs_outside_draw_are_ignored() {
        // Request specs and HTTP clients use the same verb calls.
        let content = "describe 'x' do\n  it 'works' do\n    get '/users'\n    post '/users', params: { a: 1 }\n  end\nend\n";
        assert!(extract_routes_for_extension("rb", content).is_empty());
    }

    #[test]
    fn sinatra_block_routes_need_the_framework_mentioned() {
        let sinatra = "require 'sinatra'\nget '/hello' do\n  'hi'\nend\npost '/x' do\n  1\nend\n";
        let routes = extract_routes_for_extension("rb", sinatra);
        assert_eq!(routes.len(), 2);
        assert_eq!(routes[0].path, "/hello");
        assert_eq!(routes[0].framework.as_deref(), Some("sinatra"));
        let unrelated = "get '/hello' do\n  'hi'\nend\n";
        assert!(extract_routes_for_extension("rb", unrelated).is_empty());
    }

    #[test]
    fn laravel_verbs_and_handler_forms() {
        let content = "<?php\nRoute::get('/users', [UserController::class, 'index']);\nRoute::post('/a', 'UserController@store');\nRoute::get('/c', function () { return 1; });\nRoute::get('/i', InvokableController::class);\nRoute::get('/n', [N::class, 'show'])->name('n')->middleware('auth');\nRoute::any('/any', [A::class, 'x']);\nRoute::match(['get', 'post'], '/m', [M::class, 'y']);\nRoute::view('/welcome', 'welcome');\n";
        let routes = extract_routes_for_extension("php", content);
        let got: Vec<_> = routes
            .iter()
            .map(|r| (r.method.as_deref().unwrap(), r.path.as_str(), r.handler_name.as_deref()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("GET", "/users", Some("index")),
                ("POST", "/a", Some("store")),
                ("GET", "/c", None),
                ("GET", "/i", Some("__invoke")),
                ("GET", "/n", Some("show")),
                ("*", "/any", Some("x")),
                ("GET", "/m", Some("y")),
                ("POST", "/m", Some("y")),
                ("GET", "/welcome", None),
            ]
        );
        assert!(routes.iter().all(|r| r.framework.as_deref() == Some("laravel")));
    }

    #[test]
    fn laravel_groups_compose_prefixes() {
        let content = "<?php\nRoute::prefix('api')->middleware('auth')->group(function () {\n    Route::get('/p', [PC::class, 'i']);\n    Route::prefix('v2')->group(function () { Route::get('/q', [PC::class, 'j']); });\n});\nRoute::group(['prefix' => 'admin'], function () { Route::get('/dash', [D::class, 'show']); });\nRoute::controller(OrderController::class)->group(function () { Route::get('/orders', 'index'); });\n";
        let routes = extract_routes_for_extension("php", content);
        let got: Vec<_> = routes
            .iter()
            .map(|r| (r.path.as_str(), r.handler_name.as_deref()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("/api/p", Some("i")),
                ("/api/v2/q", Some("j")),
                ("/admin/dash", Some("show")),
                ("/orders", Some("index")),
            ]
        );
    }

    #[test]
    fn laravel_resources_expand_and_filter() {
        let content = "<?php\nRoute::resource('photos', PhotoController::class);\nRoute::apiResource('posts', PostController::class)->only(['index', 'show']);\nRoute::resource('tags', TagController::class)->except(['destroy', 'create', 'edit', 'store', 'index', 'show']);\n";
        let routes = extract_routes_for_extension("php", content);
        let photos: Vec<_> = routes.iter().filter(|r| r.path.starts_with("/photos")).collect();
        assert_eq!(photos.len(), 8);
        assert!(photos
            .iter()
            .any(|r| r.method.as_deref() == Some("GET") && r.path == "/photos/{photo}/edit"));
        let posts: Vec<_> = routes
            .iter()
            .filter(|r| r.path.starts_with("/posts"))
            .map(|r| (r.method.as_deref().unwrap(), r.path.as_str()))
            .collect();
        assert_eq!(posts, vec![("GET", "/posts"), ("GET", "/posts/{post}")]);
        let tags: Vec<_> = routes.iter().filter(|r| r.path.starts_with("/tags")).collect();
        assert_eq!(tags.len(), 2); // update as PUT and PATCH
    }

    #[test]
    fn php_non_route_scoped_calls_are_ignored() {
        let content = "<?php\nCache::get('/users');\nHttp::get('/x');\n";
        assert!(extract_routes_for_extension("php", content).is_empty());
    }

    #[test]
    fn go_net_http_handle_func() {
        let content = r#"
func main() {
    http.HandleFunc("/health", healthHandler)
}
"#;
        let routes = extract_routes_for_extension("go", content);
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].path, "/health");
        assert_eq!(routes[0].framework.as_deref(), Some("net/http"));
        assert_eq!(routes[0].handler_name.as_deref(), Some("healthHandler"));
    }

    #[test]
    fn go_gin_route() {
        let content = r#"
func setup(router *gin.Engine) {
    router.GET("/users", listUsers)
}
"#;
        let routes = extract_routes_for_extension("go", content);
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].method.as_deref(), Some("GET"));
        assert_eq!(routes[0].path, "/users");
        assert_eq!(routes[0].framework.as_deref(), Some("gin"));
    }

    #[test]
    fn go_chi_route_group_composes_prefix() {
        let content = r#"
func setup(r chi.Router) {
    r.Route("/api", func(r chi.Router) {
        r.Get("/users", listUsers)
    })
}
"#;
        let routes = extract_routes_for_extension("go", content);
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].method.as_deref(), Some("GET"));
        assert_eq!(routes[0].path, "/api/users");
        assert_eq!(routes[0].framework.as_deref(), Some("chi"));
    }

    #[test]
    fn unknown_extension_returns_empty() {
        let routes = extract_routes_for_extension("rb", "# nothing here");
        assert!(routes.is_empty());
    }
}

/// Raw call/import edge extracted from source text, before symbol resolution.
#[derive(Debug, Clone)]
pub struct RawEdge {
    /// Name of the calling symbol (or the source module for import edges).
    pub from_name: String,
    /// Unresolved target name (function called, module imported, etc.).
    pub to_name: String,
    /// Optional module/file specifier for imports (e.g. './foo', '../bar', 'foo/bar').
    pub to_file: Option<String>,
    pub edge_type: &'static str,
}

// ---------------------------------------------------------------------------
// Rust edge extraction
// ---------------------------------------------------------------------------

#[cfg(feature = "tree-sitter-rust")]
pub fn extract_rust_edges(source: &str) -> Vec<RawEdge> {
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
    let root = tree.root_node();
    let mut edges = Vec::new();

    collect_rust_edges(root, bytes, None, &mut edges);
    edges
}

#[cfg(feature = "tree-sitter-rust")]
fn rust_type_names(node: tree_sitter::Node, bytes: &[u8]) -> Vec<String> {
    const SKIP: &[&str] = &[
        "u8","u16","u32","u64","u128","usize","i8","i16","i32","i64","i128","isize",
        "f32","f64","bool","char","str","String","Option","Result","Vec","Box",
        "Arc","Rc","Weak","HashMap","HashSet","BTreeMap","BTreeSet","VecDeque",
        "LinkedList","Cow","Pin","Cell","RefCell","Mutex","RwLock","MutexGuard",
        "RwLockReadGuard","RwLockWriteGuard","Send","Sync","Clone","Copy","Debug",
        "Display","Default","Iterator","IntoIterator","From","Into","AsRef","AsMut",
        "Deref","DerefMut","Future","Stream","Self","self","Error","Write","Read",
    ];
    let mut out = Vec::new();
    match node.kind() {
        "type_identifier" => {
            if let Ok(name) = node.utf8_text(bytes) {
                if !SKIP.contains(&name) {
                    out.push(name.to_string());
                }
            }
        }
        "scoped_type_identifier" => {
            if let Some(n) = node.child_by_field_name("name") {
                out.extend(rust_type_names(n, bytes));
            }
        }
        "generic_type" | "reference_type" | "pointer_type" | "abstract_type"
        | "dynamic_type" | "tuple_type" | "type_arguments" | "array_type" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                out.extend(rust_type_names(child, bytes));
            }
        }
        _ => {}
    }
    out
}

#[cfg(feature = "tree-sitter-rust")]
fn collect_rust_edges(
    node: tree_sitter::Node,
    bytes: &[u8],
    enclosing_fn: Option<&str>,
    edges: &mut Vec<RawEdge>,
) {
    let kind = node.kind();

    let is_fn = matches!(kind, "function_item" | "function_signature_item");
    let fn_name: Option<String> = if is_fn {
        node.child_by_field_name("name")
            .and_then(|n| n.utf8_text(bytes).ok())
            .map(str::to_string)
    } else {
        None
    };
    let current_fn = fn_name.as_deref().or(enclosing_fn);

    if is_fn {
        if let Some(fname) = fn_name.as_deref() {
            let mut seen = std::collections::HashSet::new();
            if let Some(params) = node.child_by_field_name("parameters") {
                let mut cursor = params.walk();
                for param in params.named_children(&mut cursor) {
                    if let Some(type_node) = param.child_by_field_name("type") {
                        for tname in rust_type_names(type_node, bytes) {
                            if seen.insert(tname.clone()) {
                                edges.push(RawEdge { from_name: fname.to_string(), to_name: tname, to_file: None, edge_type: "uses_type" });
                            }
                        }
                    }
                }
            }
            if let Some(ret) = node.child_by_field_name("return_type") {
                for tname in rust_type_names(ret, bytes) {
                    if seen.insert(tname.clone()) {
                        edges.push(RawEdge { from_name: fname.to_string(), to_name: tname, to_file: None, edge_type: "uses_type" });
                    }
                }
            }
        }
    }

    if kind == "call_expression" {
        if let Some(caller) = enclosing_fn {
            let callee = node
                .child(0)
                .and_then(|n| call_target_name(n, bytes))
                .unwrap_or_default();
            if !callee.is_empty() && callee != caller {
                edges.push(RawEdge {
                    from_name: caller.to_string(),
                    to_name: callee,
                    to_file: None,
                    edge_type: "calls",
                });
            }
        }
    } else if kind == "use_declaration" {
        if let Some(caller) = enclosing_fn.or(Some("__module__")) {
            let target = node
                .named_child(0)
                .and_then(|n| use_path_leaf(n, bytes))
                .unwrap_or_default();
            if !target.is_empty() {
                edges.push(RawEdge {
                    from_name: caller.to_string(),
                    to_name: target,
                    to_file: None,
                    edge_type: "imports",
                });
            }
        }
    }

    let mut cursor = node.walk();
    let children: Vec<_> = node.named_children(&mut cursor).collect();
    for child in children {
        collect_rust_edges(child, bytes, current_fn, edges);
    }
}

#[cfg(feature = "tree-sitter-rust")]
fn call_target_name(node: tree_sitter::Node, bytes: &[u8]) -> Option<String> {
    match node.kind() {
        "identifier" => node.utf8_text(bytes).ok().map(str::to_string),
        "scoped_identifier" => node
            .child_by_field_name("name")
            .and_then(|n| n.utf8_text(bytes).ok())
            .map(str::to_string),
        "field_expression" => node
            .child_by_field_name("field")
            .and_then(|n| n.utf8_text(bytes).ok())
            .map(str::to_string),
        _ => None,
    }
}

#[cfg(feature = "tree-sitter-rust")]
fn use_path_leaf(node: tree_sitter::Node, bytes: &[u8]) -> Option<String> {
    match node.kind() {
        "identifier" | "type_identifier" => node.utf8_text(bytes).ok().map(str::to_string),
        "scoped_identifier" => node
            .child_by_field_name("name")
            .and_then(|n| use_path_leaf(n, bytes)),
        _ => {
            let mut cursor = node.walk();
            let children: Vec<_> = node.named_children(&mut cursor).collect();
            drop(cursor);
            children.into_iter().find_map(|c| use_path_leaf(c, bytes))
        }
    }
}

// ---------------------------------------------------------------------------
// TypeScript / JavaScript shared traversal helpers
// ---------------------------------------------------------------------------

#[cfg(any(feature = "tree-sitter-typescript", feature = "tree-sitter-javascript"))]
pub(crate) fn collect_ts_edges(
    node: tree_sitter::Node,
    bytes: &[u8],
    enclosing_fn: Option<&str>,
    edges: &mut Vec<RawEdge>,
) {
    let kind = node.kind();

    let is_fn = matches!(
        kind,
        "function_declaration"
            | "method_definition"
            | "arrow_function"
            | "function_expression"
    );
    let fn_name: Option<String> = if is_fn {
        node.child_by_field_name("name")
            .and_then(|n| n.utf8_text(bytes).ok())
            .map(str::to_string)
    } else {
        None
    };
    let current_fn = fn_name.as_deref().or(enclosing_fn);

    if is_fn {
        if let Some(fname) = fn_name.as_deref() {
            let mut seen = std::collections::HashSet::new();
            if let Some(params) = node.child_by_field_name("parameters") {
                let mut cursor = params.walk();
                for param in params.named_children(&mut cursor) {
                    if matches!(param.kind(), "required_parameter" | "optional_parameter") {
                        if let Some(type_ann) = param.child_by_field_name("type") {
                            if let Some(type_node) = type_ann.named_child(0) {
                                for tname in ts_type_names(type_node, bytes) {
                                    if seen.insert(tname.clone()) {
                                        edges.push(RawEdge { from_name: fname.to_string(), to_name: tname, to_file: None, edge_type: "uses_type" });
                                    }
                                }
                            }
                        }
                    }
                }
            }
            if let Some(ret_ann) = node.child_by_field_name("return_type") {
                if let Some(type_node) = ret_ann.named_child(0) {
                    for tname in ts_type_names(type_node, bytes) {
                        if seen.insert(tname.clone()) {
                            edges.push(RawEdge { from_name: fname.to_string(), to_name: tname, to_file: None, edge_type: "uses_type" });
                        }
                    }
                }
            }
        }
    }

    if kind == "call_expression" {
        if let Some(caller) = enclosing_fn {
            let callee = node
                .child_by_field_name("function")
                .and_then(|n| ts_call_target(n, bytes))
                .unwrap_or_default();
            if !callee.is_empty() && callee != caller {
                edges.push(RawEdge {
                    from_name: caller.to_string(),
                    to_name: callee,
                    to_file: None,
                    edge_type: "calls",
                });
            }
        }
    } else if kind == "import_statement" {
        let caller = enclosing_fn.unwrap_or("__module__");
        let mut module_spec: Option<String> = None;
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if child.kind() == "string" || child.kind() == "string_literal" {
                if let Ok(s) = child.utf8_text(bytes) {
                    let trimmed = s.trim();
                    let spec = trimmed.trim_matches('"').trim_matches('\'').to_string();
                    module_spec = Some(spec);
                }
            }
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if child.kind() == "import_clause" {
                collect_ts_import_clause(child, bytes, caller, module_spec.as_deref(), edges);
            }
        }
    } else if kind == "class_declaration" {
        if let Some(heritage) = node.child_by_field_name("heritage") {
            if let Some(class_name) = node
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(bytes).ok())
            {
                let mut cursor = heritage.walk();
                for clause in heritage.named_children(&mut cursor) {
                    if clause.kind() == "class_heritage" {
                        collect_ts_heritage(clause, bytes, class_name, edges);
                    }
                }
            }
        }
    }

    let mut cursor = node.walk();
    let children: Vec<_> = node.named_children(&mut cursor).collect();
    for child in children {
        collect_ts_edges(child, bytes, current_fn, edges);
    }
}

#[cfg(any(feature = "tree-sitter-typescript", feature = "tree-sitter-javascript"))]
fn ts_type_names(node: tree_sitter::Node, bytes: &[u8]) -> Vec<String> {
    const SKIP: &[&str] = &[
        "string","number","boolean","void","any","never","unknown","null","undefined",
        "object","symbol","bigint","Array","Promise","Record","Partial","Required",
        "Readonly","Pick","Omit","Exclude","Extract","NonNullable","ReturnType",
        "InstanceType","Parameters","Map","Set","WeakMap","WeakSet","Date","RegExp",
        "Error","Function","Object","EventTarget","Element","HTMLElement",
    ];
    let mut out = Vec::new();
    match node.kind() {
        "type_identifier" => {
            if let Ok(name) = node.utf8_text(bytes) {
                if !SKIP.contains(&name) {
                    out.push(name.to_string());
                }
            }
        }
        "predefined_type" => {}
        "generic_type" | "union_type" | "intersection_type"
        | "array_type" | "tuple_type" | "type_arguments" | "parenthesized_type" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                out.extend(ts_type_names(child, bytes));
            }
        }
        _ => {}
    }
    out
}

#[cfg(any(feature = "tree-sitter-typescript", feature = "tree-sitter-javascript"))]
fn ts_call_target(node: tree_sitter::Node, bytes: &[u8]) -> Option<String> {
    match node.kind() {
        "identifier" => node.utf8_text(bytes).ok().map(str::to_string),
        "member_expression" => node
            .child_by_field_name("property")
            .and_then(|n| n.utf8_text(bytes).ok())
            .map(str::to_string),
        _ => None,
    }
}

#[cfg(any(feature = "tree-sitter-typescript", feature = "tree-sitter-javascript"))]
fn collect_ts_import_clause(
    node: tree_sitter::Node,
    bytes: &[u8],
    caller: &str,
    module_spec: Option<&str>,
    edges: &mut Vec<RawEdge>,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "identifier" => {
                if let Ok(name) = child.utf8_text(bytes) {
                    edges.push(RawEdge {
                        from_name: caller.to_string(),
                        to_name: name.to_string(),
                        to_file: module_spec.map(|s| s.to_string()),
                        edge_type: "imports",
                    });
                }
            }
            "named_imports" => {
                let mut c2 = child.walk();
                for spec in child.named_children(&mut c2) {
                    if spec.kind() == "import_specifier" {
                        let name = spec
                            .child_by_field_name("name")
                            .and_then(|n| n.utf8_text(bytes).ok())
                            .unwrap_or_default();
                        if !name.is_empty() {
                            edges.push(RawEdge {
                                from_name: caller.to_string(),
                                to_name: name.to_string(),
                                to_file: module_spec.map(|s| s.to_string()),
                                edge_type: "imports",
                            });
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(any(feature = "tree-sitter-typescript", feature = "tree-sitter-javascript"))]
fn collect_ts_heritage(
    node: tree_sitter::Node,
    bytes: &[u8],
    class_name: &str,
    edges: &mut Vec<RawEdge>,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if let Ok(base) = child.utf8_text(bytes) {
            edges.push(RawEdge {
                from_name: class_name.to_string(),
                to_name: base.to_string(),
                to_file: None,
                edge_type: "inherits",
            });
        }
    }
}

// ---------------------------------------------------------------------------
// TypeScript edge extraction
// ---------------------------------------------------------------------------

#[cfg(feature = "tree-sitter-typescript")]
pub fn extract_typescript_edges(source: &str) -> Vec<RawEdge> {
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
    let mut edges = Vec::new();
    collect_ts_edges(tree.root_node(), bytes, None, &mut edges);
    edges
}

// ---------------------------------------------------------------------------
// JavaScript edge extraction
// ---------------------------------------------------------------------------

#[cfg(feature = "tree-sitter-javascript")]
pub fn extract_javascript_edges(source: &str) -> Vec<RawEdge> {
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
    let mut edges = Vec::new();
    collect_ts_edges(tree.root_node(), bytes, None, &mut edges);
    edges
}

// ---------------------------------------------------------------------------
// Python edge extraction
// ---------------------------------------------------------------------------

#[cfg(feature = "tree-sitter-python")]
fn python_type_names(node: tree_sitter::Node, bytes: &[u8]) -> Vec<String> {
    const SKIP: &[&str] = &[
        "int","float","str","bool","bytes","bytearray","None","list","dict","set",
        "frozenset","tuple","type","List","Dict","Set","Tuple","FrozenSet","Type",
        "Optional","Union","Any","Callable","Iterator","Generator","Awaitable",
        "Coroutine","AsyncIterator","AsyncGenerator","ClassVar","Final","Literal",
        "TypeVar","Generic","Sequence","MutableSequence","Mapping","MutableMapping",
        "Iterable","Collection","Sized","Container","NamedTuple","TypedDict",
    ];
    let mut out = Vec::new();
    match node.kind() {
        "identifier" => {
            if let Ok(name) = node.utf8_text(bytes) {
                if !SKIP.contains(&name) {
                    out.push(name.to_string());
                }
            }
        }
        "attribute" => {
            if let Some(attr) = node.child_by_field_name("attribute") {
                out.extend(python_type_names(attr, bytes));
            }
        }
        "subscript" => {
            // List[T], Dict[K, V] — recurse into subscript arguments only
            if let Some(sub) = node.child_by_field_name("subscript") {
                out.extend(python_type_names(sub, bytes));
            }
        }
        "binary_operator" => {
            // A | B (Python 3.10+ union)
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                out.extend(python_type_names(child, bytes));
            }
        }
        _ => {}
    }
    out
}

#[cfg(feature = "tree-sitter-python")]
pub fn extract_python_edges(source: &str) -> Vec<RawEdge> {
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
    let mut edges = Vec::new();
    collect_python_edges(tree.root_node(), bytes, None, &mut edges);
    edges
}

#[cfg(feature = "tree-sitter-python")]
fn collect_python_edges(
    node: tree_sitter::Node,
    bytes: &[u8],
    enclosing_fn: Option<&str>,
    edges: &mut Vec<RawEdge>,
) {
    let kind = node.kind();

    let is_fn = matches!(kind, "function_definition" | "async_function_definition");
    let fn_name: Option<String> = if is_fn {
        node.child_by_field_name("name")
            .and_then(|n| n.utf8_text(bytes).ok())
            .map(str::to_string)
    } else {
        None
    };
    let current_fn = fn_name.as_deref().or(enclosing_fn);

    if is_fn {
        if let Some(fname) = fn_name.as_deref() {
            let mut seen = std::collections::HashSet::new();
            if let Some(params) = node.child_by_field_name("parameters") {
                let mut cursor = params.walk();
                for param in params.named_children(&mut cursor) {
                    if param.kind() == "typed_parameter" {
                        // named_child(0) = name, named_child(1) = type expression
                        if let Some(type_node) = param.named_child(1) {
                            for tname in python_type_names(type_node, bytes) {
                                if seen.insert(tname.clone()) {
                                    edges.push(RawEdge { from_name: fname.to_string(), to_name: tname, to_file: None, edge_type: "uses_type" });
                                }
                            }
                        }
                    }
                }
            }
            if let Some(ret) = node.child_by_field_name("return_type") {
                for tname in python_type_names(ret, bytes) {
                    if seen.insert(tname.clone()) {
                        edges.push(RawEdge { from_name: fname.to_string(), to_name: tname, to_file: None, edge_type: "uses_type" });
                    }
                }
            }
        }
    }

    match kind {
        "import_statement" => {
            // `import foo.bar` or `import foo.bar as fb`
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                let (module_path, name) = match child.kind() {
                    "dotted_name" => {
                        let text = child.utf8_text(bytes).unwrap_or("").to_string();
                        let name = text.split('.').last().unwrap_or(&text).to_string();
                        (text.replace('.', "/"), name)
                    }
                    "aliased_import" => {
                        let orig = child.child_by_field_name("name");
                        let text = orig
                            .and_then(|n| n.utf8_text(bytes).ok())
                            .unwrap_or("")
                            .to_string();
                        let name = text.split('.').last().unwrap_or(&text).to_string();
                        (text.replace('.', "/"), name)
                    }
                    _ => continue,
                };
                if !name.is_empty() {
                    edges.push(RawEdge {
                        from_name: "__module__".to_string(),
                        to_name: name,
                        to_file: Some(module_path),
                        edge_type: "imports",
                    });
                }
            }
            return; // don't recurse into import nodes
        }
        "import_from_statement" => {
            // `from foo.bar import Baz` or `from . import utils`
            let module_node = node.child_by_field_name("module_name");
            let to_file = module_node
                .and_then(|n| n.utf8_text(bytes).ok())
                .map(python_module_to_file_path);

            if let Some(ref mpath) = to_file {
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    match child.kind() {
                        // skip the module_name node itself
                        "dotted_name" | "relative_import" if Some(child) == module_node => {}
                        "dotted_name" => {
                            if let Ok(text) = child.utf8_text(bytes) {
                                let name =
                                    text.split('.').last().unwrap_or(text).to_string();
                                if !name.is_empty() {
                                    edges.push(RawEdge {
                                        from_name: "__module__".to_string(),
                                        to_name: name,
                                        to_file: Some(mpath.clone()),
                                        edge_type: "imports",
                                    });
                                }
                            }
                        }
                        "aliased_import" => {
                            if let Some(orig) = child.child_by_field_name("name") {
                                if let Ok(text) = orig.utf8_text(bytes) {
                                    let name =
                                        text.split('.').last().unwrap_or(text).to_string();
                                    if !name.is_empty() {
                                        edges.push(RawEdge {
                                            from_name: "__module__".to_string(),
                                            to_name: name,
                                            to_file: Some(mpath.clone()),
                                            edge_type: "imports",
                                        });
                                    }
                                }
                            }
                        }
                        "wildcard_import" => {
                            edges.push(RawEdge {
                                from_name: "__module__".to_string(),
                                to_name: "*".to_string(),
                                to_file: Some(mpath.clone()),
                                edge_type: "imports",
                            });
                        }
                        _ => {}
                    }
                }
            }
            return; // don't recurse into import nodes
        }
        "call" => {
            if let Some(caller) = current_fn {
                let callee = node
                    .child_by_field_name("function")
                    .and_then(|n| python_call_target(n, bytes))
                    .unwrap_or_default();
                if !callee.is_empty() && callee != caller {
                    edges.push(RawEdge {
                        from_name: caller.to_string(),
                        to_name: callee,
                        to_file: None,
                        edge_type: "calls",
                    });
                }
            }
        }
        "class_definition" => {
            let class_name = node
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(bytes).ok())
                .map(str::to_string);
            if let Some(ref cname) = class_name {
                if let Some(bases) = node.child_by_field_name("superclasses") {
                    let mut cursor = bases.walk();
                    for base in bases.named_children(&mut cursor) {
                        if let Ok(text) = base.utf8_text(bytes) {
                            let name = text.split('.').last().unwrap_or(text).to_string();
                            if !name.is_empty() && name != cname.as_str() {
                                edges.push(RawEdge {
                                    from_name: cname.clone(),
                                    to_name: name,
                                    to_file: None,
                                    edge_type: "inherits",
                                });
                            }
                        }
                    }
                }
            }
        }
        _ => {}
    }

    let mut cursor = node.walk();
    let children: Vec<_> = node.named_children(&mut cursor).collect();
    for child in children {
        collect_python_edges(child, bytes, current_fn, edges);
    }
}

#[cfg(feature = "tree-sitter-python")]
fn python_call_target(node: tree_sitter::Node, bytes: &[u8]) -> Option<String> {
    match node.kind() {
        "identifier" => node.utf8_text(bytes).ok().map(str::to_string),
        "attribute" => node
            .child_by_field_name("attribute")
            .and_then(|n| n.utf8_text(bytes).ok())
            .map(str::to_string),
        _ => None,
    }
}

/// Convert a Python module path (possibly with leading dots) to a file path string.
/// `foo.bar` → `"foo/bar"`, `.utils` → `"./utils"`, `..models` → `"../models"`
#[cfg(feature = "tree-sitter-python")]
fn python_module_to_file_path(module_text: &str) -> String {
    let dots = module_text.chars().take_while(|&c| c == '.').count();
    let rest = &module_text[dots..];
    let rest_path = rest.replace('.', "/");
    match dots {
        0 => rest_path,
        1 => {
            if rest.is_empty() {
                "./".to_string()
            } else {
                format!("./{}", rest_path)
            }
        }
        n => {
            let prefix = "../".repeat(n - 1);
            if rest.is_empty() {
                prefix
            } else {
                format!("{}{}", prefix, rest_path)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Go edge extraction
// ---------------------------------------------------------------------------

#[cfg(feature = "tree-sitter-go")]
pub fn extract_go_edges(source: &str) -> Vec<RawEdge> {
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
    let mut edges = Vec::new();
    collect_go_edges(tree.root_node(), bytes, None, &mut edges);
    edges
}

#[cfg(feature = "tree-sitter-go")]
fn collect_go_edges(
    node: tree_sitter::Node,
    bytes: &[u8],
    enclosing_fn: Option<&str>,
    edges: &mut Vec<RawEdge>,
) {
    let kind = node.kind();

    let is_fn = matches!(kind, "function_declaration" | "method_declaration");
    let fn_name: Option<String> = if is_fn {
        node.child_by_field_name("name")
            .and_then(|n| n.utf8_text(bytes).ok())
            .map(str::to_string)
    } else {
        None
    };
    let current_fn = fn_name.as_deref().or(enclosing_fn);

    match kind {
        "import_declaration" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                match child.kind() {
                    "import_spec" => go_emit_import_spec(child, bytes, edges),
                    "import_spec_list" => {
                        let mut c2 = child.walk();
                        for spec in child.named_children(&mut c2) {
                            if spec.kind() == "import_spec" {
                                go_emit_import_spec(spec, bytes, edges);
                            }
                        }
                    }
                    _ => {}
                }
            }
            return;
        }
        "call_expression" => {
            if let Some(caller) = current_fn {
                let callee = node
                    .child_by_field_name("function")
                    .and_then(|n| go_call_target(n, bytes))
                    .unwrap_or_default();
                if !callee.is_empty() && callee != caller {
                    edges.push(RawEdge {
                        from_name: caller.to_string(),
                        to_name: callee,
                        to_file: None,
                        edge_type: "calls",
                    });
                }
            }
        }
        _ => {}
    }

    let mut cursor = node.walk();
    let children: Vec<_> = node.named_children(&mut cursor).collect();
    for child in children {
        collect_go_edges(child, bytes, current_fn, edges);
    }
}

#[cfg(feature = "tree-sitter-go")]
fn go_emit_import_spec(spec: tree_sitter::Node, bytes: &[u8], edges: &mut Vec<RawEdge>) {
    // path field is an interpreted_string_literal like `"github.com/user/pkg"`
    if let Some(path_node) = spec.child_by_field_name("path") {
        if let Ok(raw) = path_node.utf8_text(bytes) {
            let path = raw.trim().trim_matches('"').trim_matches('\'');
            if path.is_empty() {
                return;
            }
            // Use the last path segment as the package name (the importable name)
            let pkg_name = path.split('/').last().unwrap_or(path).to_string();
            // Check for an explicit alias (e.g. `import alias "pkg"`)
            let alias = spec
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(bytes).ok())
                .filter(|s| *s != "_" && *s != ".")
                .map(str::to_string);
            let to_name = alias.unwrap_or(pkg_name);
            edges.push(RawEdge {
                from_name: "__module__".to_string(),
                to_name,
                to_file: None, // Go import paths are URL-style; path resolution needs module root
                edge_type: "imports",
            });
        }
    }
}

#[cfg(feature = "tree-sitter-go")]
fn go_call_target(node: tree_sitter::Node, bytes: &[u8]) -> Option<String> {
    match node.kind() {
        "identifier" => node.utf8_text(bytes).ok().map(str::to_string),
        "selector_expression" => node
            .child_by_field_name("field")
            .and_then(|n| n.utf8_text(bytes).ok())
            .map(str::to_string),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Java edge extraction
// ---------------------------------------------------------------------------

#[cfg(feature = "tree-sitter-java")]
pub fn extract_java_edges(source: &str) -> Vec<RawEdge> {
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
    let mut edges = Vec::new();
    collect_java_edges(tree.root_node(), bytes, None, &mut edges);
    edges
}

#[cfg(feature = "tree-sitter-java")]
fn collect_java_edges(
    node: tree_sitter::Node,
    bytes: &[u8],
    enclosing_fn: Option<&str>,
    edges: &mut Vec<RawEdge>,
) {
    let kind = node.kind();

    let is_fn = matches!(kind, "method_declaration" | "constructor_declaration");
    let fn_name: Option<String> = if is_fn {
        node.child_by_field_name("name")
            .and_then(|n| n.utf8_text(bytes).ok())
            .map(str::to_string)
    } else {
        None
    };
    let current_fn = fn_name.as_deref().or(enclosing_fn);

    match kind {
        "import_declaration" => {
            // Full text looks like: `import com.example.Foo;`
            // The named child is a scoped_identifier or identifier.
            let mut cursor = node.walk();
            if let Some(name_node) = node.named_children(&mut cursor).next() {
                if let Ok(text) = name_node.utf8_text(bytes) {
                    let text = text.trim_end_matches(';').trim();
                    if !text.is_empty() {
                        let class_name = text.split('.').last().unwrap_or(text).to_string();
                        let file_path = text.replace('.', "/");
                        edges.push(RawEdge {
                            from_name: "__module__".to_string(),
                            to_name: class_name,
                            to_file: Some(file_path),
                            edge_type: "imports",
                        });
                    }
                }
            }
            return;
        }
        "method_invocation" => {
            if let Some(caller) = current_fn {
                let callee = node
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(bytes).ok())
                    .map(str::to_string)
                    .unwrap_or_default();
                if !callee.is_empty() && callee != caller {
                    edges.push(RawEdge {
                        from_name: caller.to_string(),
                        to_name: callee,
                        to_file: None,
                        edge_type: "calls",
                    });
                }
            }
        }
        "class_declaration" | "interface_declaration" => {
            let class_name = node
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(bytes).ok())
                .map(str::to_string);
            if let Some(ref cname) = class_name {
                // extends clause
                if let Some(superclass) = node.child_by_field_name("superclass") {
                    if let Ok(base) = superclass.utf8_text(bytes) {
                        let base = base.trim();
                        if !base.is_empty() {
                            edges.push(RawEdge {
                                from_name: cname.clone(),
                                to_name: base.to_string(),
                                to_file: None,
                                edge_type: "inherits",
                            });
                        }
                    }
                }
                // implements clause
                if let Some(interfaces) = node.child_by_field_name("interfaces") {
                    let mut cursor = interfaces.walk();
                    for iface in interfaces.named_children(&mut cursor) {
                        if let Ok(name) = iface.utf8_text(bytes) {
                            let name = name.trim();
                            if !name.is_empty() {
                                edges.push(RawEdge {
                                    from_name: cname.clone(),
                                    to_name: name.to_string(),
                                    to_file: None,
                                    edge_type: "implements",
                                });
                            }
                        }
                    }
                }
            }
        }
        _ => {}
    }

    let mut cursor = node.walk();
    let children: Vec<_> = node.named_children(&mut cursor).collect();
    for child in children {
        collect_java_edges(child, bytes, current_fn, edges);
    }
}

// ---------------------------------------------------------------------------
// C edge extraction
// ---------------------------------------------------------------------------

#[cfg(feature = "tree-sitter-c")]
pub fn extract_c_edges(source: &str) -> Vec<RawEdge> {
    use tree_sitter::Parser;
    use tree_sitter_c::LANGUAGE;

    let mut parser = Parser::new();
    if parser.set_language(&LANGUAGE.into()).is_err() {
        return vec![];
    }
    let tree = match parser.parse(source, None) {
        Some(t) => t,
        None => return vec![],
    };
    let bytes = source.as_bytes();
    let mut edges = Vec::new();
    collect_c_edges(tree.root_node(), bytes, None, &mut edges);
    edges
}

#[cfg(feature = "tree-sitter-c")]
fn collect_c_edges(
    node: tree_sitter::Node,
    bytes: &[u8],
    enclosing_fn: Option<&str>,
    edges: &mut Vec<RawEdge>,
) {
    let kind = node.kind();

    let is_fn = kind == "function_definition";
    let fn_name: Option<String> = if is_fn {
        // In C, function name is nested: declarator → declarator → identifier
        c_function_name(node, bytes)
    } else {
        None
    };
    let current_fn = fn_name.as_deref().or(enclosing_fn);

    match kind {
        "preproc_include" => {
            // Only emit edges for local includes: `#include "file.h"` (not `<stdlib.h>`)
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if child.kind() == "string_literal" {
                    if let Ok(raw) = child.utf8_text(bytes) {
                        let path = raw.trim().trim_matches('"').trim_matches('\'').to_string();
                        if !path.is_empty() {
                            let stem = std::path::Path::new(&path)
                                .file_stem()
                                .and_then(|s| s.to_str())
                                .unwrap_or(&path)
                                .to_string();
                            edges.push(RawEdge {
                                from_name: "__module__".to_string(),
                                to_name: stem,
                                to_file: Some(path),
                                edge_type: "imports",
                            });
                        }
                    }
                }
            }
            return;
        }
        "call_expression" => {
            if let Some(caller) = current_fn {
                let callee = node
                    .child_by_field_name("function")
                    .and_then(|n| n.utf8_text(bytes).ok())
                    .map(|s| s.trim().to_string())
                    .unwrap_or_default();
                if !callee.is_empty() && callee != caller {
                    edges.push(RawEdge {
                        from_name: caller.to_string(),
                        to_name: callee,
                        to_file: None,
                        edge_type: "calls",
                    });
                }
            }
        }
        _ => {}
    }

    let mut cursor = node.walk();
    let children: Vec<_> = node.named_children(&mut cursor).collect();
    for child in children {
        collect_c_edges(child, bytes, current_fn, edges);
    }
}

/// Extract a C function's name by walking the declarator chain.
#[cfg(any(feature = "tree-sitter-c", feature = "tree-sitter-cpp"))]
fn c_function_name(node: tree_sitter::Node, bytes: &[u8]) -> Option<String> {
    let decl = node.child_by_field_name("declarator")?;
    c_declarator_name(decl, bytes)
}

#[cfg(any(feature = "tree-sitter-c", feature = "tree-sitter-cpp"))]
fn c_declarator_name(node: tree_sitter::Node, bytes: &[u8]) -> Option<String> {
    match node.kind() {
        "identifier" => node.utf8_text(bytes).ok().map(str::to_string),
        "function_declarator" => node
            .child_by_field_name("declarator")
            .and_then(|n| c_declarator_name(n, bytes)),
        "pointer_declarator" => node
            .child_by_field_name("declarator")
            .and_then(|n| c_declarator_name(n, bytes)),
        "qualified_identifier" => node
            .child_by_field_name("name")
            .and_then(|n| n.utf8_text(bytes).ok())
            .map(str::to_string),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// C++ edge extraction
// ---------------------------------------------------------------------------

#[cfg(feature = "tree-sitter-cpp")]
pub fn extract_cpp_edges(source: &str) -> Vec<RawEdge> {
    use tree_sitter::Parser;
    use tree_sitter_cpp::LANGUAGE;

    let mut parser = Parser::new();
    if parser.set_language(&LANGUAGE.into()).is_err() {
        return vec![];
    }
    let tree = match parser.parse(source, None) {
        Some(t) => t,
        None => return vec![],
    };
    let bytes = source.as_bytes();
    let mut edges = Vec::new();
    collect_cpp_edges(tree.root_node(), bytes, None, &mut edges);
    edges
}

#[cfg(feature = "tree-sitter-cpp")]
fn collect_cpp_edges(
    node: tree_sitter::Node,
    bytes: &[u8],
    enclosing_fn: Option<&str>,
    edges: &mut Vec<RawEdge>,
) {
    let kind = node.kind();

    let is_fn = kind == "function_definition";
    let fn_name: Option<String> = if is_fn {
        c_function_name(node, bytes)
    } else {
        None
    };
    let current_fn = fn_name.as_deref().or(enclosing_fn);

    match kind {
        "preproc_include" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if child.kind() == "string_literal" {
                    if let Ok(raw) = child.utf8_text(bytes) {
                        let path = raw.trim().trim_matches('"').trim_matches('\'').to_string();
                        if !path.is_empty() {
                            let stem = std::path::Path::new(&path)
                                .file_stem()
                                .and_then(|s| s.to_str())
                                .unwrap_or(&path)
                                .to_string();
                            edges.push(RawEdge {
                                from_name: "__module__".to_string(),
                                to_name: stem,
                                to_file: Some(path),
                                edge_type: "imports",
                            });
                        }
                    }
                }
            }
            return;
        }
        "using_declaration" => {
            // `using std::vector;` or `using namespace std;`
            let text = node.utf8_text(bytes).unwrap_or("").trim().to_string();
            let name = text
                .trim_start_matches("using")
                .trim_start_matches("namespace")
                .trim()
                .trim_end_matches(';')
                .split("::")
                .last()
                .unwrap_or("")
                .trim()
                .to_string();
            if !name.is_empty() {
                edges.push(RawEdge {
                    from_name: "__module__".to_string(),
                    to_name: name,
                    to_file: None,
                    edge_type: "imports",
                });
            }
            return;
        }
        "call_expression" => {
            if let Some(caller) = current_fn {
                let callee = node
                    .child_by_field_name("function")
                    .and_then(|n| cpp_call_target(n, bytes))
                    .unwrap_or_default();
                if !callee.is_empty() && callee != caller {
                    edges.push(RawEdge {
                        from_name: caller.to_string(),
                        to_name: callee,
                        to_file: None,
                        edge_type: "calls",
                    });
                }
            }
        }
        "class_specifier" | "struct_specifier" => {
            let class_name = node
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(bytes).ok())
                .map(str::to_string);
            if let Some(ref cname) = class_name {
                if let Some(bases) = node.child_by_field_name("bases") {
                    let mut cursor = bases.walk();
                    for base in bases.named_children(&mut cursor) {
                        // base_class_clause contains type identifiers
                        let base_text = base.utf8_text(bytes).unwrap_or("").trim().to_string();
                        // strip access specifier prefixes (public, protected, private)
                        let base_name = base_text
                            .trim_start_matches("public")
                            .trim_start_matches("protected")
                            .trim_start_matches("private")
                            .trim()
                            .split("::")
                            .last()
                            .unwrap_or("")
                            .trim()
                            .to_string();
                        if !base_name.is_empty() && base_name != *cname {
                            edges.push(RawEdge {
                                from_name: cname.clone(),
                                to_name: base_name,
                                to_file: None,
                                edge_type: "inherits",
                            });
                        }
                    }
                }
            }
        }
        _ => {}
    }

    let mut cursor = node.walk();
    let children: Vec<_> = node.named_children(&mut cursor).collect();
    for child in children {
        collect_cpp_edges(child, bytes, current_fn, edges);
    }
}

#[cfg(feature = "tree-sitter-cpp")]
fn cpp_call_target(node: tree_sitter::Node, bytes: &[u8]) -> Option<String> {
    match node.kind() {
        "identifier" => node.utf8_text(bytes).ok().map(str::to_string),
        "qualified_identifier" => node
            .child_by_field_name("name")
            .and_then(|n| n.utf8_text(bytes).ok())
            .map(str::to_string),
        "field_expression" => node
            .child_by_field_name("field")
            .and_then(|n| n.utf8_text(bytes).ok())
            .map(str::to_string),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Ruby edge extraction
// ---------------------------------------------------------------------------

#[cfg(feature = "tree-sitter-ruby")]
pub fn extract_ruby_edges(source: &str) -> Vec<RawEdge> {
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
    let bytes = source.as_bytes();
    let mut edges = Vec::new();
    collect_ruby_edges(tree.root_node(), bytes, None, &mut edges);
    edges
}

#[cfg(feature = "tree-sitter-ruby")]
fn collect_ruby_edges(
    node: tree_sitter::Node,
    bytes: &[u8],
    enclosing_fn: Option<&str>,
    edges: &mut Vec<RawEdge>,
) {
    let kind = node.kind();

    let is_fn = matches!(kind, "method" | "singleton_method");
    let fn_name: Option<String> = if is_fn {
        node.child_by_field_name("name")
            .and_then(|n| n.utf8_text(bytes).ok())
            .map(str::to_string)
    } else {
        None
    };
    let current_fn = fn_name.as_deref().or(enclosing_fn);

    if kind == "call" {
        let method_name = node
            .child_by_field_name("method")
            .and_then(|n| n.utf8_text(bytes).ok())
            .map(str::to_string)
            .unwrap_or_default();

        let has_receiver = node.child_by_field_name("receiver").is_some();

        match method_name.as_str() {
            "require" | "require_relative" if !has_receiver => {
                // Extract the string argument
                if let Some(args) = node.child_by_field_name("arguments") {
                    let mut cursor = args.walk();
                    for arg in args.named_children(&mut cursor) {
                        if matches!(arg.kind(), "string" | "string_literal") {
                            if let Ok(raw) = arg.utf8_text(bytes) {
                                let path = raw
                                    .trim()
                                    .trim_matches('"')
                                    .trim_matches('\'')
                                    .to_string();
                                if !path.is_empty() {
                                    let to_file = if method_name == "require_relative" {
                                        // relative: prefix with ./ so existing resolver picks it up
                                        Some(if path.starts_with('.') {
                                            path.clone()
                                        } else {
                                            format!("./{}", path)
                                        })
                                    } else {
                                        // plain require — could be gem; emit without to_file
                                        // and rely on unique-name fallback
                                        None
                                    };
                                    let name = std::path::Path::new(&path)
                                        .file_stem()
                                        .and_then(|s| s.to_str())
                                        .unwrap_or(&path)
                                        .to_string();
                                    edges.push(RawEdge {
                                        from_name: "__module__".to_string(),
                                        to_name: name,
                                        to_file,
                                        edge_type: "imports",
                                    });
                                }
                            }
                        }
                    }
                }
            }
            "include" | "extend" | "prepend" if !has_receiver => {
                // `include Foo` — mixin
                if let Some(args) = node.child_by_field_name("arguments") {
                    let mut cursor = args.walk();
                    for arg in args.named_children(&mut cursor) {
                        if let Ok(name) = arg.utf8_text(bytes) {
                            let name = name.trim().to_string();
                            if !name.is_empty() {
                                let from = enclosing_fn.unwrap_or("__module__");
                                edges.push(RawEdge {
                                    from_name: from.to_string(),
                                    to_name: name,
                                    to_file: None,
                                    edge_type: "uses_type",
                                });
                            }
                        }
                    }
                }
            }
            callee if !callee.is_empty() => {
                if let Some(caller) = current_fn {
                    if callee != caller {
                        edges.push(RawEdge {
                            from_name: caller.to_string(),
                            to_name: callee.to_string(),
                            to_file: None,
                            edge_type: "calls",
                        });
                    }
                }
            }
            _ => {}
        }
    } else if kind == "class" {
        let class_name = node
            .child_by_field_name("name")
            .and_then(|n| n.utf8_text(bytes).ok())
            .map(str::to_string);
        if let Some(ref cname) = class_name {
            if let Some(superclass) = node.child_by_field_name("superclass") {
                if let Ok(base) = superclass.utf8_text(bytes) {
                    let base = base.trim();
                    if !base.is_empty() {
                        edges.push(RawEdge {
                            from_name: cname.clone(),
                            to_name: base.to_string(),
                            to_file: None,
                            edge_type: "inherits",
                        });
                    }
                }
            }
        }
    }

    let mut cursor = node.walk();
    let children: Vec<_> = node.named_children(&mut cursor).collect();
    for child in children {
        collect_ruby_edges(child, bytes, current_fn, edges);
    }
}

// ---------------------------------------------------------------------------
// Fallbacks when tree-sitter features are not enabled
// ---------------------------------------------------------------------------

#[cfg(not(feature = "tree-sitter-rust"))]
pub fn extract_rust_edges(_source: &str) -> Vec<RawEdge> {
    vec![]
}

#[cfg(not(feature = "tree-sitter-typescript"))]
pub fn extract_typescript_edges(_source: &str) -> Vec<RawEdge> {
    vec![]
}

#[cfg(not(feature = "tree-sitter-javascript"))]
pub fn extract_javascript_edges(_source: &str) -> Vec<RawEdge> {
    vec![]
}

#[cfg(not(feature = "tree-sitter-python"))]
pub fn extract_python_edges(_source: &str) -> Vec<RawEdge> {
    vec![]
}

#[cfg(not(feature = "tree-sitter-go"))]
pub fn extract_go_edges(_source: &str) -> Vec<RawEdge> {
    vec![]
}

#[cfg(not(feature = "tree-sitter-java"))]
pub fn extract_java_edges(_source: &str) -> Vec<RawEdge> {
    vec![]
}

#[cfg(not(feature = "tree-sitter-c"))]
pub fn extract_c_edges(_source: &str) -> Vec<RawEdge> {
    vec![]
}

#[cfg(not(feature = "tree-sitter-cpp"))]
pub fn extract_cpp_edges(_source: &str) -> Vec<RawEdge> {
    vec![]
}

#[cfg(not(feature = "tree-sitter-ruby"))]
pub fn extract_ruby_edges(_source: &str) -> Vec<RawEdge> {
    vec![]
}

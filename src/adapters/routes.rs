use regex::Regex;

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
        "js" | "ts" => extract_express(content),
        "py" => extract_python(content),
        "rs" => extract_actix(content),
        _ => vec![],
    }
}

fn line_of(content: &str, byte_offset: usize) -> usize {
    content[..byte_offset].chars().filter(|c| *c == '\n').count() + 1
}

fn extract_express(content: &str) -> Vec<ExtractedRoute> {
    let re = Regex::new(
        r#"(?m)(app|router)\.(get|post|put|delete|patch|use)\s*\(\s*['"]([^'"]+)['"]"#,
    )
    .unwrap();

    // Pattern to match the handler identifier after the path argument: `, identifier`
    let handler_re = Regex::new(r#",\s*([A-Za-z_]\w*)\s*[,)]"#).unwrap();

    let mut routes = Vec::new();
    for cap in re.captures_iter(content) {
        let method_str = cap.get(2).unwrap().as_str();
        let method = if method_str == "use" {
            Some("*".to_string())
        } else {
            Some(method_str.to_uppercase())
        };
        let path = cap.get(3).unwrap().as_str().to_string();
        let match_end = cap.get(0).unwrap().end();
        let line = line_of(content, cap.get(0).unwrap().start());

        // Try to find a handler name after the path argument
        let rest = &content[match_end..];
        let handler_name = handler_re
            .captures(rest)
            .and_then(|hc| {
                // Only accept if it's near the start (within ~60 chars)
                let m = hc.get(0).unwrap();
                if m.start() < 60 {
                    hc.get(1).map(|n| n.as_str().to_string())
                } else {
                    None
                }
            });

        routes.push(ExtractedRoute {
            method,
            path,
            handler_name,
            framework: Some("express".to_string()),
            line,
        });
    }
    routes
}

fn extract_python(content: &str) -> Vec<ExtractedRoute> {
    let mut routes = Vec::new();

    // FastAPI/Flask style: @app.get('/path') or @router.post('/path')
    let re_method = Regex::new(
        r#"(?m)@(app|router)\.(get|post|put|delete|patch)\s*\(\s*['"]([^'"]+)['"]"#,
    )
    .unwrap();
    for cap in re_method.captures_iter(content) {
        let method = cap.get(2).unwrap().as_str().to_uppercase();
        let path = cap.get(3).unwrap().as_str().to_string();
        let line = line_of(content, cap.get(0).unwrap().start());
        routes.push(ExtractedRoute {
            method: Some(method),
            path,
            handler_name: None,
            framework: Some("fastapi".to_string()),
            line,
        });
    }

    // Flask route: @app.route('/path', methods=['GET', 'POST'])
    let re_route = Regex::new(
        r#"(?m)@(app|router)\.route\s*\(\s*['"]([^'"]+)['"](?:[^)]*methods\s*=\s*\[([^\]]*)\])?"#,
    )
    .unwrap();
    for cap in re_route.captures_iter(content) {
        let path = cap.get(2).unwrap().as_str().to_string();
        let method = cap.get(3).map(|m| {
            // Parse first method from e.g. "'GET', 'POST'"
            let methods_str = m.as_str();
            let re_m = Regex::new(r#"['"](\w+)['"]"#).unwrap();
            re_m.captures(methods_str)
                .and_then(|mc| mc.get(1))
                .map(|s| s.as_str().to_uppercase())
                .unwrap_or_else(|| "*".to_string())
        });
        let line = line_of(content, cap.get(0).unwrap().start());
        routes.push(ExtractedRoute {
            method: method.or(Some("*".to_string())),
            path,
            handler_name: None,
            framework: Some("flask".to_string()),
            line,
        });
    }

    routes
}

fn extract_actix(content: &str) -> Vec<ExtractedRoute> {
    let re = Regex::new(
        r#"(?m)#\[(get|post|put|delete|patch)\s*\(\s*"([^"]+)"\s*\)\s*\]"#,
    )
    .unwrap();
    // Find `async fn <name>` on the next non-empty line
    let fn_re = Regex::new(r#"async\s+fn\s+(\w+)"#).unwrap();

    let mut routes = Vec::new();
    for cap in re.captures_iter(content) {
        let method = cap.get(1).unwrap().as_str().to_uppercase();
        let path = cap.get(2).unwrap().as_str().to_string();
        let match_end = cap.get(0).unwrap().end();
        let line = line_of(content, cap.get(0).unwrap().start());

        // Look for `async fn handler` within the next ~200 chars
        let rest = &content[match_end..];
        let search_window = &rest[..rest.len().min(200)];
        let handler_name = fn_re
            .captures(search_window)
            .and_then(|fc| fc.get(1))
            .map(|n| n.as_str().to_string());

        routes.push(ExtractedRoute {
            method: Some(method),
            path,
            handler_name,
            framework: Some("actix".to_string()),
            line,
        });
    }
    routes
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
    fn fastapi_decorator() {
        let content = "@app.get('/items')\nasync def list_items():\n    pass\n";
        let routes = extract_routes_for_extension("py", content);
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].method.as_deref(), Some("GET"));
        assert_eq!(routes[0].path, "/items");
        assert_eq!(routes[0].framework.as_deref(), Some("fastapi"));
    }

    #[test]
    fn flask_route_with_methods() {
        let content = "@app.route('/login', methods=['GET', 'POST'])\ndef login():\n    pass\n";
        let routes = extract_routes_for_extension("py", content);
        // flask route pattern
        let flask_route = routes.iter().find(|r| r.framework.as_deref() == Some("flask"));
        assert!(flask_route.is_some());
        assert_eq!(flask_route.unwrap().path, "/login");
        assert_eq!(flask_route.unwrap().method.as_deref(), Some("GET"));
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
    fn unknown_extension_returns_empty() {
        let routes = extract_routes_for_extension("go", "// nothing here");
        assert!(routes.is_empty());
    }
}

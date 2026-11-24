use super::*;

const FIXTURE_KEY: &str = "synthetic-provider-key-for-tests";

fn json_response(body: impl Into<Vec<u8>>) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::builder()
        .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
        .body(std::io::Cursor::new(body.into()))
        .unwrap()
}

fn service(id: &str) -> SearchService {
    let selected = adapter(id).expect("built-in search adapter");
    SearchService::new(selected, FIXTURE_KEY.to_owned())
}

#[test]
fn brave_search_uses_the_fixed_keyed_request_and_maps_web_results() {
    let service = service("brave");
    assert_eq!(service.provider(), "brave");
    assert_eq!(adapter("brave").unwrap().key_name(), "BRAVE_SEARCH_API_KEY");

    let query = "Rust & count=50 é / books";
    let results = service
        .search_using(query, 2, |request| {
            assert_eq!(request.method(), Method::GET);
            let uri = request.uri();
            assert_eq!(uri.scheme_str(), Some("https"));
            assert_eq!(uri.authority().unwrap().as_str(), "api.search.brave.com");
            assert_eq!(uri.path(), "/res/v1/web/search");
            let pairs: std::collections::BTreeMap<_, _> =
                url::form_urlencoded::parse(uri.query().unwrap().as_bytes()).collect();
            assert_eq!(pairs.len(), 4);
            assert_eq!(pairs["q"], query);
            assert_eq!(pairs["count"], "2");
            assert_eq!(pairs["safesearch"], "moderate");
            assert_eq!(pairs["text_decorations"], "false");
            assert_eq!(request.headers()["x-subscription-token"], FIXTURE_KEY);
            assert!(request.headers()["x-subscription-token"].is_sensitive());
            assert!(!request.headers().contains_key("x-api-key"));
            assert_eq!(request.headers()[header::ACCEPT], "application/json");
            assert!(request.body().is_empty());
            Ok(json_response(
                br#"{"web":{"results":[
                    {"title":"First","url":"https://example.org/first","description":"An excerpt"},
                    {"title":"Second","url":"https://example.org/second"},
                    {"title":"Third","url":"https://example.org/third"}
                ]}}"#,
            ))
        })
        .unwrap();
    assert_eq!(
        serde_json::to_value(results).unwrap(),
        serde_json::json!([
            {"title":"First", "url":"https://example.org/first", "description":"An excerpt"},
            {"title":"Second", "url":"https://example.org/second", "description":""},
        ]),
    );
}

#[test]
fn keenable_search_uses_only_the_keyed_api_and_maps_snippets() {
    let selected = adapter("keenable").expect("built-in Keenable adapter");
    assert_eq!(selected.key_name(), "KEENABLE_API_KEY");
    let key = "synthetic-keenable-key-for-tests";
    let service = SearchService::new(selected, key.to_owned());
    assert_eq!(service.provider(), "keenable");

    let results = service
        .search_using("Rust \"examples\" é", 10, |request| {
            assert_eq!(request.method(), Method::POST);
            assert_eq!(request.uri(), "https://api.keenable.ai/v1/search");
            assert_eq!(request.headers()["x-api-key"], key);
            assert!(request.headers()["x-api-key"].is_sensitive());
            assert!(!request.headers().contains_key("x-subscription-token"));
            assert_eq!(request.headers()[header::CONTENT_TYPE], "application/json");
            assert_eq!(
                serde_json::from_slice::<Value>(request.body()).unwrap(),
                serde_json::json!({
                    "query": "Rust \"examples\" é",
                    "max_results": 10,
                    "snippet_max_length": 1024,
                    "moderation": "moderate",
                    "disable_cards": true,
                }),
            );
            Ok(json_response(br#"{"results":[
                {"title":"First","url":"https://example.org/first","description":"Deprecated","snippet":"**Example**"},
                {"title":"Second","url":"http://example.org/second","description":"Deprecated"}
            ]}"#))
        })
        .unwrap();
    assert_eq!(
        serde_json::to_value(results).unwrap(),
        serde_json::json!([
            {"title":"First", "url":"https://example.org/first", "description":"**Example**"},
            {"title":"Second", "url":"http://example.org/second", "description":""},
        ]),
    );
}

#[test]
fn keenable_refuses_url_only_queries_before_http() {
    let service = service("keenable");
    for query in [
        "https://example.org/articles?q=rust#reference",
        "HTTP://localhost",
        "https:example.org/article",
        "example.org/article",
        "www.example.org",
        "example.org:8080/path",
        "//example.org/article",
        "bücher.de",
        "127.0.0.1/",
        "[2001:db8::1]:8080/",
    ] {
        let result = service.search_using::<std::io::Empty>(query, 1, |_| {
            panic!("sent URL-only query to Keenable: {query}");
        });
        assert_eq!(result.err().unwrap().code(), "url_query", "{query}");
    }

    for query in ["site:example.org", "compare https://example.org", "2024"] {
        let results = service
            .search_using(query, 1, |_| Ok(json_response(br#"{"results":[]}"#)))
            .unwrap();
        assert!(results.is_empty(), "{query}");
    }
}

#[test]
fn returns_bounded_ranked_results_without_unsafe_entries_or_key_echoes() {
    use serde_json::json;

    let url = "https://example.org/result";
    let mut entries = vec![
        json!(null),
        json!({"url": url}),
        json!({"title":"Wrong excerpt type", "url":url, "description":["bad"]}),
        json!({"title":" \u{202e}\u{0000} ", "url":url}),
        json!({"title":format!("{}{}", "x".repeat(512), FIXTURE_KEY), "url":url}),
        json!({"title":"URL echo", "url":format!("{url}/{FIXTURE_KEY}")}),
        json!({"title":"Excerpt echo", "url":url, "description":format!("{}{}", "x".repeat(1024), FIXTURE_KEY)}),
        json!({"title":FIXTURE_KEY.replacen('-', "\u{0000}-", 1), "url":url}),
    ];
    for rejected in [
        "javascript:alert(1)",
        "/relative-path",
        "https:example.org/ambiguous",
        "https://user:pass@example.org/private",
        "https://@example.org/empty-userinfo",
        "http://router/",
        "http://bridge.local/",
        "http://127.1/",
        "http://169.254.169.254/",
        "https://192.168.1.2/",
        "https://[::ffff:127.0.0.1]/",
        "https://[fd00::1]/",
        "https://example.org/spoof\u{202e}",
        "https://example.org/literal space",
    ] {
        entries.push(json!({"title":"Bad URL", "url":rejected}));
    }
    entries.push(json!({"title":"Long URL", "url":format!("{url}/{}", "x".repeat(2048))}));
    entries.push(json!({"title":" \u{202e}First\u{0000}\nresult\t  ", "url":url, "description":"\t**useful**\r\ntext "}));
    let title_prefix = "é".repeat(255);
    let excerpt_prefix = "é".repeat(511);
    entries.push(json!({
        "title":format!("{title_prefix}€rest"),
        "url":"http://[2606:4700:4700::1111]/topic",
        "description":format!("{excerpt_prefix}€rest"),
    }));
    entries.push(json!({"title":"Beyond requested rank", "url":url}));

    let results = service("brave")
        .search_using("fixture query", 2, |_| {
            Ok(json_response(
                serde_json::to_vec(&json!({"web":{"results":entries}})).unwrap(),
            ))
        })
        .unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].title, "First result");
    assert_eq!(results[0].url, url);
    assert_eq!(results[0].description, "**useful** text");
    assert_eq!(results[1].title, title_prefix);
    assert_eq!(results[1].url, "http://[2606:4700:4700::1111]/topic");
    assert_eq!(results[1].description, excerpt_prefix);
}

#[test]
fn provider_failures_are_safe_codes_without_reading_error_bodies() {
    struct Unreadable;
    impl Read for Unreadable {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            panic!("the upstream error body must not be read");
        }
    }

    for id in ["brave", "keenable"] {
        let service = service(id);
        for (status, expected) in [
            (401, "credential_rejected"),
            (403, "credential_rejected"),
            (402, "quota_exhausted"),
            (429, "rate_limited"),
            (500, "provider_unavailable"),
            (529, "provider_unavailable"),
            (302, "upstream_rejected"),
            (418, "upstream_rejected"),
        ] {
            let mut requests = 0;
            let result = service.search_using("fixture query", 5, |_| {
                requests += 1;
                Ok(Response::builder()
                    .status(status)
                    .header(header::LOCATION, "https://other.example.org/")
                    .header(header::RETRY_AFTER, "1")
                    .header("x-upstream-detail", FIXTURE_KEY)
                    .body(Unreadable)
                    .unwrap())
            });
            assert_eq!(result.err().unwrap().code(), expected, "{id} HTTP {status}");
            assert_eq!(requests, 1, "no retries or fallback");
        }
    }

    struct TimedOutBody;
    impl Read for TimedOutBody {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(ureq::Error::Timeout(ureq::Timeout::RecvBody).into_io())
        }
    }
    let result = service("keenable").search_using("fixture query", 5, |_| {
        Ok(Response::builder()
            .header(header::CONTENT_TYPE, "application/json")
            .body(TimedOutBody)
            .unwrap())
    });
    assert_eq!(result.err().unwrap().code(), "timeout");
}

#[test]
fn accepts_only_bounded_json_with_the_selected_providers_envelope() {
    const RESPONSE_CAP: usize = 1024 * 1024;
    let brave = service("brave");
    let valid = br#"{"web":{"results":[]}}"#;
    for response in [
        Response::new(std::io::Cursor::new(valid.to_vec())),
        Response::builder()
            .header(header::CONTENT_TYPE, "text/html")
            .body(std::io::Cursor::new(valid.to_vec()))
            .unwrap(),
        Response::builder()
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::CONTENT_TYPE, "text/html")
            .body(std::io::Cursor::new(valid.to_vec()))
            .unwrap(),
        json_response(br#"{"web":{"results":{} }}"#),
        json_response(br#"{"web":{}}"#),
        json_response(br#"[null]"#),
        json_response(b"not-json"),
    ] {
        let result = brave.search_using("fixture query", 5, |_| Ok(response));
        assert_eq!(
            result.err().expect("invalid response accepted").code(),
            "invalid_response"
        );
    }
    for body in [br#"{"type":"search"}"#.as_slice(), br#"{"web":null}"#] {
        assert!(
            brave
                .search_using("fixture query", 5, |_| Ok(json_response(body)))
                .unwrap()
                .is_empty()
        );
    }
    let keenable = service("keenable");
    for body in [br#"{}"#.as_slice(), br#"{"results":null}"#] {
        let result = keenable.search_using("fixture query", 5, |_| Ok(json_response(body)));
        assert_eq!(result.err().unwrap().code(), "invalid_response");
    }

    struct StreamingBody<'a>(&'a mut usize);
    impl Read for StreamingBody<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            *self.0 += buf.len();
            assert!(*self.0 <= RESPONSE_CAP + 1, "read beyond the byte cap");
            buf.fill(b' ');
            Ok(buf.len())
        }
    }
    for (declared_size, expected_read) in [(None, RESPONSE_CAP + 1), (Some(RESPONSE_CAP + 1), 0)] {
        let mut bytes_read = 0;
        let result = brave.search_using("fixture query", 5, |_| {
            let mut response = Response::builder().header(header::CONTENT_TYPE, "application/json");
            if let Some(size) = declared_size {
                response = response.header(header::CONTENT_LENGTH, size.to_string());
            }
            Ok(response.body(StreamingBody(&mut bytes_read)).unwrap())
        });
        assert_eq!(result.err().unwrap().code(), "invalid_response");
        assert_eq!(bytes_read, expected_read);
    }
}

#[test]
fn http_client_never_follows_redirects_and_requires_verified_https() {
    use std::io::{BufRead, BufReader, Write};
    use std::net::{Ipv4Addr, TcpListener};
    use std::thread;

    let service = service("brave");
    let agent = &service.agent;
    assert!(agent.config().proxy().is_none());
    assert!(!agent.config().tls_config().disable_verification());
    assert_eq!(
        agent.config().timeouts().global,
        Some(Duration::from_secs(10))
    );

    let origin = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let target = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    origin.set_nonblocking(true).unwrap();
    target.set_nonblocking(true).unwrap();
    let origin_url = format!("http://{}/search", origin.local_addr().unwrap());
    let target_url = format!("http://{}/redirected", target.local_addr().unwrap());
    assert!(matches!(
        agent.get(&origin_url).call(),
        Err(ureq::Error::RequireHttpsOnly(_))
    ));

    let server = thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut stream = loop {
            match origin.accept() {
                Ok((stream, _)) => break stream,
                Err(error)
                    if error.kind() == io::ErrorKind::WouldBlock
                        && std::time::Instant::now() < deadline =>
                {
                    thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("no origin connection: {error}"),
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut reader = BufReader::new(&mut stream);
        let mut line = String::new();
        loop {
            line.clear();
            assert_ne!(reader.read_line(&mut line).unwrap(), 0);
            if line == "\r\n" {
                break;
            }
        }
        write!(stream, "HTTP/1.1 302 Found\r\nLocation: {target_url}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
    });

    // Override only this fixture's scheme and budget. No request carries a key.
    let response = agent
        .get(&origin_url)
        .config()
        .https_only(false)
        .timeout_global(Some(Duration::from_secs(2)))
        .build()
        .call();
    server.join().unwrap();
    assert!(
        matches!(target.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock),
        "HTTP client contacted the redirect target"
    );
    assert_eq!(response.unwrap().status().as_u16(), 302);
}

#[test]
fn fixture_only_adapter_reuses_the_shared_search_contract() {
    struct FixtureAdapter;

    impl Adapter for FixtureAdapter {
        fn id(&self) -> &'static str {
            "fixture"
        }

        fn key_name(&self) -> &'static str {
            "FIXTURE_SEARCH_API_KEY"
        }

        fn key_header(&self) -> HeaderName {
            HeaderName::from_static("x-fixture-key")
        }

        fn request(&self, query: &str, count: usize) -> Result<Request<Vec<u8>>, SearchError> {
            let mut url = Url::parse("https://fixture.example.org/search").unwrap();
            url.query_pairs_mut()
                .append_pair("term", query)
                .append_pair("limit", &count.to_string());
            Request::get(url.as_str())
                .body(Vec::new())
                .map_err(|_| SearchError::UpstreamRejected)
        }

        fn map_response(&self, body: &Value) -> Result<Vec<SearchResult>, SearchError> {
            let catalog = body.get("catalog").ok_or(SearchError::InvalidResponse)?;
            result_list(catalog, "excerpt")
        }
    }

    assert!(
        adapter("fixture").is_none(),
        "test adapter is never registered in the host"
    );
    assert_eq!(FixtureAdapter.key_name(), "FIXTURE_SEARCH_API_KEY");
    let service = SearchService::new(&FixtureAdapter, FIXTURE_KEY.to_owned());
    assert_eq!(service.provider(), "fixture");
    let results = service
        .search_using("sample", 1, |request| {
            assert_eq!(
                request.uri(),
                "https://fixture.example.org/search?term=sample&limit=1"
            );
            assert_eq!(request.headers()["x-fixture-key"], FIXTURE_KEY);
            assert!(!request.headers().contains_key("x-subscription-token"));
            assert!(!request.headers().contains_key("x-api-key"));
            Ok(json_response(
                br#"{"catalog":{"results":[
            {"title":"Private", "url":"http://localhost/private"},
            {"title":"Top", "url":"https://example.org/top", "excerpt":"A\u0000\n**sample**"},
            {"title":"After limit", "url":"https://example.org/after"}
        ]}}"#,
            ))
        })
        .unwrap();
    assert_eq!(
        serde_json::to_value(results).unwrap(),
        serde_json::json!([
            {"title":"Top", "url":"https://example.org/top", "description":"A **sample**"}
        ])
    );

    for key in ["", "   ", "synthetic\nx-invalid: injected"] {
        let result = SearchService::new(&FixtureAdapter, key.to_owned()).search_using::<io::Empty>(
            "sample",
            1,
            |_| panic!("HTTP with invalid credential"),
        );
        assert_eq!(result.err().unwrap().code(), "credential_rejected");
    }
}

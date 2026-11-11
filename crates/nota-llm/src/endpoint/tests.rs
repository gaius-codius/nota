use super::*;

/// A base URL is read into its host, port and path, with the path's
/// closing slash dropped and port 80 the default.
#[test]
fn a_base_url_is_read_into_its_parts() {
    let cases = [
        ("http://localhost:11434/v1", "localhost", 11434, "/v1"),
        ("HTTP://gpu-box.local/v1/", "gpu-box.local", 80, "/v1"),
        ("http://10.0.0.2:8080", "10.0.0.2", 8080, ""),
        ("http://[::1]:8080/api/v1", "[::1]", 8080, "/api/v1"),
        ("http://[2001:db8::1]", "[2001:db8::1]", 80, ""),
    ];
    for (url, host, port, base) in cases {
        let endpoint = Endpoint::parse(url).unwrap();
        let expected = Endpoint {
            host: host.to_owned(),
            port,
            base: base.to_owned(),
        };
        assert_eq!(endpoint, expected, "{url}");
    }
}

/// A URL nota can't use is refused with the reason: TLS and keys wait
/// for Settings, and nothing that could break the request's head gets in.
#[test]
fn a_url_nota_cant_use_is_refused() {
    let cases = [
        ("https://api.example.com/v1", UrlError::Https),
        ("ftp://example.com", UrlError::NotHttp),
        ("localhost:8080", UrlError::NotHttp),
        ("http://user:key@example.com/v1", UrlError::Credentials),
        ("http:///v1", UrlError::BadHost),
        ("http://exa mple.com", UrlError::BadHost),
        ("http://example.com\r\nX: y", UrlError::BadHost),
        ("http://[::1", UrlError::BadHost),
        ("http://[nope]:80", UrlError::BadHost),
        ("http://[::1]x", UrlError::BadHost),
        ("http://example.com:0", UrlError::BadPort),
        ("http://example.com:99999", UrlError::BadPort),
        ("http://example.com:", UrlError::BadPort),
        ("http://example.com/v1 HTTP/1.1\r\n", UrlError::BadPath),
        ("http://example.com/v1?key=x", UrlError::BadPath),
    ];
    for (url, refused) in cases {
        assert_eq!(Endpoint::parse(url), Err(refused), "{url:?}");
    }
    let long = format!("http://example.com/{}", "a".repeat(MAX_URL_LEN));
    assert_eq!(Endpoint::parse(&long), Err(UrlError::TooLong));
}

/// The Host header names the port unless it's HTTP's own, and questions
/// go under the base path.
#[test]
fn the_request_names_the_host_and_the_api_path() {
    let endpoint = Endpoint::parse("http://gpu-box:8080/v1").unwrap();
    assert_eq!(endpoint.host_header(), "gpu-box:8080");
    assert_eq!(endpoint.chat_path(), "/v1/chat/completions");
    let endpoint = Endpoint::parse("http://[::1]").unwrap();
    assert_eq!(endpoint.host_header(), "[::1]");
    assert_eq!(endpoint.bare_host(), "::1");
    assert_eq!(endpoint.chat_path(), "/chat/completions");
    assert_eq!(endpoint.to_string(), "http://[::1]:80");
}

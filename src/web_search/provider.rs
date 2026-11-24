use std::io::{self, Read};
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;
use ureq::http::{HeaderName, HeaderValue, Method, Request, Response, header};
use url::{Host, Url};

const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_URL_BYTES: usize = 2048;
const MAX_TITLE_BYTES: usize = 512;
const MAX_DESCRIPTION_BYTES: usize = 1024;
const BRAVE_ENDPOINT: &str = "https://api.search.brave.com/res/v1/web/search";
const KEENABLE_ENDPOINT: &str = "https://api.keenable.ai/v1/search";

// Adapters are compiled into the host. Register one here, never from the workspace.
static ADAPTERS: &[&dyn Adapter] = &[&Brave, &Keenable];

pub(super) trait Adapter: Sync {
    fn id(&self) -> &'static str;
    fn key_name(&self) -> &'static str;
    fn key_header(&self) -> HeaderName;
    // Build a request for a fixed HTTPS endpoint; the service adds its selected key.
    fn request(&self, query: &str, count: usize) -> Result<Request<Vec<u8>>, SearchError>;
    // Return candidates in provider rank order; the service applies the shared output policy.
    fn map_response(&self, body: &Value) -> Result<Vec<SearchResult>, SearchError>;
}

pub(super) fn adapter(id: &str) -> Option<&'static dyn Adapter> {
    ADAPTERS.iter().copied().find(|adapter| adapter.id() == id)
}

pub(super) struct SearchService {
    adapter: &'static dyn Adapter,
    key: String,
    agent: ureq::Agent,
}

#[derive(Serialize)]
pub(super) struct SearchResult {
    title: String,
    url: String,
    description: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SearchError {
    UrlQuery,
    CredentialRejected,
    QuotaExhausted,
    RateLimited,
    Timeout,
    ProviderUnavailable,
    InvalidResponse,
    UpstreamRejected,
}

impl SearchError {
    pub(super) fn code(self) -> &'static str {
        match self {
            Self::UrlQuery => "url_query",
            Self::CredentialRejected => "credential_rejected",
            Self::QuotaExhausted => "quota_exhausted",
            Self::RateLimited => "rate_limited",
            Self::Timeout => "timeout",
            Self::ProviderUnavailable => "provider_unavailable",
            Self::InvalidResponse => "invalid_response",
            Self::UpstreamRejected => "upstream_rejected",
        }
    }
}

impl SearchService {
    pub(super) fn new(adapter: &'static dyn Adapter, key: String) -> Self {
        Self {
            adapter,
            key,
            agent: ureq::Agent::new_with_config(transport_config()),
        }
    }

    pub(super) fn provider(&self) -> &'static str {
        self.adapter.id()
    }

    pub(super) fn search(
        &self,
        query: &str,
        count: usize,
    ) -> Result<Vec<SearchResult>, SearchError> {
        self.search_using(query, count, |request| {
            self.agent
                .run(request)
                .map(|response| response.map(ureq::Body::into_reader))
                .map_err(request_failure)
        })
    }

    // Replace only the HTTP exchange in fixtures; request construction and all response
    // checks still use the production path.
    fn search_using<R: Read>(
        &self,
        query: &str,
        count: usize,
        send: impl FnOnce(Request<Vec<u8>>) -> Result<Response<R>, SearchError>,
    ) -> Result<Vec<SearchResult>, SearchError> {
        let mut request = self.adapter.request(query, count)?;
        if self.key.trim().is_empty() {
            return Err(SearchError::CredentialRejected);
        }
        let mut key =
            HeaderValue::from_str(&self.key).map_err(|_| SearchError::CredentialRejected)?;
        key.set_sensitive(true);
        request.headers_mut().insert(self.adapter.key_header(), key);
        request
            .headers_mut()
            .insert(header::ACCEPT, HeaderValue::from_static("application/json"));

        let body = parse_response(send(request)?)?;
        Ok(self
            .adapter
            .map_response(&body)?
            .into_iter()
            .filter_map(|result| result.normalize(&self.key))
            .take(count)
            .collect())
    }
}

fn parse_response(response: Response<impl Read>) -> Result<Value, SearchError> {
    if !response.status().is_success() {
        return Err(match response.status().as_u16() {
            401 | 403 => SearchError::CredentialRejected,
            402 => SearchError::QuotaExhausted,
            429 => SearchError::RateLimited,
            500..=599 => SearchError::ProviderUnavailable,
            _ => SearchError::UpstreamRejected,
        });
    }
    let mut content_types = response.headers().get_all(header::CONTENT_TYPE).iter();
    let is_json = content_types
        .next()
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"));
    if !is_json || content_types.next().is_some() {
        return Err(SearchError::InvalidResponse);
    }
    if let Some(value) = response.headers().get(header::CONTENT_LENGTH) {
        let length = value
            .to_str()
            .ok()
            .and_then(|value| value.parse::<u64>().ok());
        if length.is_none_or(|length| length > MAX_RESPONSE_BYTES as u64) {
            return Err(SearchError::InvalidResponse);
        }
    }

    let mut bytes = Vec::new();
    response
        .into_body()
        .take((MAX_RESPONSE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(body_failure)?;
    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err(SearchError::InvalidResponse);
    }
    serde_json::from_slice(&bytes).map_err(|_| SearchError::InvalidResponse)
}

fn request_failure(error: ureq::Error) -> SearchError {
    match error {
        error if timed_out(&error) => SearchError::Timeout,
        ureq::Error::Protocol(_) | ureq::Error::LargeResponseHeader(_, _) => {
            SearchError::InvalidResponse
        }
        ureq::Error::TooManyRedirects
        | ureq::Error::RedirectFailed
        | ureq::Error::RequireHttpsOnly(_) => SearchError::UpstreamRejected,
        _ => SearchError::ProviderUnavailable,
    }
}

fn body_failure(error: io::Error) -> SearchError {
    if timed_out(&error.into()) {
        SearchError::Timeout
    } else {
        SearchError::InvalidResponse
    }
}

fn timed_out(error: &ureq::Error) -> bool {
    matches!(error, ureq::Error::Timeout(_))
        || matches!(error, ureq::Error::Io(e) if matches!(e.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock))
}

impl SearchResult {
    fn normalize(mut self, key: &str) -> Option<Self> {
        // Check before truncation so a result that echoes the key is dropped as a whole.
        if self.contains_key(key) {
            return None;
        }
        self.url = result_url(&self.url)?;
        self.title = bounded_text(&self.title, MAX_TITLE_BYTES);
        self.description = bounded_text(&self.description, MAX_DESCRIPTION_BYTES);
        // Removing controls or canonicalizing the URL can also expose a literal match.
        (!self.title.is_empty() && !self.contains_key(key)).then_some(self)
    }

    fn contains_key(&self, key: &str) -> bool {
        [&self.title, &self.url, &self.description]
            .into_iter()
            .any(|field| field.contains(key))
    }
}

fn bounded_text(raw: &str, limit: usize) -> String {
    let mut text = String::new();
    let mut space_pending = false;
    for ch in raw.chars() {
        if ch.is_whitespace() {
            space_pending = !text.is_empty();
        } else if !ch.is_control() && !invisible_format(ch) {
            if text.len() + usize::from(space_pending) + ch.len_utf8() > limit {
                break;
            }
            if space_pending {
                text.push(' ');
                space_pending = false;
            }
            text.push(ch);
        }
    }
    text
}

fn invisible_format(ch: char) -> bool {
    matches!(ch, '\u{061c}' | '\u{200b}' | '\u{200e}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}')
}

fn result_url(raw: &str) -> Option<String> {
    if raw.len() > MAX_URL_BYTES
        || raw.contains('\\')
        || raw
            .chars()
            .any(|ch| ch.is_whitespace() || ch.is_control() || invisible_format(ch))
    {
        return None;
    }
    let (scheme, rest) = raw.split_once("://")?;
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return None;
    }
    // The URL parser can erase empty userinfo; reject even a bare '@' in the authority.
    let authority = rest.split(['/', '?', '#']).next()?;
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    let url = Url::parse(raw).ok()?;
    if url.as_str().len() > MAX_URL_BYTES || !public_looking_host(url.host()?) {
        return None;
    }
    Some(url.into())
}

// Filter visibly nonpublic destinations without DNS lookup. The launcher never fetches result URLs.
fn public_looking_host(host: Host<&str>) -> bool {
    match host {
        Host::Domain(domain) => {
            let domain = domain.strip_suffix('.').unwrap_or(domain);
            let Some((_, suffix)) = domain.rsplit_once('.') else {
                return false;
            };
            !matches!(
                suffix,
                "arpa"
                    | "corp"
                    | "example"
                    | "home"
                    | "internal"
                    | "invalid"
                    | "lan"
                    | "local"
                    | "localdomain"
                    | "localhost"
                    | "onion"
                    | "test"
            ) && domain.len() <= 253
                && domain.split('.').all(|label| {
                    !label.is_empty()
                        && label.len() <= 63
                        && !label.starts_with('-')
                        && !label.ends_with('-')
                        && label
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                })
        }
        Host::Ipv4(ip) => !matches!(
            ip.octets(),
            [0 | 10 | 127 | 224..=255, ..]
                | [100, 64..=127, ..]
                | [169, 254, ..]
                | [172, 16..=31, ..]
                | [192, 0, 0 | 2, _]
                | [192, 88, 99, _]
                | [192, 168, ..]
                | [198, 18 | 19, ..]
                | [198, 51, 100, _]
                | [203, 0, 113, _]
        ),
        Host::Ipv6(ip) => {
            let [first, second, ..] = ip.segments();
            matches!(first, 0x2000..=0x3fff)
                && !matches!(
                    (first, second),
                    (0x2001, 0..=0x01ff | 0x0db8) | (0x2002, _) | (0x3fff, 0..=0x0fff)
                )
        }
    }
}

fn transport_config() -> ureq::config::Config {
    ureq::Agent::config_builder()
        .https_only(true)
        .proxy(None)
        .max_redirects(0)
        .http_status_as_error(false)
        .accept_encoding("identity")
        .timeout_global(Some(REQUEST_TIMEOUT))
        .build()
}

struct Brave;

impl Adapter for Brave {
    fn id(&self) -> &'static str {
        "brave"
    }

    fn key_name(&self) -> &'static str {
        "BRAVE_SEARCH_API_KEY"
    }

    fn key_header(&self) -> HeaderName {
        HeaderName::from_static("x-subscription-token")
    }

    fn request(&self, query: &str, count: usize) -> Result<Request<Vec<u8>>, SearchError> {
        let mut url = Url::parse(BRAVE_ENDPOINT).expect("fixed Brave search endpoint");
        url.query_pairs_mut()
            .append_pair("q", query)
            .append_pair("count", &count.to_string())
            .append_pair("safesearch", "moderate")
            .append_pair("text_decorations", "false");
        Request::builder()
            .method(Method::GET)
            .uri(url.as_str())
            .body(Vec::new())
            .map_err(|_| SearchError::UpstreamRejected)
    }

    fn map_response(&self, body: &Value) -> Result<Vec<SearchResult>, SearchError> {
        let root = body.as_object().ok_or(SearchError::InvalidResponse)?;
        match root.get("web") {
            None | Some(Value::Null) => Ok(Vec::new()),
            Some(web) => result_list(web, "description"),
        }
    }
}

struct Keenable;

#[derive(Serialize)]
struct KeenableRequest<'a> {
    query: &'a str,
    max_results: usize,
    snippet_max_length: usize,
    moderation: &'static str,
    disable_cards: bool,
}

impl Adapter for Keenable {
    fn id(&self) -> &'static str {
        "keenable"
    }

    fn key_name(&self) -> &'static str {
        "KEENABLE_API_KEY"
    }

    fn key_header(&self) -> HeaderName {
        HeaderName::from_static("x-api-key")
    }

    fn request(&self, query: &str, count: usize) -> Result<Request<Vec<u8>>, SearchError> {
        // Keenable can turn a URL query into a fetch. Keep fetching out of search.
        if is_url_query(query) {
            return Err(SearchError::UrlQuery);
        }
        let body = serde_json::to_vec(&KeenableRequest {
            query,
            max_results: count,
            snippet_max_length: 1024,
            moderation: "moderate",
            disable_cards: true,
        })
        .map_err(|_| SearchError::UpstreamRejected)?;
        Request::builder()
            .method(Method::POST)
            .uri(KEENABLE_ENDPOINT)
            .header(header::CONTENT_TYPE, "application/json")
            .body(body)
            .map_err(|_| SearchError::UpstreamRejected)
    }

    fn map_response(&self, body: &Value) -> Result<Vec<SearchResult>, SearchError> {
        // Keenable's deprecated description is not an excerpt; only use snippet.
        result_list(body, "snippet")
    }
}

fn is_url_query(query: &str) -> bool {
    let query = query.trim();
    if query.chars().any(char::is_whitespace) {
        return false;
    }
    if Url::parse(query).is_ok_and(|url| matches!(url.scheme(), "http" | "https") && url.has_host())
    {
        return true;
    }
    Url::parse(&format!("https://{query}")).is_ok_and(|url| match url.host() {
        Some(_) if query.starts_with("//") => true,
        Some(Host::Domain(host)) => {
            let host = host.trim_end_matches('.');
            host.contains('.') || host == "localhost"
        }
        // The URL parser can read a single integer as IPv4, but that is also a keyword.
        Some(Host::Ipv4(_)) => query
            .split(['/', '?', '#'])
            .next()
            .is_some_and(|s| s.contains('.')),
        Some(Host::Ipv6(_)) => true,
        None => false,
    })
}

fn result_list(body: &Value, excerpt: &str) -> Result<Vec<SearchResult>, SearchError> {
    let entries = body
        .get("results")
        .and_then(Value::as_array)
        .ok_or(SearchError::InvalidResponse)?;
    Ok(entries
        .iter()
        .filter_map(|entry| {
            let description = match entry.get(excerpt) {
                None | Some(Value::Null) => "",
                Some(value) => value.as_str()?,
            };
            Some(SearchResult {
                title: entry.get("title")?.as_str()?.to_owned(),
                url: entry.get("url")?.as_str()?.to_owned(),
                description: description.to_owned(),
            })
        })
        .collect())
}

#[cfg(test)]
mod tests;

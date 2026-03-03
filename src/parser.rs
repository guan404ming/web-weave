use scraper::{Html, Selector};
use url::Url;

/// Non-crawlable file extensions.
const SKIP_EXTENSIONS: &[&str] = &[
    ".pdf", ".jpg", ".jpeg", ".png", ".gif", ".svg", ".webp", ".ico", ".bmp", ".tiff",
    ".css", ".js", ".json", ".xml", ".rss", ".atom",
    ".zip", ".tar", ".gz", ".bz2", ".7z", ".rar",
    ".mp3", ".mp4", ".avi", ".mov", ".wmv", ".flv", ".webm", ".ogg",
    ".exe", ".dmg", ".iso", ".msi", ".deb", ".rpm",
    ".woff", ".woff2", ".ttf", ".eot", ".otf",
    ".doc", ".docx", ".xls", ".xlsx", ".ppt", ".pptx",
];

/// Tracking query params to strip during normalization.
const TRACKING_PARAMS: &[&str] = &[
    "utm_source",
    "utm_medium",
    "utm_campaign",
    "utm_content",
    "utm_term",
    "fbclid",
    "gclid",
    "ref",
    "source",
];

/// Extract and normalize links from an HTML page.
pub fn extract_links(html_body: &str, base_url: &str, max_links: usize) -> Vec<String> {
    let base = match Url::parse(base_url) {
        Ok(u) => u,
        Err(_) => return Vec::new(),
    };

    let document = Html::parse_document(html_body);
    let selector = match Selector::parse("a[href]") {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };

    let mut links = Vec::new();
    for element in document.select(&selector) {
        if links.len() >= max_links {
            break;
        }
        if let Some(href) = element.value().attr("href") {
            if let Some(normalized) = normalize_url(href, &base) {
                links.push(normalized);
            }
        }
    }
    links
}

/// Parse a sitemap XML and extract <loc> URLs.
pub fn parse_sitemap_xml(xml: &str, max_urls: usize) -> Vec<String> {
    let document = Html::parse_document(xml);
    let selector = match Selector::parse("loc") {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };

    let mut urls = Vec::new();
    for element in document.select(&selector) {
        if urls.len() >= max_urls {
            break;
        }
        let text = element.text().collect::<String>();
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            if let Ok(url) = Url::parse(trimmed) {
                if is_crawlable(&url) {
                    urls.push(url.to_string());
                }
            }
        }
    }
    urls
}

/// Normalize a URL: resolve relative, lowercase host, remove fragment,
/// strip tracking params, sort query params.
pub fn normalize_url(raw: &str, base: &Url) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    let parsed = base.join(trimmed).ok()?;

    if !is_crawlable(&parsed) {
        return None;
    }

    let mut normalized = parsed.clone();
    normalized.set_fragment(None);

    // Remove default ports
    if (normalized.scheme() == "http" && normalized.port() == Some(80))
        || (normalized.scheme() == "https" && normalized.port() == Some(443))
    {
        let _ = normalized.set_port(None);
    }

    // Filter tracking query params and sort remaining
    if let Some(query) = normalized.query() {
        let mut params: Vec<(String, String)> = query
            .split('&')
            .filter_map(|pair| {
                let mut parts = pair.splitn(2, '=');
                let key = parts.next()?.to_lowercase();
                let value = parts.next().unwrap_or("").to_string();
                if TRACKING_PARAMS.contains(&key.as_str()) {
                    None
                } else {
                    Some((key, value))
                }
            })
            .collect();

        if params.is_empty() {
            normalized.set_query(None);
        } else {
            params.sort_by(|a, b| a.0.cmp(&b.0));
            let query_str: Vec<String> = params
                .iter()
                .map(|(k, v)| {
                    if v.is_empty() {
                        k.clone()
                    } else {
                        format!("{}={}", k, v)
                    }
                })
                .collect();
            normalized.set_query(Some(&query_str.join("&")));
        }
    }

    Some(normalized.to_string())
}

/// Extract the domain (host) from a URL string.
pub fn extract_domain(url_str: &str) -> Option<String> {
    Url::parse(url_str)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_lowercase()))
}

/// Check if a URL is worth crawling.
fn is_crawlable(url: &Url) -> bool {
    match url.scheme() {
        "http" | "https" => {}
        _ => return false,
    }

    let path = url.path().to_lowercase();
    for ext in SKIP_EXTENSIONS {
        if path.ends_with(ext) {
            return false;
        }
    }

    true
}

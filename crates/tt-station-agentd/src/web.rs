// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! The assistant's window on the web: `web_search` (DuckDuckGo's keyless HTML
//! endpoint) and `read_webpage` (fetch a page, keep its readable text). The
//! model runs on the box; only the search query and the URLs it reads leave it.
//!
//! `read_webpage` refuses addresses on the box's own networks (loopback,
//! private, link-local): a page or a prompt must not be able to steer the
//! assistant into the agent's own API or other devices on the LAN. The check
//! is made where the connection is: the client resolves names through
//! [`PublicOnly`], so a name that answers with a public address for the
//! check and a private one for the connection (DNS rebinding) still can't
//! reach the LAN, and every redirect is checked again.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs};
use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;
use serde::Serialize;

const SEARCH_URL: &str = "https://html.duckduckgo.com/html/";
const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) tt-station-agentd";
/// Most text handed to the model from one page.
pub const MAX_PAGE_CHARS: usize = 6000;
const MAX_PAGE_BYTES: usize = 3 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SearchHit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

static RESULT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?s)class="result__a"[^>]*href="([^"]+)"[^>]*>(.*?)</a>.*?class="result__snippet"[^>]*>(.*?)</a>"#)
        .expect("valid regex")
});
static TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<[^>]*>").expect("valid regex"));
static DROP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)<(script|style|noscript|svg|nav|footer|header|aside|form|iframe)\b.*?</(script|style|noscript|svg|nav|footer|header|aside|form|iframe)>")
        .expect("valid regex")
});
static BLOCK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)</?(p|div|br|li|h[1-6]|tr|section|article|blockquote|pre)\b[^>]*>")
        .expect("valid regex")
});
static TITLE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<title[^>]*>(.*?)</title>").expect("valid regex"));
static MAIN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)<(article|main)\b[^>]*>(.*)</(article|main)>").expect("valid regex")
});

/// Decode the HTML entities that matter for reading.
pub fn decode_entities(s: &str) -> String {
    let mut out = s
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
        .replace("&rsquo;", "'")
        .replace("&lsquo;", "'")
        .replace("&ldquo;", "\"")
        .replace("&rdquo;", "\"")
        .replace("&mdash;", "—")
        .replace("&ndash;", "–")
        .replace("&hellip;", "…");
    out = out.replace("&amp;", "&");
    out
}

fn clean_inline(html: &str) -> String {
    let text = decode_entities(&TAG.replace_all(html, ""));
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Parse DuckDuckGo's HTML results page.
pub fn parse_search(html: &str, limit: usize) -> Vec<SearchHit> {
    RESULT
        .captures_iter(html)
        .filter_map(|c| {
            let mut url = decode_entities(&c[1]);
            // Older pages wrap links in a redirect: //duckduckgo.com/l/?uddg=<url>
            if let Some(rest) = url.split("uddg=").nth(1) {
                url = percent_decode(rest.split('&').next().unwrap_or(rest));
            }
            if url.starts_with("//") {
                url = format!("https:{url}");
            }
            if !url.starts_with("http") || url.contains("duckduckgo.com/y.js") {
                return None; // ads
            }
            Some(SearchHit {
                title: clean_inline(&c[2]),
                url,
                snippet: clean_inline(&c[3]),
            })
        })
        .take(limit)
        .collect()
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A page's title and readable text: the `<article>`/`<main>` when there is
/// one, without scripts, navigation and markup, paragraphs kept.
pub fn readable_text(html: &str) -> (Option<String>, String) {
    let title = TITLE
        .captures(html)
        .map(|c| clean_inline(&c[1]))
        .filter(|t| !t.is_empty());
    let body = MAIN
        .captures(html)
        .map(|c| c.get(2).map(|m| m.as_str()).unwrap_or_default().to_string())
        .unwrap_or_else(|| html.to_string());
    let body = DROP.replace_all(&body, " ");
    let body = BLOCK.replace_all(&body, "\n");
    let text = decode_entities(&TAG.replace_all(&body, " "));
    let lines: Vec<String> = text
        .lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|l| l.len() > 1)
        .collect();
    (title, lines.join("\n"))
}

/// Whether `ip` is on this box's own networks, or isn't a public unicast
/// address at all (never fetched).
pub fn is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => private_v4(v4),
        IpAddr::V6(v6) => {
            let s = v6.segments();
            // An IPv4 address written as IPv6 is that IPv4 address:
            // mapped (::ffff:a.b.c.d), compatible (::a.b.c.d), NAT64 (64:ff9b::a.b.c.d).
            let embedded = v6
                .to_ipv4_mapped()
                .or_else(|| (s[..6] == [0; 6]).then(|| v6.to_ipv4()).flatten())
                .or_else(|| {
                    (s[..6] == [0x64, 0xff9b, 0, 0, 0, 0])
                        .then(|| Ipv4Addr::from(((s[6] as u32) << 16) | s[7] as u32))
                });
            if let Some(v4) = embedded {
                return private_v4(v4);
            }
            // 6to4 (2002:a.b.c.d::/48) carries one too.
            if s[0] == 0x2002 && private_v4(Ipv4Addr::from(((s[1] as u32) << 16) | s[2] as u32)) {
                return true;
            }
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (s[0] & 0xfe00) == 0xfc00 // unique local fc00::/7
                || (s[0] & 0xffc0) == 0xfe80 // link-local fe80::/10
                || (s[0] & 0xffc0) == 0xfec0 // site-local fec0::/10 (deprecated, still routed by some)
        }
    }
}

fn private_v4(v4: Ipv4Addr) -> bool {
    let [a, b, ..] = v4.octets();
    v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local()
        || v4.is_unspecified()
        || v4.is_broadcast()
        || v4.is_multicast()
        || a == 0 // 0.0.0.0/8: "this network"
        || (a == 192 && b == 0 && v4.octets()[2] == 0) // 192.0.0.0/24: protocol assignments
        || (a == 198 && (b == 18 || b == 19)) // 198.18.0.0/15: benchmarking
        || a >= 240 // 240.0.0.0/4: reserved
        || crate::pairqr::is_tailscale(&v4) // 100.64.0.0/10: carrier-grade NAT, Tailscale
}

/// Name resolution that answers only with public addresses: what the client
/// connects to is what was checked.
struct PublicOnly;

impl reqwest::dns::Resolve for PublicOnly {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        Box::pin(async move {
            let found: Vec<SocketAddr> =
                tokio::net::lookup_host((name.as_str(), 0)).await?.collect();
            let public: Vec<SocketAddr> = found
                .iter()
                .copied()
                .filter(|a| !is_private(a.ip()))
                .collect();
            if public.is_empty() || public.len() < found.len() {
                return Err(format!(
                    "{} is on a private network; only public pages can be read",
                    name.as_str()
                )
                .into());
            }
            Ok(Box::new(public.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(20))
        // Redirects are re-checked by hand (they could point at the LAN).
        .redirect(reqwest::redirect::Policy::none())
        .dns_resolver(std::sync::Arc::new(PublicOnly))
        .build()
        .map_err(|e| e.to_string())
}

/// Search the web.
pub async fn search(query: &str, limit: usize) -> Result<Vec<SearchHit>, String> {
    let html = client()?
        .post(SEARCH_URL)
        .form(&[("q", query)])
        .send()
        .await
        .map_err(|e| format!("search failed: {e}"))?
        .text()
        .await
        .map_err(|e| format!("search failed: {e}"))?;
    Ok(parse_search(&html, limit))
}

/// Fetch a public web page's readable text (following up to 5 redirects, each
/// checked against [`is_private`]).
pub async fn read(url: &str) -> Result<(String, Option<String>, String), String> {
    read_limited(url, MAX_PAGE_CHARS).await
}

/// [`read`], keeping up to `max_chars` of the page's text (research reads more of a page).
pub async fn read_limited(
    url: &str,
    max_chars: usize,
) -> Result<(String, Option<String>, String), String> {
    let client = client()?;
    let mut current = url.trim().to_string();
    for _ in 0..6 {
        let parsed =
            reqwest::Url::parse(&current).map_err(|_| format!("not a web address: {current}"))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err("only http and https pages can be read".to_string());
        }
        let host = parsed
            .host_str()
            .ok_or("that address has no host")?
            // An IPv6 literal comes bracketed ("[::1]").
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_string();
        let port = parsed.port_or_known_default().unwrap_or(443);
        let addrs: Vec<IpAddr> = tokio::task::spawn_blocking(move || {
            (host.as_str(), port)
                .to_socket_addrs()
                .map(|a| a.map(|s| s.ip()).collect())
                .unwrap_or_default()
        })
        .await
        .map_err(|e| e.to_string())?;
        if addrs.is_empty() {
            return Err(format!("couldn't find {}", parsed.host_str().unwrap_or("")));
        }
        if addrs.iter().any(|ip| is_private(*ip)) {
            return Err(
                "that address is on a private network; only public pages can be read".to_string(),
            );
        }
        let response = client
            .get(parsed.clone())
            .send()
            .await
            .map_err(|e| format!("couldn't load the page: {e}"))?;
        if response.status().is_redirection() {
            let next = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or("a redirect without a location")?;
            current = parsed.join(next).map_err(|e| e.to_string())?.to_string();
            continue;
        }
        if !response.status().is_success() {
            return Err(format!("the page answered {}", response.status()));
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_lowercase();
        if !(content_type.contains("html")
            || content_type.contains("text")
            || content_type.is_empty())
        {
            return Err(format!("that isn't a web page ({content_type})"));
        }
        let bytes = response.bytes().await.map_err(|e| e.to_string())?;
        let html = String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_PAGE_BYTES)]).into_owned();
        let (title, mut text) = if content_type.contains("html") || html.contains("<html") {
            readable_text(&html)
        } else {
            (None, html)
        };
        if text.chars().count() > max_chars {
            text = text.chars().take(max_chars).collect::<String>() + "\n[…truncated]";
        }
        return Ok((current, title, text));
    }
    Err("too many redirects".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_duckduckgo_results() {
        let html = r#"
        <a rel="nofollow" class="result__a" href="https://docs.tenstorrent.com/aibs/blackhole/p300.html">Blackhole p300c Card &mdash; Specs</a>
        <a class="result__snippet" href="https://docs.tenstorrent.com/x">Two <b>Blackhole</b> ASICs, each with 120 Tensix Cores &amp; 32 GB.</a>
        <a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fa%3Fb%3Dc&amp;rut=x">Old style</a>
        <a class="result__snippet" href="x">snippet two</a>
        "#;
        let hits = parse_search(html, 5);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "Blackhole p300c Card — Specs");
        assert_eq!(
            hits[0].snippet,
            "Two Blackhole ASICs, each with 120 Tensix Cores & 32 GB."
        );
        assert_eq!(hits[1].url, "https://example.com/a?b=c");
        assert_eq!(parse_search(html, 1).len(), 1);
    }

    #[test]
    fn readable_text_keeps_the_article_and_drops_the_chrome() {
        let html = "<html><head><title>Fox &amp; Snow</title><style>p{}</style></head><body>\
            <nav>Home | About</nav><article><h1>Red foxes</h1><p>Foxes hunt   in snow.</p>\
            <script>track()</script><p>They hear mice under it.</p></article><footer>©</footer></body></html>";
        let (title, text) = readable_text(html);
        assert_eq!(title.as_deref(), Some("Fox & Snow"));
        assert_eq!(
            text,
            "Red foxes\nFoxes hunt in snow.\nThey hear mice under it."
        );
    }

    #[test]
    fn private_networks_are_refused() {
        for ip in [
            "127.0.0.1",
            "10.0.0.122",
            "192.168.1.5",
            "169.254.1.1",
            "100.100.1.1",
            "::1",
            "fd00::1",
            "fe80::1",
            // Written as IPv6.
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "::ffff:192.168.1.5",
            "::127.0.0.1",
            "64:ff9b::7f00:1",
            "2002:c0a8:105::1",
            // Not public unicast.
            "0.1.2.3",
            "100.64.0.1",
            "100.127.255.254",
            "169.254.169.254",
            "224.0.0.1",
            "255.255.255.255",
            "240.0.0.1",
            "198.18.0.1",
            "ff02::1",
            "fec0::1",
            "fc00::1",
            "::",
        ] {
            assert!(is_private(ip.parse().unwrap()), "{ip}");
        }
        for ip in [
            "93.184.216.34",
            "2606:4700::1111",
            "::ffff:93.184.216.34",
            "100.128.0.1",
            "8.8.8.8",
        ] {
            assert!(!is_private(ip.parse().unwrap()), "{ip}");
        }
    }

    #[tokio::test]
    async fn read_refuses_the_box_itself() {
        for url in [
            "http://127.0.0.1:8765/status",
            "http://[::ffff:127.0.0.1]:8765/status",
            "http://localhost:8765/status",
            "http://0.0.0.0:8765/",
            "http://169.254.169.254/latest/meta-data/",
        ] {
            assert!(read(url).await.unwrap_err().contains("private"), "{url}");
        }
        assert!(read("file:///etc/passwd")
            .await
            .unwrap_err()
            .contains("http"));
    }

    /// A name that answers with a private address when the connection is
    /// made (DNS rebinding: public for the check, private for the fetch)
    /// never gets a connection: the client's own resolver refuses it.
    #[tokio::test]
    async fn the_client_itself_never_connects_to_a_private_address() {
        use reqwest::dns::Resolve;
        let name: reqwest::dns::Name = "localhost".parse().unwrap();
        let err = PublicOnly.resolve(name).await.err().expect("refused");
        assert!(err.to_string().contains("private"), "{err}");
        // And a request through the client (the redirect-check path bypassed)
        // fails before reaching the box's own listener.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let hit = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let seen = hit.clone();
        tokio::spawn(async move {
            if listener.accept().await.is_ok() {
                seen.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        });
        let r = client()
            .unwrap()
            .get(format!("http://localhost:{port}/"))
            .send()
            .await;
        assert!(r.is_err());
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            !hit.load(std::sync::atomic::Ordering::SeqCst),
            "connected to the box"
        );
    }
}

//! crawllab bench: spider driving a remote obscura over CDP, against local
//! fixtures (an Anubis instance and an echo server). `run.sh` brings the
//! containers up and calls one subcommand per check; each exits non-zero on
//! failure.
//!
//! Environment: CDP (the browser's /json/version URL), ANUBIS (http://ip:8923),
//! ECHO (http://ip:8000), ECHO_HOST (the echo server by a hostname only the
//! browser can resolve).

use spider::client::cookie::Jar;
use spider::configuration::{CrawlEnhancement, WaitForDelay};
use spider::page::Page;
use spider::utils::disposition::PageDisposition;
use spider::website::Website;
use std::sync::Arc;
use std::time::Duration;

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} is not set"))
}

/// One page through the remote browser, configured the way the crawler runs:
/// the browser owns the identity and nothing is synthesised on the page.
async fn fetch(url: &str, wait_secs: u64) -> Option<Page> {
    fetch_with_jar(url, wait_secs, Default::default()).await
}

/// `fetch` with the crawl's cookie jar: crawls given one jar share a session.
async fn fetch_with_jar(url: &str, wait_secs: u64, jar: Arc<Jar>) -> Option<Page> {
    let mut w = Website::new(url);
    w.with_limit(1)
        .with_depth(0)
        .with_user_agent(None)
        .with_stealth(false)
        .with_fingerprint(false)
        .with_enhancement(CrawlEnhancement::PointerAssist, false)
        .with_wait_for_delay(Some(WaitForDelay::new(Some(Duration::from_secs(
            wait_secs,
        )))))
        .with_chrome_connection(Some(env("CDP")));
    let mut w = w.build().ok()?;
    w.cookie_jar = jar;
    let mut rx = w.subscribe(4);
    let got = tokio::spawn(async move { rx.recv().await.ok() });
    w.crawl().await;
    w.unsubscribe();
    got.await.ok().flatten()
}

fn title(page: &Page) -> String {
    let html = page.get_html();
    html.split("<title")
        .nth(1)
        .and_then(|t| t.split('>').nth(1))
        .and_then(|t| t.split("</title").next())
        .unwrap_or("")
        .to_string()
}

struct Checks(Vec<(String, bool)>);

impl Checks {
    fn check(&mut self, name: impl Into<String>, ok: bool, detail: impl std::fmt::Display) {
        let name = name.into();
        println!("{} {name}: {detail}", if ok { "PASS" } else { "FAIL" });
        self.0.push((name, ok));
    }
    fn exit(self) -> ! {
        let failed = self.0.iter().filter(|(_, ok)| !ok).count();
        println!("{} checks, {failed} failed", self.0.len());
        std::process::exit(if failed == 0 { 0 } else { 1 })
    }
}

/// Every Anubis challenge method, at the policy's base and hard settings. The
/// JavaScript proof-of-work waits are long because solve time is random and
/// rises with host load (fast at difficulty 5: 3 to 33 s on a loaded Mac).
async fn anubis(c: &mut Checks) {
    let base = env("ANUBIS");
    let paths = [
        ("metarefresh", 6),
        ("preact", 6),
        ("fast", 15),
        ("slow", 15),
        ("sha256", 6),
        ("argon2id", 6),
        ("hashx", 6),
        ("hard/fast5", 90),
        ("hard/sha256-6", 20),
        ("hard/hashx6", 20),
        ("hard/argon2id4", 20),
    ];
    for (path, wait) in paths {
        let page = fetch(&format!("{base}/{path}"), wait).await;
        let (t, d) = page
            .as_ref()
            .map(|p| (title(p), p.disposition()))
            .unwrap_or((String::new(), PageDisposition::Error));
        c.check(
            format!("anubis /{path}"),
            t.starts_with("Backend") && d == PageDisposition::Content,
            format!("title={t:?} disposition={d:?}"),
        );
    }
}

/// A page that navigates itself during the wait reports the document reached.
async fn refresh(c: &mut Checks) {
    let echo = env("ECHO");
    let page = fetch(&format!("{echo}/rh"), 4).await;
    let (url, status) = page
        .as_ref()
        .map(|p| (p.get_url_final().to_string(), p.status_code.as_u16()))
        .unwrap_or_default();
    c.check(
        "refresh to content",
        url.ends_with("/content") && status == 200,
        format!("final_url={url} status={status}"),
    );
    let page = fetch(&format!("{echo}/rgone"), 4).await;
    let (url, status) = page
        .as_ref()
        .map(|p| (p.get_url_final().to_string(), p.status_code.as_u16()))
        .unwrap_or_default();
    c.check(
        "refresh to a 404",
        url.ends_with("/gone") && status == 404,
        format!("final_url={url} status={status}"),
    );
}

/// Two crawls sharing a cookie jar share the session: the browser reads the
/// jar's cookies before each page and writes its own back after. run.sh checks
/// that Anubis challenged only the first.
async fn session(c: &mut Checks) {
    let url = format!("{}/fast", env("ANUBIS"));
    let jar = Arc::new(Jar::default());
    for crawl in ["first", "second"] {
        let t = fetch_with_jar(&url, 15, jar.clone())
            .await
            .map(|p| title(&p))
            .unwrap_or_default();
        c.check(
            format!("session {crawl} crawl"),
            t == "Backend /fast",
            format!("title={t:?}"),
        );
    }
}

/// A hostname only the browser resolves still loads (no local-DNS veto).
async fn dns(c: &mut Checks) {
    let page = fetch(&format!("{}/content", env("ECHO_HOST")), 2).await;
    let (t, status) = page
        .as_ref()
        .map(|p| (title(p), p.status_code.as_u16()))
        .unwrap_or_default();
    c.check(
        "browser-only hostname",
        status == 200 && t == "Real content",
        format!("status={status} title={t:?}"),
    );
}

/// A challenge page is classified as one, and its honeypot link is not followed.
async fn challenge_page(c: &mut Checks) {
    let path = env("CHALLENGE_HTML");
    let html = std::fs::read(&path).expect("challenge page");
    let url = format!("{}/fast", env("ANUBIS"));
    let res = spider::utils::PageResponse {
        content: Some(html),
        status_code: spider::reqwest::StatusCode::OK,
        ..Default::default()
    };
    let mut page = spider::page::build(&url, res);
    c.check(
        "challenge disposition",
        page.disposition() == PageDisposition::Challenge,
        format!("{:?}", page.disposition()),
    );
    let selectors = spider::page::get_page_selectors(&url, false, false);
    let links = page.links(&selectors, &None).await;
    let honeypot = links.iter().any(|l| l.inner().contains("honeypot"));
    c.check(
        "honeypot link ignored",
        !honeypot,
        format!("{} links", links.len()),
    );
}

#[tokio::main]
async fn main() {
    let mut c = Checks(Vec::new());
    match std::env::args().nth(1).as_deref() {
        Some("anubis") => anubis(&mut c).await,
        Some("refresh") => refresh(&mut c).await,
        Some("dns") => dns(&mut c).await,
        Some("session") => session(&mut c).await,
        Some("challenge-page") => challenge_page(&mut c).await,
        Some("fetch") => {
            // Used by run.sh around checks it verifies from container logs.
            let url = std::env::args().nth(2).expect("url");
            let page = fetch(&url, 2).await;
            c.check(
                format!("fetch {url}"),
                page.is_some_and(|p| p.status_code.is_success()),
                "",
            );
        }
        other => {
            eprintln!(
                "usage: runner <anubis|refresh|dns|session|challenge-page|fetch URL> (got {other:?})"
            );
            std::process::exit(2);
        }
    }
    c.exit()
}

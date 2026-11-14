//! Skyward's own releases on GitHub: which is the newest, and the zip in
//! it for one phone, as the release workflow names each phone's.

use anyhow::{bail, ensure, Context, Result};
use serde::Deserialize;
use std::io::Read;
use std::time::{Duration, Instant};

/// Where GitHub lists Skyward's releases, newest first: the fork this
/// recovery is built and kept in, whose releases carry the builds for the
/// phones it is made for. The list holds pre-releases too, which is how a
/// build is published here; `/latest` leaves them out.
pub const LATEST: &str = "https://api.github.com/repos/luoxunyong/skyward-by-Redmi-K70/releases?per_page=20";

/// The releases page on github.com, read where the API turns the phone
/// down, as an address that asked too often is.
pub const PAGE: &str = "https://github.com/luoxunyong/skyward-by-Redmi-K70/releases";

/// The version of GitHub's API the answer is read as.
const API_VERSION: &str = "2022-11-28";

/// The newest release, as the build of Skyward that asked sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Newest {
    /// Its version, such as 0.1.4, or the build a `build-<build>-<time>`
    /// tag names.
    pub version: String,
    /// It is newer than the build that asked.
    pub newer: bool,
    /// Its zip for the phone; none when it has none.
    pub build: Option<Build>,
}

/// A phone's zip in a release, which holds Skyward's image for the phone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Build {
    /// The zip's own name, such as
    /// skyward-v0.1.4-ishtar-OS3.0.308.0.WMACNXM.zip.
    pub name: String,
    /// Where GitHub's API hands it out, which [`locate`] asks for where the
    /// file itself is.
    pub url: String,
    /// The release's own link to it on github.com.
    pub link: String,
    pub size: u64,
    /// Its SHA-256 as GitHub gives it, in hex.
    pub sha256: String,
}

/// A GitHub download mirror: the name the pages show, and the prefix its
/// address takes before the release's own link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mirror {
    pub name: &'static str,
    pub prefix: &'static str,
}

/// The mirrors tried beside github.com itself. Each takes the release's
/// own address after the prefix; one that is down or cannot carry the file
/// is passed over by the download.
pub const MIRRORS: &[Mirror] = &[
    Mirror { name: "ghfast", prefix: "https://ghfast.top/" },
    Mirror { name: "ghproxy", prefix: "https://ghproxy.net/" },
    Mirror { name: "gh-proxy", prefix: "https://gh-proxy.com/" },
    Mirror { name: "moeyy", prefix: "https://github.moeyy.xyz/" },
    Mirror { name: "gitmirror", prefix: "https://hub.gitmirror.com/" },
    Mirror { name: "llkk", prefix: "https://gh.llkk.cc/" },
];

/// The releases page and the mirrors fronting it, for timing the sources
/// when no build is known.
pub fn page_sources() -> Vec<String> {
    let mut out = vec![PAGE.to_string()];
    out.extend(MIRRORS.iter().map(|m| format!("{}{}", m.prefix, PAGE)));
    out
}

impl Build {
    /// Where to download the zip from: the file's own short-lived address,
    /// where GitHub's API gave one, then the release's link on github.com,
    /// then the mirrors that front it.
    pub fn sources(&self, located: Option<String>) -> Vec<String> {
        let mut out: Vec<String> = located.into_iter().collect();
        out.push(self.link.clone());
        out.extend(MIRRORS.iter().map(|m| format!("{}{}", m.prefix, self.link)));
        out
    }
}

/// What a source of `link` is called on the pages: GitHub itself, one of the
/// mirrors, or GitHub's API handing out a short-lived address.
pub fn source_name(url: &str, link: &str) -> String {
    if url == link {
        return "GitHub".into();
    }
    for mirror in MIRRORS {
        if url == format!("{}{}", mirror.prefix, link) {
            return mirror.name.into();
        }
    }
    "GitHub API".into()
}

/// How long `url` took to answer a one-byte request, or `None` where it did
/// not answer with the file. The agent is made for short probes.
pub fn probe(agent: &ureq::Agent, url: &str) -> Option<Duration> {
    let start = Instant::now();
    let mut response = agent.get(url).header("Range", "bytes=0-0").call().ok()?;
    let status = response.status();
    if !status.is_success() && !status.is_redirection() {
        return None;
    }
    let mut byte = [0u8; 1];
    let mut body = response.body_mut().as_reader();
    let _ = body.read(&mut byte);
    Some(start.elapsed())
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    url: String,
    name: String,
    size: u64,
    /// The digest GitHub took of the file as it was uploaded, such as
    /// `sha256:` and the SHA-256 in hex.
    digest: Option<String>,
    browser_download_url: String,
}

/// Why GitHub turned a question down, as it says.
#[derive(Deserialize)]
struct Message {
    message: String,
}

/// The newest release that `answer`, GitHub's answer at [`LATEST`] or one
/// release of it, tells of, and its zip for the phone `codename` names, as
/// the build of version `running` sees it.
pub fn newest(answer: &str, codename: &str, running: &str) -> Result<Newest> {
    newest_with(answer, codename, running, "")
}

/// [`newest`], with the build `running_build` that asks, for releases
/// tagged `build-<build>-<time>`.
fn newest_with(answer: &str, codename: &str, running: &str, running_build: &str) -> Result<Newest> {
    let release: Release = serde_json::from_str(answer).context("read GitHub's answer")?;
    newest_release(release, codename, running, running_build)
}

/// The first release in `answer`, GitHub's answer at [`LATEST`], which
/// lists them newest first, and its zip for the phone `codename` names.
fn newest_releases(answer: &str, codename: &str, running: &str, running_build: &str) -> Result<Newest> {
    let releases: Vec<Release> = serde_json::from_str(answer).context("read GitHub's answer")?;
    let release = releases.into_iter().next().context("GitHub names no release")?;
    newest_release(release, codename, running, running_build)
}

/// The version `release` carries and its zip for `codename`.
fn newest_release(release: Release, codename: &str, running: &str, running_build: &str) -> Result<Newest> {
    let tag = release.tag_name;
    let (version, newer) = version_and_newer(&tag, running, running_build)?;
    let build = match release.assets.into_iter().find(|a| is_for(&a.name, codename)) {
        None => None,
        Some(a) => {
            let sha256 = a.digest.as_deref().and_then(|d| d.strip_prefix("sha256:")).with_context(|| format!("GitHub gives no SHA-256 of {}", a.name))?;
            ensure!(sha256.len() == 64 && sha256.bytes().all(|b| b.is_ascii_hexdigit()), "GitHub gives {} the SHA-256 {sha256}", a.name);
            Some(Build { sha256: sha256.to_lowercase(), name: a.name, url: a.url, link: a.browser_download_url, size: a.size })
        }
    };
    Ok(Newest { version, newer, build })
}

/// The version a release's `tag` names, and whether it is newer than the
/// build that asked. This fork tags its builds `build-<build>-<time>`, and
/// compares them by the build; a `v<version>` tag is compared by its
/// version, as upstream releases are.
fn version_and_newer(tag: &str, running: &str, running_build: &str) -> Result<(String, bool)> {
    if let Some(rest) = tag.strip_prefix("build-") {
        let id = rest.split('-').next().unwrap_or_default();
        ensure!(!id.is_empty(), "the newest release is tagged {tag}, which names no build");
        return Ok((id.to_string(), running_build.is_empty() || !running_build.eq_ignore_ascii_case(id)));
    }
    let version = tag.strip_prefix('v').with_context(|| format!("the newest release is tagged {tag}, which is not v and a version"))?;
    let newest = semver::Version::parse(version).with_context(|| format!("the newest release is tagged {tag}"))?;
    let this = semver::Version::parse(running).with_context(|| format!("this build's version is {running}"))?;
    Ok((version.to_string(), newest > this))
}

/// Whether `name` is a zip for the phone `codename` names. This fork names
/// its build `skyward-<codename>-<version>-<build>-<date>.zip`, and the
/// upstream one `skyward-<tag>-<codename>-<stock>.zip`, so the codename is
/// looked for as a whole part of the name.
fn is_for(name: &str, codename: &str) -> bool {
    name.strip_suffix(".zip")
        .is_some_and(|stem| stem.starts_with("skyward-") && stem.split('-').any(|part| part.eq_ignore_ascii_case(codename)))
}

/// Ask GitHub for Skyward's newest release, and its zip for the phone
/// `codename` names, as the build of version `running` and commit `build`.
/// The API is asked first; where it turns the phone down, as an address
/// that asked too often is, the releases page is read instead.
pub fn ask(agent: &ureq::Agent, codename: &str, running: &str, build: &str) -> Result<Newest> {
    ask_api(agent, codename, running, build).or_else(|api| {
        ask_page(agent, codename, running, build).with_context(|| format!("{api:#}"))
    })
}

/// Ask GitHub's API for its releases, newest first, and read the first.
fn ask_api(agent: &ureq::Agent, codename: &str, running: &str, build: &str) -> Result<Newest> {
    let mut response = agent
        .get(LATEST)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", API_VERSION)
        .header("User-Agent", format!("Skyward/{running}"))
        .call()
        .with_context(|| format!("ask {LATEST}"))?;
    let status = response.status();
    let body = response.body_mut().read_to_string().with_context(|| format!("read the answer of {LATEST}"))?;
    if !status.is_success() {
        bail!(refusal(status, &body));
    }
    newest_releases(&body, codename, running, build)
}

/// Read GitHub's releases page and the newest release's assets for the
/// phone `codename` names. The page lists pre-releases, which `/latest`
/// leaves out, and carries each asset's SHA-256.
fn ask_page(agent: &ureq::Agent, codename: &str, running: &str, build: &str) -> Result<Newest> {
    let mut response = agent
        .get(PAGE)
        .header("User-Agent", format!("Skyward/{running}"))
        .call()
        .with_context(|| format!("ask {PAGE}"))?;
    let status = response.status();
    let body = response.body_mut().read_to_string().with_context(|| format!("read {PAGE}"))?;
    ensure!(status.is_success(), "{PAGE} answered {status}");
    let tag = first_tag(&body).with_context(|| format!("{PAGE} names no release"))?;
    let url = format!("https://github.com/luoxunyong/skyward-by-Redmi-K70/releases/expanded_assets/{tag}");
    let mut response = agent
        .get(&url)
        .header("User-Agent", format!("Skyward/{running}"))
        .call()
        .with_context(|| format!("ask {url}"))?;
    let status = response.status();
    let assets = response.body_mut().read_to_string().with_context(|| format!("read {url}"))?;
    ensure!(status.is_success(), "{url} answered {status}");
    let (version, newer) = version_and_newer(&tag, running, build)?;
    let build = page_build(&assets, codename)?;
    Ok(Newest { version, newer, build })
}

/// The tag of the newest release on the releases page. Its table of
/// contents names it first, and its own link does too.
fn first_tag(html: &str) -> Option<String> {
    const ANCHOR: &str = "\"anchorId\":\"release-";
    if let Some(at) = html.find(ANCHOR) {
        let rest = &html[at + ANCHOR.len()..];
        if let Some(end) = rest.find('"') {
            if end > 0 {
                return Some(rest[..end].to_string());
            }
        }
    }
    const LINK: &str = "/releases/tag/";
    let at = html.find(LINK)? + LINK.len();
    let rest = &html[at..];
    let end = rest.find(|c: char| matches!(c, '"' | '\'' | '?' | '#' | '<' | ' ')).unwrap_or(rest.len());
    (end > 0).then(|| rest[..end].to_string())
}

/// The zip for `codename` among the assets the releases page shows, with
/// its link, SHA-256 and size. None where the release has no zip for it.
fn page_build(assets: &str, codename: &str) -> Result<Option<Build>> {
    for block in assets.split("<li").skip(1) {
        let block = &block[..block.find("</li>").unwrap_or(block.len())];
        let Some(href) = between(block, "href=\"", "\"") else { continue };
        if !href.contains("/releases/download/") {
            continue;
        }
        let name = href.rsplit('/').next().unwrap_or_default();
        if !is_for(name, codename) {
            continue;
        }
        let sha256 = sha256_in(block).with_context(|| format!("the release page gives {name} no SHA-256"))?;
        let link = if href.starts_with("http") { href.to_string() } else { format!("https://github.com{href}") };
        return Ok(Some(Build { name: name.to_string(), url: link.clone(), link, size: shown_size(block).unwrap_or(0), sha256 }));
    }
    Ok(None)
}

/// What lies in `s` between `before` and the next `after`.
fn between<'a>(s: &'a str, before: &str, after: &str) -> Option<&'a str> {
    let at = s.find(before)? + before.len();
    let rest = &s[at..];
    let end = rest.find(after)?;
    Some(&rest[..end])
}

/// The first `sha256:` in `block` followed by 64 hex digits, in lowercase.
fn sha256_in(block: &str) -> Option<String> {
    let mut rest = block;
    while let Some(at) = rest.find("sha256:") {
        let start = at + "sha256:".len();
        let hex = &rest[start..];
        let end = hex.find(|c: char| !c.is_ascii_hexdigit()).unwrap_or(hex.len());
        if end == 64 {
            return Some(hex[..end].to_ascii_lowercase());
        }
        rest = &rest[start..];
    }
    None
}

/// The size the page shows beside an asset, such as `41.7 MB`, in bytes.
fn shown_size(block: &str) -> Option<u64> {
    for (unit, scale) in [(" MB", 1_000_000u64), (" KB", 1_000u64), (" GB", 1_000_000_000u64), (" B", 1u64)] {
        if let Some(at) = block.find(unit) {
            let before = &block[..at];
            let start = before.rfind(|c: char| !(c.is_ascii_digit() || c == '.')).map_or(0, |i| i + 1);
            if let Ok(n) = before[start..].parse::<f64>() {
                return Some((n * scale as f64).round() as u64);
            }
        }
    }
    None
}

/// Where the file a build's `url` hands out is now, as GitHub's API sends
/// the build of version `running` that asks for it on: a short-lived
/// address of the file itself, away from github.com.
pub fn locate(agent: &ureq::Agent, url: &str, running: &str) -> Result<String> {
    let mut response = agent
        .get(url)
        .header("Accept", "application/octet-stream")
        .header("X-GitHub-Api-Version", API_VERSION)
        .header("User-Agent", format!("Skyward/{running}"))
        .config()
        .max_redirects(0)
        .build()
        .call()
        .with_context(|| format!("ask {url}"))?;
    let status = response.status();
    if status.is_redirection() {
        let location = response.headers().get(ureq::http::header::LOCATION).with_context(|| format!("GitHub answered {status}, and names no address"))?;
        return Ok(location.to_str().context("GitHub names an address that is not text")?.to_string());
    }
    ensure!(!status.is_success(), "GitHub sends the file itself in its answer, at no address of its own");
    let body = response.body_mut().read_to_string().with_context(|| format!("read the answer of {url}"))?;
    bail!(refusal(status, &body))
}

/// Why GitHub turned a question down, as it says, as when too many questions
/// came from the address that asked.
fn refusal(status: ureq::http::StatusCode, body: &str) -> String {
    match serde_json::from_str::<Message>(body) {
        Ok(m) => format!("GitHub answered {status}: {}", m.message),
        Err(_) => format!("GitHub answered {status}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What GitHub answered of the newest release on 2026-11-07, with the
    /// parts that do not matter here left out.
    const ANSWERED_013: &str = r#"{"url":"https://api.github.com/repos/kirklin/skyward/releases/437365758","tag_name":"v0.1.3","name":"v0.1.3","draft":false,"prerelease":false,"assets":[{"url":"https://api.github.com/repos/kirklin/skyward/releases/assets/719751468","id":719751468,"name":"SHA256SUMS","label":"","content_type":"application/octet-stream","state":"uploaded","size":2337,"digest":"sha256:052f9ca757baeb2eac78c8e546bcc54178f6bb9bf9e5bde53fa81b3f48448957","browser_download_url":"https://github.com/kirklin/skyward/releases/download/v0.1.3/SHA256SUMS"},{"url":"https://api.github.com/repos/kirklin/skyward/releases/assets/719751598","id":719751598,"name":"skyward-v0.1.3-ishtar-OS3.0.308.0.WMACNXM.zip","label":"Xiaomi 13 Ultra (ishtar)","content_type":"application/zip","state":"uploaded","size":42739608,"digest":"sha256:62b66f7d13a0eb6c9af3359b5ec6f9c223fd9a86b1f8abab3e9d2646cb2ce534","browser_download_url":"https://github.com/kirklin/skyward/releases/download/v0.1.3/skyward-v0.1.3-ishtar-OS3.0.308.0.WMACNXM.zip"},{"url":"https://api.github.com/repos/kirklin/skyward/releases/assets/719751747","id":719751747,"name":"skyward-v0.1.3-nezha-OS4.0.4.0.XPACNXM.zip","label":"Xiaomi 17 Ultra (nezha)","content_type":"application/zip","state":"uploaded","size":43145196,"digest":"sha256:9f6165079529418d8f6635beddfdd9abf9a70c89d69eb467a9922b0db69bfaf1","browser_download_url":"https://github.com/kirklin/skyward/releases/download/v0.1.3/skyward-v0.1.3-nezha-OS4.0.4.0.XPACNXM.zip"}]}"#;

    #[test]
    fn the_newest_release_gives_the_zip_for_the_phone() {
        let n = newest(ANSWERED_013, "ishtar", "0.1.2").unwrap();
        assert_eq!(n.version, "0.1.3");
        assert!(n.newer);
        assert_eq!(
            n.build,
            Some(Build {
                name: "skyward-v0.1.3-ishtar-OS3.0.308.0.WMACNXM.zip".into(),
                url: "https://api.github.com/repos/kirklin/skyward/releases/assets/719751598".into(),
                link: "https://github.com/kirklin/skyward/releases/download/v0.1.3/skyward-v0.1.3-ishtar-OS3.0.308.0.WMACNXM.zip".into(),
                size: 42739608,
                sha256: "62b66f7d13a0eb6c9af3359b5ec6f9c223fd9a86b1f8abab3e9d2646cb2ce534".into(),
            })
        );
        assert_eq!(newest(ANSWERED_013, "nezha", "0.1.2").unwrap().build.unwrap().size, 43145196);
    }

    #[test]
    fn a_zip_need_not_name_the_stock_build_it_is_made_on() {
        // A release carrying one phone's build alone is named with the tag
        // and the phone, and nothing after them.
        let plain = ANSWERED_013.replace("skyward-v0.1.3-ishtar-OS3.0.308.0.WMACNXM", "skyward-v0.1.3-ishtar");
        let named = newest(&plain, "ishtar", "0.1.2").unwrap().build.expect("the phone's own build");
        assert_eq!(named.name, "skyward-v0.1.3-ishtar.zip");
        // A phone whose name begins with another's is not taken for it.
        assert!(newest(ANSWERED_013, "ishta", "0.1.2").unwrap().build.is_none(), "`ishta` is not `ishtar`");
        assert!(newest(ANSWERED_013, "ishtarx", "0.1.2").unwrap().build.is_none(), "nor is `ishtarx`");
    }

    #[test]
    fn a_zip_is_downloaded_from_where_github_sends_it_then_from_its_link() {
        let build = newest(ANSWERED_013, "ishtar", "0.1.2").unwrap().build.unwrap();
        let link = "https://github.com/kirklin/skyward/releases/download/v0.1.3/skyward-v0.1.3-ishtar-OS3.0.308.0.WMACNXM.zip";
        let sources = build.sources(Some("https://release-assets.githubusercontent.com/a?sig=1".into()));
        assert_eq!(&sources[..2], ["https://release-assets.githubusercontent.com/a?sig=1", link]);
        assert_eq!(sources.len(), 2 + MIRRORS.len());
        assert_eq!(source_name(&sources[2], link), MIRRORS[0].name);
        let direct = build.sources(None);
        assert_eq!(direct[0], link, "where GitHub's API gave no address");
        assert_eq!(direct.len(), 1 + MIRRORS.len());
    }

    /// A server on this computer that answers one request with `answer`,
    /// and gives the request it had.
    fn server(answer: String) -> (String, std::thread::JoinHandle<String>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/repos/kirklin/skyward/releases/assets/719751598", listener.local_addr().unwrap());
        let held = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut chunk = [0u8; 512];
            while !request.ends_with(b"\r\n\r\n") {
                let n = s.read(&mut chunk).unwrap();
                request.extend_from_slice(&chunk[..n]);
            }
            s.write_all(answer.as_bytes()).unwrap();
            String::from_utf8(request).unwrap()
        });
        (url, held)
    }

    #[test]
    fn github_s_api_sends_a_zip_on_to_where_the_file_is() {
        let agent = crate::http::agent(Default::default());
        let sent_on = "HTTP/1.1 302 Found\r\nLocation: https://release-assets.githubusercontent.com/a?sig=1\r\nContent-Length: 0\r\n\r\n";
        let (url, held) = server(sent_on.into());
        assert_eq!(locate(&agent, &url, "0.1.2").unwrap(), "https://release-assets.githubusercontent.com/a?sig=1", "the address, not followed");
        let request = held.join().unwrap().to_lowercase();
        assert!(request.contains("accept: application/octet-stream\r\n"), "asked for the file itself: {request}");
        assert!(request.contains("user-agent: skyward/0.1.2\r\n"), "{request}");
        let why = r#"{"message":"API rate limit exceeded for 203.0.113.7"}"#;
        let (url, _) = server(format!("HTTP/1.1 403 Forbidden\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{why}", why.len()));
        assert_eq!(locate(&agent, &url, "0.1.2").unwrap_err().to_string(), "GitHub answered 403 Forbidden: API rate limit exceeded for 203.0.113.7");
        let (url, _) = server("HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nPK".into());
        assert!(locate(&agent, &url, "0.1.2").is_err(), "the file itself, with no address to download it from again");
    }

    #[test]
    fn a_release_is_newer_only_than_an_older_build() {
        assert!(!newest(ANSWERED_013, "ishtar", "0.1.3").unwrap().newer, "the same version");
        assert!(!newest(ANSWERED_013, "ishtar", "0.2.0").unwrap().newer, "a build ahead of every release");
        assert!(newest(ANSWERED_013, "ishtar", "0.1.3-rc.1").unwrap().newer, "a build before the release it led to");
        assert!(newest(ANSWERED_013, "ishtar", "0.1").is_err(), "a version this build cannot have");
    }

    #[test]
    fn a_phone_is_found_by_its_whole_codename() {
        assert_eq!(newest(ANSWERED_013, "fuxi", "0.1.2").unwrap().build, None, "a phone the release has no zip for");
        assert_eq!(newest(ANSWERED_013, "ish", "0.1.2").unwrap().build, None, "the start of another phone's codename");
        assert_eq!(newest(ANSWERED_013, "SHA256SUMS", "0.1.2").unwrap().build, None);
    }

    #[test]
    fn a_zip_needs_its_sha256_and_a_release_its_version() {
        let unsummed = ANSWERED_013.replace("\"digest\":\"sha256:62b66f7d13a0eb6c9af3359b5ec6f9c223fd9a86b1f8abab3e9d2646cb2ce534\"", "\"digest\":null");
        let e = newest(&unsummed, "ishtar", "0.1.2").unwrap_err();
        assert_eq!(e.to_string(), "GitHub gives no SHA-256 of skyward-v0.1.3-ishtar-OS3.0.308.0.WMACNXM.zip");
        assert!(newest(&unsummed, "nezha", "0.1.2").is_ok(), "another phone's zip has its own");
        let untagged = ANSWERED_013.replace("\"tag_name\":\"v0.1.3\"", "\"tag_name\":\"nightly\"");
        assert!(newest(&untagged, "ishtar", "0.1.2").is_err());
        assert!(newest("{}", "ishtar", "0.1.2").is_err());
    }

    #[test]
    fn a_build_tag_names_the_build_and_is_newer_unless_it_is_this_one() {
        let answered = r#"{"tag_name":"build-7d47475-20261114-195313","assets":[{"url":"https://api.github.com/repos/luoxunyong/skyward-by-Redmi-K70/releases/assets/1","name":"skyward-vermeer-v0.2.0-7d47475-20261114.zip","size":43682291,"digest":"sha256:44eb4518956aaeb97e8851cd04f954fc6b013a2b01bb6df5e6916bbf8e3a40c3","browser_download_url":"https://github.com/luoxunyong/skyward-by-Redmi-K70/releases/download/build-7d47475-20261114-195313/skyward-vermeer-v0.2.0-7d47475-20261114.zip"}]}"#;
        let n = newest_with(answered, "vermeer", "0.2.0", "7d47475").unwrap();
        assert_eq!(n.version, "7d47475");
        assert!(!n.newer, "the build running is the release");
        assert_eq!(n.build.expect("the phone's build").size, 43682291);
        assert!(newest_with(answered, "vermeer", "0.2.0", "f4836b4").unwrap().newer, "another build is newer");
    }

    #[test]
    fn a_release_list_is_read_newest_first() {
        let list = format!("[{ANSWERED_013}]");
        assert_eq!(newest_releases(&list, "ishtar", "0.1.2", "").unwrap().version, "0.1.3");
    }

    #[test]
    fn the_release_page_names_its_newest_release_and_the_phone_s_zip() {
        let page = r#"<script>{"payload":{"releasesIndexRoute":{"toc":[{"anchorId":"release-build-7d47475-20261114-195313"},{"anchorId":"release-0.1.4"}]}}}</script>"#;
        assert_eq!(first_tag(page).as_deref(), Some("build-7d47475-20261114-195313"));
        let assets = r#"<li><a href="/luoxunyong/skyward-by-Redmi-K70/releases/download/build-7d47475-20261114-195313/skyward-vermeer-v0.2.0-7d47475-20261114.zip"><span class="text-bold">skyward-vermeer-v0.2.0-7d47475-20261114.zip</span></a><span class="Truncate-text">sha256:44eb4518956aaeb97e8851cd04f954fc6b013a2b01bb6df5e6916bbf8e3a40c3</span><span>41.7 MB</span></li>"#;
        let build = page_build(assets, "vermeer").unwrap().expect("the phone's build");
        assert_eq!(build.name, "skyward-vermeer-v0.2.0-7d47475-20261114.zip");
        assert_eq!(build.sha256, "44eb4518956aaeb97e8851cd04f954fc6b013a2b01bb6df5e6916bbf8e3a40c3");
        assert_eq!(build.size, 41_700_000);
        assert_eq!(build.link, "https://github.com/luoxunyong/skyward-by-Redmi-K70/releases/download/build-7d47475-20261114-195313/skyward-vermeer-v0.2.0-7d47475-20261114.zip");
        assert_eq!(build.url, build.link, "the page has no API address, and the link sends the download on");
    }
}

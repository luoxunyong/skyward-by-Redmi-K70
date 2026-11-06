//! Whether a network joined reaches the internet, told as the phone's own
//! system tells it: a page the phone maker keeps answers 204 and nothing
//! more, and a network that wants a login in a browser first answers in
//! its stead, most often sending on to its login page.

use crate::dns;
use anyhow::{anyhow, bail, ensure, Context, Result};
use std::io::{ErrorKind, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::time::Duration;
use ureq::http::Uri;

/// How long reaching the page may take, and how long its answer may.
const WAIT: Duration = Duration::from_secs(5);

/// The most of an answer read for its status and headers.
const HEAD: usize = 16 * 1024;

/// How far a network joined reaches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reach {
    /// The page answered as it does.
    Online,
    /// The network answered in the page's stead: it wants a login in a
    /// browser first, at the page it sends on to where it names one.
    Login(Option<String>),
    /// Nothing answered, or not as either would: why.
    Offline(String),
}

impl std::fmt::Display for Reach {
    /// How far a network reaches, said of the network.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Reach::Online => f.write_str("reaches the internet"),
            Reach::Login(Some(page)) => write!(f, "wants a login in a browser first, at {page}"),
            Reach::Login(None) => f.write_str("wants a login in a browser first"),
            Reach::Offline(why) => write!(f, "does not reach the internet: {why}"),
        }
    }
}

/// Ask `page`, a plain HTTP address, looking its host up in `servers`.
pub fn probe(page: &str, servers: &[Ipv4Addr]) -> Reach {
    probe_at(page, servers).0
}

/// Ask `page`, a plain HTTP address, looking its host up in `servers`, and
/// give how far the network reaches beside the time the answer's own server
/// said it was, in Unix seconds, where it said one.
pub fn probe_at(page: &str, servers: &[Ipv4Addr]) -> (Reach, Option<i64>) {
    match answer(page, servers) {
        Ok((code, location, date)) => (judge(page, code, location), date),
        Err(e) => (Reach::Offline(format!("{e:#}")), None),
    }
}

/// How far a network reaches, by what `page` answered: 204 is the page
/// itself, another answer a browser would show or follow is the network's
/// login, and any other is neither.
fn judge(page: &str, code: u16, location: Option<String>) -> Reach {
    match code {
        204 => Reach::Online,
        200..=399 => Reach::Login(location),
        _ => Reach::Offline(format!("{page} answered {code}")),
    }
}

/// What `page` answers: its status, where it sends on to, and the time its
/// server said it was, in Unix seconds.
fn answer(page: &str, servers: &[Ipv4Addr]) -> Result<(u16, Option<String>, Option<i64>)> {
    let uri: Uri = page.parse().with_context(|| format!("{page} is not an address"))?;
    let (host, authority) = match (uri.host(), uri.authority()) {
        (Some(host), Some(authority)) => (host, authority.as_str()),
        _ => bail!("{page} names no host"),
    };
    let port = uri.port_u16().unwrap_or(80);
    let path = uri.path_and_query().map_or("/", |p| p.as_str());
    let mut tried = Vec::new();
    for address in dns::lookup(host, servers)? {
        let at = SocketAddr::new(IpAddr::V4(address), port);
        match TcpStream::connect_timeout(&at, WAIT) {
            Ok(stream) => return ask(stream, authority, path).with_context(|| format!("ask {page} at {at}")),
            Err(e) => tried.push(format!("{at}: {e}")),
        }
    }
    Err(anyhow!("could not reach {host}: {}", tried.join("; ")))
}

/// Ask over `stream` for `path` of `authority`, and read the head of the
/// answer. Some networks' gateways answer only a request whose header
/// names are spelled as browsers spell them, Host with a capital, and the
/// HTTP client the rest of the recovery uses writes every name in small
/// letters, so the request is written here as a browser writes it.
fn ask(mut stream: TcpStream, authority: &str, path: &str) -> Result<(u16, Option<String>, Option<i64>)> {
    stream.set_read_timeout(Some(WAIT)).context("time the connection")?;
    stream.set_write_timeout(Some(WAIT)).context("time the connection")?;
    stream.write_all(format!("GET {path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n").as_bytes()).context("send the request")?;
    let mut head = Vec::new();
    let mut chunk = [0u8; 2048];
    loop {
        let n = match stream.read(&mut chunk) {
            Ok(n) => n,
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => bail!("no answer in {} s", WAIT.as_secs()),
            Err(e) => return Err(e).context("read the answer"),
        };
        ensure!(n > 0, "the connection closed before the answer's head ended");
        head.extend_from_slice(&chunk[..n]);
        let mut headers = [httparse::EMPTY_HEADER; 64];
        let mut response = httparse::Response::new(&mut headers);
        if response.parse(&head).context("read the head of the answer")?.is_complete() {
            let code = response.code.context("the answer has no status")?;
            let location = response.headers.iter().find(|h| h.name.eq_ignore_ascii_case("location")).map(|h| String::from_utf8_lossy(h.value).into_owned());
            let date = response.headers.iter().find(|h| h.name.eq_ignore_ascii_case("date")).and_then(|h| Date::parse(&String::from_utf8_lossy(h.value)));
            return Ok((code, location, date));
        }
        ensure!(head.len() < HEAD, "the head of the answer runs past {HEAD} bytes");
    }
}

/// An HTTP `Date` header, `Sun, 06 Nov 1994 08:49:37 GMT`: the Unix seconds
/// it names.
pub struct Date;

impl Date {
    /// The Unix seconds the header `text` names; `None` when it is not a
    /// date in the form HTTP sends.
    pub fn parse(text: &str) -> Option<i64> {
        // The day and month names an HTTP date starts with, and the month
        // it names, numbered from one.
        let rest = text.trim();
        let (_, rest) = rest.split_once(", ")?;
        let mut parts = rest.split(' ');
        let day: i64 = parts.next()?.parse().ok()?;
        let month = match parts.next()? {
            "Jan" => 1,
            "Feb" => 2,
            "Mar" => 3,
            "Apr" => 4,
            "May" => 5,
            "Jun" => 6,
            "Jul" => 7,
            "Aug" => 8,
            "Sep" => 9,
            "Oct" => 10,
            "Nov" => 11,
            "Dec" => 12,
            _ => return None,
        };
        let year: i64 = parts.next()?.parse().ok()?;
        let time = parts.next()?;
        if !parts.next()?.eq_ignore_ascii_case("GMT") {
            return None;
        }
        let mut clock = time.split(':');
        let (hour, minute, second): (i64, i64, i64) = (clock.next()?.parse().ok()?, clock.next()?.parse().ok()?, clock.next()?.parse().ok()?);
        if clock.next().is_some() || !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60 {
            return None;
        }
        Some(seconds(year, month, day, hour, minute, second.min(59)))
    }
}

/// The Unix seconds of a civil date and time, by the Gregorian calendar,
/// counted from 1970 as the C library counts it.
fn seconds(year: i64, month: i64, day: i64, hour: i64, minute: i64, second: i64) -> i64 {
    // Days from 1970-01-01 to `year`-`month`-`day`, as Howard Hinnant's
    // `days_from_civil` counts them.
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    days * 86_400 + hour * 3600 + minute * 60 + second
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    /// A server on the loopback answering one request with `answer`, and
    /// only a request that spells its Host header as browsers do, as some
    /// gateways answer.
    fn gateway(answer: &'static str) -> String {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut chunk = [0u8; 512];
            while !request.ends_with(b"\r\n\r\n") {
                let n = stream.read(&mut chunk).unwrap();
                request.extend_from_slice(&chunk[..n]);
            }
            if String::from_utf8_lossy(&request).contains("\r\nHost: ") {
                stream.write_all(answer.as_bytes()).unwrap();
            }
            // Held open, as such a gateway holds it.
            thread::sleep(Duration::from_secs(1));
        });
        format!("http://127.0.0.1:{port}/generate_204")
    }

    #[test]
    fn the_page_itself_answers_204() {
        let page = gateway("HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n");
        assert_eq!(probe(&page, &[]), Reach::Online);
    }

    #[test]
    fn a_network_that_wants_a_login_sends_on_to_its_page() {
        let page = gateway("HTTP/1.1 302 Moved Temporarily\r\nCache-Control: no-cache\r\nLocation: http://192.168.64.254:2060/redirect/?pip=192.168.72.76\r\n\r\n");
        let login = probe(&page, &[]);
        assert_eq!(login, Reach::Login(Some("http://192.168.64.254:2060/redirect/?pip=192.168.72.76".into())), "the head is read as it ends, the rest not waited for");
    }

    #[test]
    fn a_page_shown_in_the_pages_stead_is_a_login_and_an_error_is_neither() {
        assert_eq!(judge("p", 200, None), Reach::Login(None));
        assert_eq!(judge("p", 307, Some("http://login".into())), Reach::Login(Some("http://login".into())));
        assert_eq!(judge("http://a/generate_204", 503, None), Reach::Offline("http://a/generate_204 answered 503".into()));
    }

    #[test]
    fn nothing_listening_reaches_nothing() {
        let port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap().local_addr().unwrap().port();
        let Reach::Offline(why) = probe(&format!("http://127.0.0.1:{port}/generate_204"), &[]) else { panic!("a closed port reaches nothing") };
        assert!(why.contains("could not reach 127.0.0.1"), "{why}");
        assert!(matches!(probe("http://connect.rom.miui.com/generate_204", &[]), Reach::Offline(_)), "a name without a DNS server to ask");
    }

    #[test]
    fn an_answers_date_is_the_time_its_server_said_it_was() {
        // 2026-11-06 12:06:31 GMT.
        assert_eq!(Date::parse("Fri, 06 Nov 2026 12:06:31 GMT"), Some(1_793_966_791));
        assert_eq!(Date::parse("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0), "the epoch itself");
        assert_eq!(Date::parse("Sat, 29 Feb 2020 23:59:59 GMT"), Some(1_583_020_799), "a leap day");
        assert_eq!(Date::parse("Fri, 06 Nov 2026 12:06:31 CET"), None, "not the time zone HTTP sends");
        assert_eq!(Date::parse("yesterday"), None);
        assert_eq!(Date::parse("Fri, 32 Nov 2026 12:06:31 GMT"), None, "no such day");
    }

    #[test]
    fn a_page_that_says_when_it_answered_gives_the_time() {
        let page = gateway("HTTP/1.1 204 No Content\r\nDate: Thu, 01 Jan 1970 00:00:00 GMT\r\nContent-Length: 0\r\n\r\n");
        assert_eq!(probe_at(&page, &[]), (Reach::Online, Some(0)));
        let page = gateway("HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n");
        assert_eq!(probe_at(&page, &[]), (Reach::Online, None), "an answer with no date");
    }
}

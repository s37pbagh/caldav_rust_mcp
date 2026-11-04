//! CalDAV (RFC 4791) client: discovery, calendar listing, event queries and CRUD.

use anyhow::{Context, Result, anyhow, bail};
use reqwest::{Method, StatusCode, header};
use serde::Serialize;
use tokio::sync::Mutex;
use url::Url;

const NS_DAV: &str = "DAV:";
const NS_CALDAV: &str = "urn:ietf:params:xml:ns:caldav";
const NS_CS: &str = "http://calendarserver.org/ns/";
const NS_ICAL: &str = "http://apple.com/ns/ical/";

#[derive(Clone)]
pub enum Auth {
    None,
    Basic { user: String, pass: String },
    Bearer(String),
}

pub struct Config {
    pub base_url: Url,
    pub auth: Auth,
}

pub struct CalDavClient {
    http: reqwest::Client,
    cfg: Config,
    home: Mutex<Option<Url>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Calendar {
    pub url: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    pub components: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ctag: Option<String>,
}

impl Calendar {
    pub fn supports_events(&self) -> bool {
        self.components.is_empty() || self.components.iter().any(|c| c == "VEVENT")
    }
}

/// A calendar object resource (one .ics file on the server).
#[derive(Debug, Clone)]
pub struct CalendarObject {
    pub url: Url,
    pub etag: Option<String>,
    pub data: String,
}

#[derive(Clone, Copy)]
pub enum Precondition<'a> {
    /// Create only (If-None-Match: *).
    MustNotExist,
    /// Update only if unchanged (If-Match: etag).
    Matches(&'a str),
    /// Unconditional write.
    None,
}

pub struct HttpResponse {
    pub status: StatusCode,
    pub headers: header::HeaderMap,
    pub body: String,
}

/// One <DAV:response> with its successful properties.
#[derive(Debug, Default)]
struct DavResponse {
    href: String,
    props: Vec<DavProp>,
}

#[derive(Debug)]
struct DavProp {
    ns: String,
    name: String,
    text: String,
    hrefs: Vec<String>,
    /// (namespace, local name, value of "name" attribute) for each child element.
    children: Vec<(String, String, Option<String>)>,
}

impl DavResponse {
    fn prop(&self, ns: &str, name: &str) -> Option<&DavProp> {
        self.props.iter().find(|p| p.ns == ns && p.name == name)
    }
}

fn parse_multistatus(xml: &str) -> Result<Vec<DavResponse>> {
    let doc = roxmltree::Document::parse(xml).context("invalid XML in multistatus response")?;
    let is = |n: &roxmltree::Node, name: &str| n.is_element() && n.tag_name().namespace() == Some(NS_DAV) && n.tag_name().name() == name;
    let mut out = Vec::new();
    for resp in doc.descendants().filter(|n| is(n, "response")) {
        let mut r = DavResponse::default();
        for child in resp.children() {
            if is(&child, "href") {
                r.href = child.text().unwrap_or("").trim().to_string();
            } else if is(&child, "propstat") {
                let ok = child
                    .children()
                    .find(|n| is(n, "status"))
                    .and_then(|n| n.text())
                    .map(|s| s.contains(" 200"))
                    .unwrap_or(true);
                if !ok {
                    continue;
                }
                for prop in child.children().filter(|n| is(n, "prop")) {
                    for p in prop.children().filter(|n| n.is_element()) {
                        let text: String = p.descendants().filter(|n| n.is_text()).filter_map(|n| n.text()).collect();
                        r.props.push(DavProp {
                            ns: p.tag_name().namespace().unwrap_or("").to_string(),
                            name: p.tag_name().name().to_string(),
                            text: text.trim().to_string(),
                            hrefs: p
                                .descendants()
                                .filter(|n| is(n, "href"))
                                .filter_map(|n| n.text())
                                .map(|t| t.trim().to_string())
                                .collect(),
                            children: p
                                .children()
                                .filter(|n| n.is_element())
                                .map(|n| {
                                    (
                                        n.tag_name().namespace().unwrap_or("").to_string(),
                                        n.tag_name().name().to_string(),
                                        n.attribute("name").map(str::to_string),
                                    )
                                })
                                .collect(),
                        });
                    }
                }
            }
        }
        out.push(r);
    }
    Ok(out)
}

pub fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// Ensure a collection URL ends with a slash so relative joins work.
fn as_collection(mut u: Url) -> Url {
    if !u.path().ends_with('/') {
        let p = format!("{}/", u.path());
        u.set_path(&p);
    }
    u
}

impl CalDavClient {
    pub fn new(cfg: Config) -> Result<Self> {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("caldav_mcp/", env!("CARGO_PKG_VERSION")))
            .timeout(std::time::Duration::from_secs(60))
            .build()?;
        Ok(Self { http, cfg, home: Mutex::new(None) })
    }

    pub fn resolve(&self, href: &str) -> Result<Url> {
        self.cfg.base_url.join(href).with_context(|| format!("invalid URL/href '{href}'"))
    }

    /// Send a request, following redirects manually so that WebDAV methods and bodies are preserved.
    pub async fn request(
        &self,
        method: Method,
        url: Url,
        headers: &[(&str, String)],
        body: Option<(String, &str)>,
    ) -> Result<HttpResponse> {
        let mut url = url;
        let mut send_auth = true;
        for _ in 0..6 {
            let mut req = self.http.request(method.clone(), url.clone());
            if send_auth {
                req = match &self.cfg.auth {
                    Auth::None => req,
                    Auth::Basic { user, pass } => req.basic_auth(user, Some(pass)),
                    Auth::Bearer(t) => req.bearer_auth(t),
                };
            }
            for (k, v) in headers {
                req = req.header(*k, v);
            }
            if let Some((b, ct)) = &body {
                req = req.header(header::CONTENT_TYPE, *ct).body(b.clone());
            }
            let resp = req.send().await.with_context(|| format!("{method} {url} failed"))?;
            let status = resp.status();
            if status.is_redirection()
                && let Some(loc) = resp.headers().get(header::LOCATION).and_then(|v| v.to_str().ok()) {
                    let next = url.join(loc)?;
                    // Never send credentials over a downgraded (https -> http) connection.
                    if url.scheme() == "https" && next.scheme() != "https" {
                        send_auth = false;
                    }
                    url = next;
                    continue;
                }
            let headers = resp.headers().clone();
            let body = resp.text().await.unwrap_or_default();
            return Ok(HttpResponse { status, headers, body });
        }
        bail!("too many redirects for {method} {url}")
    }

    async fn dav(&self, method: &str, url: Url, depth: &str, body: &str) -> Result<Vec<DavResponse>> {
        let m = Method::from_bytes(method.as_bytes())?;
        let resp = self
            .request(m, url.clone(), &[("Depth", depth.to_string())], Some((body.to_string(), "application/xml; charset=utf-8")))
            .await?;
        if resp.status != StatusCode::MULTI_STATUS && !resp.status.is_success() {
            bail!("{method} {url} returned {}: {}", resp.status, truncate(&resp.body, 500));
        }
        parse_multistatus(&resp.body)
    }

    async fn propfind(&self, url: Url, depth: &str, props: &str) -> Result<Vec<DavResponse>> {
        let body = format!(
            r#"<?xml version="1.0" encoding="utf-8"?><d:propfind xmlns:d="{NS_DAV}" xmlns:c="{NS_CALDAV}" xmlns:cs="{NS_CS}" xmlns:ic="{NS_ICAL}"><d:prop>{props}</d:prop></d:propfind>"#
        );
        self.dav("PROPFIND", url, depth, &body).await
    }

    /// Find the calendar home collection (cached).
    async fn calendar_home(&self) -> Result<Url> {
        let mut guard = self.home.lock().await;
        if let Some(h) = guard.as_ref() {
            return Ok(h.clone());
        }
        let home = self.discover_home().await?;
        *guard = Some(home.clone());
        Ok(home)
    }

    async fn discover_home(&self) -> Result<Url> {
        let base = self.cfg.base_url.clone();
        let mut candidates = vec![base.clone()];
        if let Ok(wk) = base.join("/.well-known/caldav") {
            candidates.push(wk);
        }
        for start in candidates {
            let Ok(resps) = self.propfind(start.clone(), "0", "<d:current-user-principal/><c:calendar-home-set/>").await else {
                continue;
            };
            // The URL might already be a principal (or expose the home set directly).
            if let Some(h) = resps.iter().find_map(|r| r.prop(NS_CALDAV, "calendar-home-set")).and_then(|p| p.hrefs.first()) {
                return Ok(as_collection(start.join(h)?));
            }
            let Some(principal) = resps
                .iter()
                .find_map(|r| r.prop(NS_DAV, "current-user-principal"))
                .and_then(|p| p.hrefs.first())
            else {
                continue;
            };
            let principal = start.join(principal)?;
            let resps = self.propfind(principal.clone(), "0", "<c:calendar-home-set/>").await?;
            if let Some(h) = resps.iter().find_map(|r| r.prop(NS_CALDAV, "calendar-home-set")).and_then(|p| p.hrefs.first()) {
                return Ok(as_collection(principal.join(h)?));
            }
        }
        // Fall back to treating the configured URL as the calendar home itself.
        Ok(as_collection(base))
    }

    pub async fn list_calendars(&self) -> Result<Vec<Calendar>> {
        let home = self.calendar_home().await?;
        let resps = self
            .propfind(
                home.clone(),
                "1",
                "<d:resourcetype/><d:displayname/><c:calendar-description/><c:supported-calendar-component-set/><cs:getctag/><ic:calendar-color/>",
            )
            .await?;
        let mut cals = Vec::new();
        for r in resps {
            let is_calendar = r
                .prop(NS_DAV, "resourcetype")
                .is_some_and(|p| p.children.iter().any(|(ns, n, _)| ns == NS_CALDAV && n == "calendar"));
            if !is_calendar {
                continue;
            }
            let url = as_collection(home.join(&r.href)?);
            let name = r
                .prop(NS_DAV, "displayname")
                .map(|p| p.text.clone())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| url.path_segments().and_then(|mut s| s.rfind(|x| !x.is_empty())).unwrap_or("").to_string());
            let nonempty = |p: Option<&DavProp>| p.map(|p| p.text.clone()).filter(|s| !s.is_empty());
            cals.push(Calendar {
                url: url.to_string(),
                name,
                description: nonempty(r.prop(NS_CALDAV, "calendar-description")),
                color: nonempty(r.prop(NS_ICAL, "calendar-color")),
                components: r
                    .prop(NS_CALDAV, "supported-calendar-component-set")
                    .map(|p| p.children.iter().filter_map(|(_, _, n)| n.clone()).collect())
                    .unwrap_or_default(),
                ctag: nonempty(r.prop(NS_CS, "getctag")),
            });
        }
        Ok(cals)
    }

    /// Resolve a calendar reference: a URL/path, a display name, or None for the default.
    pub async fn resolve_calendar(&self, reference: Option<&str>, default: Option<&str>) -> Result<Url> {
        let reference = reference.filter(|s| !s.trim().is_empty()).or(default);
        if let Some(r) = reference
            && (r.starts_with("http://") || r.starts_with("https://") || r.starts_with('/')) {
                return Ok(as_collection(self.resolve(r)?));
            }
        let cals = self.list_calendars().await?;
        match reference {
            Some(r) => cals
                .iter()
                .find(|c| c.name.eq_ignore_ascii_case(r))
                .or_else(|| cals.iter().find(|c| c.url.trim_end_matches('/').ends_with(&format!("/{}", r.trim_matches('/')))))
                .map(|c| Url::parse(&c.url).unwrap())
                .ok_or_else(|| {
                    let names: Vec<_> = cals.iter().map(|c| c.name.as_str()).collect();
                    anyhow!("no calendar named '{r}'. Available: {}", names.join(", "))
                }),
            None => cals
                .iter()
                .find(|c| c.supports_events())
                .map(|c| Url::parse(&c.url).unwrap())
                .ok_or_else(|| anyhow!("no calendar supporting events was found")),
        }
    }

    /// Run a calendar-query REPORT with the given VEVENT filter body.
    async fn calendar_query(&self, calendar: Url, event_filter: &str) -> Result<Vec<CalendarObject>> {
        let data = "<c:calendar-data/>";
        let body = format!(
            r#"<?xml version="1.0" encoding="utf-8"?><c:calendar-query xmlns:d="{NS_DAV}" xmlns:c="{NS_CALDAV}"><d:prop><d:getetag/>{data}</d:prop><c:filter><c:comp-filter name="VCALENDAR"><c:comp-filter name="VEVENT">{event_filter}</c:comp-filter></c:comp-filter></c:filter></c:calendar-query>"#
        );
        let resps = self.dav("REPORT", calendar.clone(), "1", &body).await?;
        let mut out = Vec::new();
        for r in resps {
            let Some(data) = r.prop(NS_CALDAV, "calendar-data") else { continue };
            out.push(CalendarObject {
                url: calendar.join(&r.href)?,
                etag: r.prop(NS_DAV, "getetag").map(|p| p.text.clone()),
                data: data.text.clone(),
            });
        }
        Ok(out)
    }

    /// Calendar objects with events overlapping the UTC range (YYYYMMDDTHHMMSSZ).
    /// Recurring events are returned unexpanded (see `expand.rs`).
    pub async fn events_in_range(&self, calendar: Url, start: &str, end: &str) -> Result<Vec<CalendarObject>> {
        let filter = format!(r#"<c:time-range start="{start}" end="{end}"/>"#);
        self.calendar_query(calendar, &filter).await
    }

    pub async fn find_by_uid(&self, calendar: Url, uid: &str) -> Result<Option<CalendarObject>> {
        let filter = format!(
            r#"<c:prop-filter name="UID"><c:text-match collation="i;octet">{}</c:text-match></c:prop-filter>"#,
            xml_escape(uid)
        );
        Ok(self.calendar_query(calendar, &filter).await?.into_iter().next())
    }

    pub async fn get(&self, url: Url) -> Result<CalendarObject> {
        let resp = self.request(Method::GET, url.clone(), &[("Accept", "text/calendar".into())], None).await?;
        if !resp.status.is_success() {
            bail!("GET {url} returned {}: {}", resp.status, truncate(&resp.body, 300));
        }
        Ok(CalendarObject { etag: etag_header(&resp.headers), url, data: resp.body })
    }

    /// PUT a calendar object with the given precondition.
    pub async fn put(&self, url: Url, data: String, pre: Precondition<'_>) -> Result<Option<String>> {
        let headers: Vec<(&str, String)> = match pre {
            Precondition::MustNotExist => vec![("If-None-Match", "*".to_string())],
            Precondition::Matches(e) => vec![("If-Match", e.to_string())],
            Precondition::None => vec![],
        };
        let resp = self.request(Method::PUT, url.clone(), &headers, Some((data, "text/calendar; charset=utf-8"))).await?;
        if resp.status == StatusCode::PRECONDITION_FAILED {
            bail!(
                "PUT {url}: precondition failed ({}). Fetch the event again and retry.",
                match pre {
                    Precondition::MustNotExist => "an event already exists at this URL",
                    _ => "the event was modified by someone else (etag mismatch)",
                }
            );
        }
        if !resp.status.is_success() {
            bail!("PUT {url} returned {}: {}", resp.status, truncate(&resp.body, 500));
        }
        Ok(etag_header(&resp.headers))
    }

    pub async fn delete(&self, url: Url, if_match: Option<&str>) -> Result<()> {
        let headers: Vec<(&str, String)> = if_match.map(|e| vec![("If-Match", e.to_string())]).unwrap_or_default();
        let resp = self.request(Method::DELETE, url.clone(), &headers, None).await?;
        if !resp.status.is_success() {
            bail!("DELETE {url} returned {}: {}", resp.status, truncate(&resp.body, 300));
        }
        Ok(())
    }
}

fn etag_header(h: &header::HeaderMap) -> Option<String> {
    h.get(header::ETAG).and_then(|v| v.to_str().ok()).map(str::to_string)
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { format!("{}…", s.chars().take(n).collect::<String>()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multistatus() {
        let xml = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:cal="urn:ietf:params:xml:ns:caldav">
 <d:response><d:href>/cal/home/</d:href>
  <d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
 </d:response>
 <d:response><d:href>/cal/home/work/</d:href>
  <d:propstat><d:prop><d:resourcetype><d:collection/><cal:calendar/></d:resourcetype><d:displayname>Work</d:displayname>
   <cal:supported-calendar-component-set><cal:comp name="VEVENT"/><cal:comp name="VTODO"/></cal:supported-calendar-component-set></d:prop>
   <d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  <d:propstat><d:prop><cal:calendar-description/></d:prop><d:status>HTTP/1.1 404 Not Found</d:status></d:propstat>
 </d:response>
</d:multistatus>"#;
        let r = parse_multistatus(xml).unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r[1].href, "/cal/home/work/");
        assert_eq!(r[1].prop(NS_DAV, "displayname").unwrap().text, "Work");
        assert!(r[1].prop(NS_CALDAV, "calendar-description").is_none());
        let comps: Vec<_> = r[1].prop(NS_CALDAV, "supported-calendar-component-set").unwrap().children.iter().filter_map(|c| c.2.clone()).collect();
        assert_eq!(comps, ["VEVENT", "VTODO"]);
    }
}

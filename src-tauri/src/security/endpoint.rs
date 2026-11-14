//! Endpoint policy (spec §6.2).
//!
//! * Remote endpoints must use HTTPS. HTTP is allowed only for loopback, or for private
//!   networks the user explicitly registered (`allow_private_http`).
//! * userinfo, fragments, queries and non-http(s) schemes are rejected; host is lowercased
//!   and IDNA-encoded (url crate), default ports removed, path normalized.
//! * Link-local (incl. cloud metadata 169.254.169.254), unspecified, multicast and broadcast
//!   addresses are always rejected.
//! * The connect-time check ([`PolicyResolver`]) re-validates the addresses a host name
//!   actually resolves to, defeating DNS rebinding (e.g. `localhost` -> public IP, or a
//!   public HTTPS name -> 127.0.0.1).

use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

use url::{Host, Url};

use crate::error::{AppError, AppResult};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedEndpoint {
    pub url: Url,
    /// Stable identity used in cache keys, e.g. `http://127.0.0.1:11434` or `https://host/v1`.
    pub identity_string: String,
    pub is_loopback: bool,
    pub is_private: bool,
    pub is_https: bool,
    /// The host is a DNS name (not an IP literal); resolution is checked at connect time.
    pub is_domain: bool,
}

impl NormalizedEndpoint {
    /// Base URL joined with a relative API path (`"chat/completions"`, `"api/tags"`).
    pub fn join(&self, rel: &str) -> Url {
        let mut u = self.url.clone();
        let base = u.path().trim_end_matches('/').to_string();
        u.set_path(&format!("{}/{}", base, rel.trim_start_matches('/')));
        u
    }

    /// The address policy applied to resolved addresses at connect time.
    pub fn resolve_policy(&self) -> ResolvePolicy {
        if self.is_loopback {
            ResolvePolicy::LoopbackOnly
        } else if !self.is_https {
            // HTTP to a registered private-network name: private or loopback only.
            ResolvePolicy::PrivateOrLoopback
        } else if self.is_private {
            // HTTPS to a registered private endpoint.
            ResolvePolicy::Any
        } else {
            ResolvePolicy::PublicOnly
        }
    }
}

fn reject(msg: &str) -> AppError {
    AppError::invalid(msg.to_string())
}

/// Normalize and validate a user-entered endpoint.
pub fn normalize_endpoint(raw: &str, allow_private_http: bool) -> AppResult<NormalizedEndpoint> {
    let raw = raw.trim();
    if raw.is_empty() || raw.len() > 2048 {
        return Err(reject("서버 주소를 입력해 주세요."));
    }
    let mut url = Url::parse(raw).map_err(|_| reject("서버 주소 형식이 올바르지 않습니다."))?;
    let is_https = match url.scheme() {
        "https" => true,
        "http" => false,
        _ => return Err(reject("http 또는 https 주소만 사용할 수 있습니다.")),
    };
    if !url.username().is_empty() || url.password().is_some() {
        return Err(reject("주소에 사용자 정보(아이디·비밀번호)를 넣을 수 없습니다. API 키는 별도로 입력해 주세요."));
    }
    if url.fragment().is_some() {
        return Err(reject("주소에 # 조각을 넣을 수 없습니다."));
    }
    if url.query().is_some() {
        return Err(reject("주소에 쿼리 문자열(?)을 넣을 수 없습니다."));
    }

    let (is_loopback, is_private, is_domain) = match url.host() {
        None => return Err(reject("서버 주소에 호스트가 없습니다.")),
        Some(Host::Domain(d)) => {
            let d = d.trim_end_matches('.').to_ascii_lowercase();
            if d.is_empty() {
                return Err(reject("서버 주소에 호스트가 없습니다."));
            }
            let lb = d == "localhost" || d.ends_with(".localhost");
            if url.host_str() != Some(d.as_str()) {
                url.set_host(Some(&d)).map_err(|_| reject("서버 주소 형식이 올바르지 않습니다."))?;
            }
            // A non-loopback name over HTTP is only allowed as a registered private endpoint;
            // HTTPS names flagged private may resolve to private addresses.
            (lb, !lb && allow_private_http, true)
        }
        Some(Host::Ipv4(ip)) => {
            let ip = IpAddr::V4(ip);
            check_never_allowed(ip)?;
            (is_loopback_ip(ip), is_private_ip(ip), false)
        }
        Some(Host::Ipv6(ip)) => {
            let ip = IpAddr::V6(ip);
            check_never_allowed(ip)?;
            (is_loopback_ip(ip), is_private_ip(ip), false)
        }
    };

    if !is_https && !is_loopback && !(is_private && allow_private_http) {
        return Err(reject(
            "원격 서버는 https 주소만 사용할 수 있습니다. 사설망 서버는 '사설망 http 허용'을 켜야 합니다.",
        ));
    }
    if is_https && !is_domain && is_private && !is_loopback && !allow_private_http {
        return Err(reject("사설망 주소는 '사설망 허용'을 켠 경우에만 사용할 수 있습니다."));
    }

    // Path: collapse repeated slashes, strip trailing slash. (url already resolved dot segments.)
    let mut path = String::with_capacity(url.path().len());
    for seg in url.path().split('/').filter(|s| !s.is_empty()) {
        path.push('/');
        path.push_str(seg);
    }
    url.set_path(if path.is_empty() { "/" } else { &path });
    // Re-apply default port removal explicitly (url does it on parse already).
    if url.port() == default_port(url.scheme()) {
        let _ = url.set_port(None);
    }

    let mut identity = format!("{}://{}", url.scheme(), url.host_str().unwrap_or(""));
    if let Some(p) = url.port() {
        identity.push_str(&format!(":{p}"));
    }
    identity.push_str(&path);

    Ok(NormalizedEndpoint {
        url,
        identity_string: identity,
        is_loopback,
        is_private: is_private && !is_loopback,
        is_https,
        is_domain,
    })
}

fn default_port(scheme: &str) -> Option<u16> {
    match scheme {
        "http" => Some(80),
        "https" => Some(443),
        _ => None,
    }
}

fn check_never_allowed(ip: IpAddr) -> AppResult<()> {
    if is_forbidden_ip(ip) {
        return Err(reject("이 주소(링크 로컬·메타데이터·미지정 주소)는 사용할 수 없습니다."));
    }
    Ok(())
}

fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => IpAddr::V6(v6),
        },
        v4 => v4,
    }
}

pub fn is_loopback_ip(ip: IpAddr) -> bool {
    match canonical(ip) {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => v6 == Ipv6Addr::LOCALHOST,
    }
}

/// 10/8, 172.16/12, 192.168/16, fc00::/7.
pub fn is_private_ip(ip: IpAddr) -> bool {
    match canonical(ip) {
        IpAddr::V4(v4) => v4.is_private(),
        IpAddr::V6(v6) => (v6.segments()[0] & 0xfe00) == 0xfc00,
    }
}

/// Link-local (169.254/16 incl. metadata, fe80::/10), unspecified, multicast, broadcast.
pub fn is_forbidden_ip(ip: IpAddr) -> bool {
    match canonical(ip) {
        IpAddr::V4(v4) => {
            v4.is_link_local() || v4.is_unspecified() || v4.is_multicast() || v4.is_broadcast() || v4.octets()[0] == 0
        }
        IpAddr::V6(v6) => {
            v6.is_unspecified() || v6.is_multicast() || (v6.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

/// Which resolved addresses are acceptable for an endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvePolicy {
    /// HTTP/HTTPS to `localhost`: every address must be loopback.
    LoopbackOnly,
    /// HTTP to a registered private-network name.
    PrivateOrLoopback,
    /// HTTPS to a public name: loopback/private results are DNS rebinding and rejected.
    PublicOnly,
    /// HTTPS to a name the user registered as private.
    Any,
}

/// Error placed in the reqwest error chain when the policy rejects a resolution.
#[derive(Debug, Clone, thiserror::Error)]
#[error("resolved address rejected by endpoint policy")]
pub struct ResolvePolicyError;

/// Validate the full set of resolved addresses. Any violating address rejects the whole
/// resolution (a mixed answer is treated as an attack, not filtered).
pub fn check_resolved(policy: ResolvePolicy, addrs: &[IpAddr]) -> Result<(), ResolvePolicyError> {
    if addrs.is_empty() {
        return Err(ResolvePolicyError);
    }
    for &ip in addrs {
        if is_forbidden_ip(ip) {
            return Err(ResolvePolicyError);
        }
        let ok = match policy {
            ResolvePolicy::LoopbackOnly => is_loopback_ip(ip),
            ResolvePolicy::PrivateOrLoopback => is_loopback_ip(ip) || is_private_ip(ip),
            ResolvePolicy::PublicOnly => !is_loopback_ip(ip) && !is_private_ip(ip),
            ResolvePolicy::Any => true,
        };
        if !ok {
            return Err(ResolvePolicyError);
        }
    }
    Ok(())
}

/// DNS resolver enforcing [`ResolvePolicy`] at connect time. Used for every provider client.
/// IP-literal hosts bypass DNS in hyper and are validated by [`normalize_endpoint`].
#[derive(Clone)]
pub struct PolicyResolver {
    policy: ResolvePolicy,
    lookup: Arc<dyn Fn(String) -> futures_util::future::BoxFuture<'static, std::io::Result<Vec<SocketAddr>>> + Send + Sync>,
}

impl std::fmt::Debug for PolicyResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PolicyResolver").field("policy", &self.policy).finish()
    }
}

impl PolicyResolver {
    pub fn new(policy: ResolvePolicy) -> Self {
        Self {
            policy,
            lookup: Arc::new(|host: String| {
                Box::pin(async move { Ok(tokio::net::lookup_host((host.as_str(), 0)).await?.collect()) })
            }),
        }
    }

    /// Test hook: replace the system lookup.
    pub fn with_lookup<F>(policy: ResolvePolicy, f: F) -> Self
    where
        F: Fn(String) -> futures_util::future::BoxFuture<'static, std::io::Result<Vec<SocketAddr>>> + Send + Sync + 'static,
    {
        Self { policy, lookup: Arc::new(f) }
    }

    pub async fn resolve_checked(&self, host: &str) -> Result<Vec<SocketAddr>, Box<dyn std::error::Error + Send + Sync>> {
        let addrs = (self.lookup)(host.to_string()).await?;
        let ips: Vec<IpAddr> = addrs.iter().map(|a| a.ip()).collect();
        if let Err(e) = check_resolved(self.policy, &ips) {
            tracing::warn!(target: "provider", "endpoint policy rejected a DNS resolution (policy={:?})", self.policy);
            return Err(Box::new(e));
        }
        Ok(addrs)
    }
}

impl reqwest::dns::Resolve for PolicyResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let this = self.clone();
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addrs = this.resolve_checked(&host).await?;
            let it: reqwest::dns::Addrs = Box::new(addrs.into_iter());
            Ok(it)
        })
    }
}

/// True when `e` (or a source) is a [`ResolvePolicyError`].
pub fn is_policy_error(e: &(dyn std::error::Error + 'static)) -> bool {
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(err) = cur {
        if err.downcast_ref::<ResolvePolicyError>().is_some() {
            return true;
        }
        cur = err.source();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(raw: &str, allow: bool) -> NormalizedEndpoint {
        normalize_endpoint(raw, allow).unwrap_or_else(|e| panic!("{raw} should be accepted: {e:?}"))
    }
    fn bad(raw: &str, allow: bool) {
        assert!(normalize_endpoint(raw, allow).is_err(), "{raw} (allow={allow}) should be rejected");
    }

    #[test]
    fn normalization_table() {
        // (input, allow_private, expected identity)
        let cases: &[(&str, bool, &str)] = &[
            ("http://127.0.0.1:11434", false, "http://127.0.0.1:11434"),
            ("http://127.0.0.1:11434/", false, "http://127.0.0.1:11434"),
            ("http://localhost:11434//api/", false, "http://localhost:11434/api"),
            ("http://LOCALHOST:8080", false, "http://localhost:8080"),
            ("http://[::1]:8080/", false, "http://[::1]:8080"),
            ("http://127.8.9.10", false, "http://127.8.9.10"),
            ("https://API.Example.COM/v1/", false, "https://api.example.com/v1"),
            ("https://api.example.com:443/v1", false, "https://api.example.com/v1"),
            ("http://localhost:80/x", false, "http://localhost/x"),
            ("https://api.example.com:8443/v1//", false, "https://api.example.com:8443/v1"),
            ("https://api.example.com/a/./b/../v1", false, "https://api.example.com/a/v1"),
            ("http://10.0.0.5:11434", true, "http://10.0.0.5:11434"),
            ("http://192.168.1.20:1234/v1", true, "http://192.168.1.20:1234/v1"),
            ("http://172.16.3.4", true, "http://172.16.3.4"),
            ("http://[fd00::1]:8000", true, "http://[fd00::1]:8000"),
            ("http://nas.lan:11434", true, "http://nas.lan:11434"),
            ("https://bücher.example/v1", false, "https://xn--bcher-kva.example/v1"),
            ("https://localhost.:8443", false, "https://localhost:8443"),
        ];
        for (raw, allow, id) in cases {
            assert_eq!(ok(raw, *allow).identity_string, *id, "{raw}");
        }
        let lb = ok("http://localhost:11434", false);
        assert!(lb.is_loopback && !lb.is_private && lb.is_domain);
        assert_eq!(lb.resolve_policy(), ResolvePolicy::LoopbackOnly);
        let p = ok("http://10.1.2.3", true);
        assert!(p.is_private && !p.is_loopback);
        assert_eq!(ok("https://api.example.com/v1", false).resolve_policy(), ResolvePolicy::PublicOnly);
        assert_eq!(ok("http://nas.lan", true).resolve_policy(), ResolvePolicy::PrivateOrLoopback);
        assert_eq!(
            ok("https://api.example.com/v1", false).join("chat/completions").as_str(),
            "https://api.example.com/v1/chat/completions"
        );
        assert_eq!(ok("http://127.0.0.1:11434", false).join("api/chat").as_str(), "http://127.0.0.1:11434/api/chat");
    }

    #[test]
    fn rejections() {
        bad("http://api.example.com/v1", false); // remote http
        bad("http://8.8.8.8", true); // public IP over http even with allow
        bad("http://10.0.0.5:11434", false); // private requires allow
        bad("http://192.168.0.2", false);
        bad("https://10.0.0.5", false);
        bad("https://user:pw@api.example.com", false);
        bad("https://user@api.example.com", false);
        bad("https://api.example.com/v1#frag", false);
        bad("https://api.example.com/v1?key=1", false);
        bad("https://api.example.com/v1?", false);
        bad("ftp://api.example.com", false);
        bad("file:///etc/passwd", false);
        bad("javascript:alert(1)", false);
        bad("http://169.254.169.254/latest", true);
        bad("https://169.254.169.254", true);
        bad("http://[fe80::1]", true);
        bad("http://0.0.0.0:11434", true);
        bad("https://[::]", true);
        bad("http://[::ffff:169.254.169.254]", true);
        bad("", false);
        bad("not a url", false);
    }

    #[test]
    fn http_public_name_with_allow_is_private_name() {
        // A non-loopback name over http with allow flag is accepted at parse time; the
        // connect-time resolver then requires private/loopback addresses.
        let e = ok("http://api.example.com/v1", true);
        assert_eq!(e.resolve_policy(), ResolvePolicy::PrivateOrLoopback);
    }

    #[test]
    fn resolved_address_checks() {
        let lo: IpAddr = "127.0.0.1".parse().unwrap();
        let lo6: IpAddr = "::1".parse().unwrap();
        let pubip: IpAddr = "93.184.216.34".parse().unwrap();
        let priv_ip: IpAddr = "10.0.0.7".parse().unwrap();
        let meta: IpAddr = "169.254.169.254".parse().unwrap();
        assert!(check_resolved(ResolvePolicy::LoopbackOnly, &[lo, lo6]).is_ok());
        assert!(check_resolved(ResolvePolicy::LoopbackOnly, &[lo, pubip]).is_err()); // rebinding
        assert!(check_resolved(ResolvePolicy::LoopbackOnly, &[]).is_err());
        assert!(check_resolved(ResolvePolicy::PublicOnly, &[pubip]).is_ok());
        assert!(check_resolved(ResolvePolicy::PublicOnly, &[lo]).is_err());
        assert!(check_resolved(ResolvePolicy::PublicOnly, &[priv_ip]).is_err());
        assert!(check_resolved(ResolvePolicy::PrivateOrLoopback, &[priv_ip, lo]).is_ok());
        assert!(check_resolved(ResolvePolicy::PrivateOrLoopback, &[pubip]).is_err());
        assert!(check_resolved(ResolvePolicy::Any, &[meta]).is_err());
        assert!(check_resolved(ResolvePolicy::Any, &[pubip, priv_ip]).is_ok());
    }

    #[tokio::test]
    async fn resolver_rejects_rebinding() {
        let r = PolicyResolver::with_lookup(ResolvePolicy::LoopbackOnly, |_h| {
            Box::pin(async { Ok(vec!["203.0.113.9:0".parse().unwrap()]) })
        });
        let err = r.resolve_checked("localhost").await.unwrap_err();
        assert!(is_policy_error(err.as_ref()));
        // The real system resolver maps localhost to loopback.
        let r = PolicyResolver::new(ResolvePolicy::LoopbackOnly);
        let addrs = r.resolve_checked("localhost").await.unwrap();
        assert!(addrs.iter().all(|a| a.ip().is_loopback()));
    }

    #[tokio::test]
    async fn reqwest_client_with_rebinding_resolver_fails_before_sending() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        // Name resolves to loopback, but the endpoint is a public HTTPS name -> reject.
        let r = PolicyResolver::with_lookup(ResolvePolicy::PublicOnly, move |_h| {
            Box::pin(async move { Ok(vec![SocketAddr::from(([127, 0, 0, 1], port))]) })
        });
        let client = reqwest::Client::builder().no_proxy().dns_resolver(r).build().unwrap();
        let err = client.get(format!("http://rebind.example:{port}/")).send().await.unwrap_err();
        assert!(err.is_connect());
        assert!(is_policy_error(&err));
    }
}

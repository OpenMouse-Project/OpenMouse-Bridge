use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail, ensure};
use reqwest::{Url, redirect::Policy};
use tokio::sync::Semaphore;

const FETCH_TIMEOUT: Duration = Duration::from_secs(10);
/// A wallpaper can be a few MB; the page scales it down before it is saved.
const MAX_DOWNLOAD_BYTES: usize = 12 * 1024 * 1024;
const MAX_REDIRECTS: usize = 4;
const UNREACHABLE: &str = "that link cannot be reached";
/// Each download can hold 12 MB, so only a couple run at once.
static DOWNLOADS: Semaphore = Semaphore::const_new(2);

/// Whether `ip` is an address on the public internet. Loopback, private, link-local,
/// shared (carrier-grade NAT), multicast and unspecified addresses are not.
pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_public_v4(ip),
        IpAddr::V6(ip) => is_public_v6(ip),
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_multicast()
        || a == 0
        || a >= 240 // reserved
        || (a == 100 && (64..128).contains(&b)) // 100.64.0.0/10
        || (a == 192 && b == 0 && c == 0) // 192.0.0.0/24
        || (a == 198 && (18..20).contains(&b)) // 198.18.0.0/15, benchmarking
        || matches!((a, b, c), (192, 0, 2) | (198, 51, 100) | (203, 0, 113))) // documentation
}

fn v4_from(high: u16, low: u16) -> Ipv4Addr {
    Ipv4Addr::new((high >> 8) as u8, high as u8, (low >> 8) as u8, low as u8)
}

/// IPv6 forms that carry an IPv4 address are judged by that address, so a private one cannot hide inside.
fn is_public_v6(ip: Ipv6Addr) -> bool {
    let segments = ip.segments();
    let first = segments[0];
    let embedded = v4_from(segments[6], segments[7]);
    match segments[..6] {
        [0, 0, 0, 0, 0, 0] | [0, 0, 0, 0, 0, 0xffff] | [0x64, 0xff9b, 0, 0, 0, 0] => {
            return is_public_v4(embedded);
        }
        _ => {}
    }
    if first == 0x2002 {
        return is_public_v4(v4_from(segments[1], segments[2])); // 6to4
    }
    !(ip.is_loopback()
        || ip.is_multicast()
        || first & 0xfe00 == 0xfc00 // unique local
        || first & 0xffc0 == 0xfe80 // link-local
        || (first == 0x2001 && segments[1] == 0) // Teredo
        || (first == 0x2001 && segments[1] == 0x0db8)) // documentation
}

pub const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];

pub fn image_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&PNG_SIGNATURE) {
        Some("image/png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() > 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// The public addresses `url`'s host resolves to. The download connects only to these,
/// so the host cannot answer differently the second time.
async fn check(url: &Url) -> Result<Vec<SocketAddr>> {
    ensure!(
        matches!(url.scheme(), "http" | "https"),
        "only http and https links work"
    );
    let host = url.host_str().context("the link has no host")?;
    let port = url.port_or_known_default().unwrap_or(443);
    // One message for "not found" and "not public", so a page cannot tell which names exist on the local network.
    let addresses: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|_| anyhow!(UNREACHABLE))?
        .collect();
    ensure!(
        !addresses.is_empty() && addresses.iter().all(|address| is_public_ip(address.ip())),
        UNREACHABLE
    );
    Ok(addresses)
}

/// The whole download, however many redirects and name lookups it takes. A name
/// lookup has no time limit of its own, and the download's slot is held until it ends.
const TOTAL_TIMEOUT: Duration = Duration::from_secs(30);

pub async fn fetch(link: &str) -> Result<(Vec<u8>, &'static str)> {
    let _download = DOWNLOADS
        .try_acquire()
        .map_err(|_| anyhow!("too many downloads at once, try again in a moment"))?;
    tokio::time::timeout(TOTAL_TIMEOUT, download(link))
        .await
        .map_err(|_| anyhow!("the download took too long"))?
}

async fn download(link: &str) -> Result<(Vec<u8>, &'static str)> {
    let mut url = Url::parse(link).context("that is not a link")?;
    for _ in 0..=MAX_REDIRECTS {
        let addresses = check(&url).await?;
        let client = reqwest::Client::builder()
            // A proxy would resolve the name itself, past the addresses checked above.
            .no_proxy()
            .timeout(FETCH_TIMEOUT)
            .user_agent(concat!("OpenMouse-Bridge/", env!("CARGO_PKG_VERSION")))
            .redirect(Policy::none())
            .resolve_to_addrs(url.host_str().unwrap_or_default(), &addresses)
            .build()
            .context("could not create the download client")?;
        let mut response = client
            .get(url.clone())
            .send()
            .await
            .with_context(|| format!("could not download {url}"))?;
        if response.status().is_redirection() {
            let next = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .context("the link redirected nowhere")?;
            url = url
                .join(next)
                .context("the link redirected to something that is not a link")?;
            continue;
        }
        ensure!(
            response.status().is_success(),
            "the site answered {}",
            response.status()
        );
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .context("the download was interrupted")?
        {
            bytes.extend_from_slice(&chunk);
            ensure!(bytes.len() <= MAX_DOWNLOAD_BYTES, "that image is too big");
        }
        let kind = image_type(&bytes).context("that link is not a PNG, JPEG, GIF or WebP image")?;
        return Ok((bytes, kind));
    }
    bail!("the link redirects too many times")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn public(ip: &str) -> bool {
        is_public_ip(ip.parse().expect("an address"))
    }

    #[test]
    fn addresses_on_this_pc_or_the_local_network_are_refused() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "172.31.255.254",
            "192.168.1.1",
            "169.254.169.254",
            "0.0.0.0",
            "100.64.0.1",
            "224.0.0.1",
            "255.255.255.255",
            "::1",
            "::",
            "fc00::1",
            "fd12::1",
            "fe80::1",
            "ff02::1",
            "::ffff:192.168.0.5",
            "::ffff:127.0.0.1",
            "64:ff9b::c0a8:1",
            "64:ff9b::7f00:1",
            "2002:c0a8:101::1",
            "2002:7f00:1::",
            "::192.168.0.5",
            "2001::1",
            "2001:db8::1",
            "198.18.0.1",
            "198.19.255.1",
            "192.0.0.8",
            "192.0.2.1",
            "198.51.100.1",
            "203.0.113.1",
            "240.0.0.1",
            "250.1.1.1",
        ] {
            assert!(!public(ip), "{ip} should not be fetched");
        }
    }

    #[test]
    fn addresses_on_the_public_internet_are_allowed() {
        for ip in [
            "1.1.1.1",
            "8.8.8.8",
            "93.184.216.34",
            "172.32.0.1",
            "100.63.0.1",
            "2606:4700:4700::1111",
            "::ffff:8.8.8.8",
            "64:ff9b::808:808",
            "2002:808:808::1",
            "198.17.255.1",
            "198.20.0.1",
        ] {
            assert!(public(ip), "{ip} should be fetched");
        }
    }

    #[test]
    fn only_real_images_are_recognised() {
        assert_eq!(
            image_type(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0]),
            Some("image/png")
        );
        assert_eq!(image_type(&[0xff, 0xd8, 0xff, 0xe0]), Some("image/jpeg"));
        assert_eq!(image_type(b"GIF89a......"), Some("image/gif"));
        assert_eq!(image_type(b"RIFF\0\0\0\0WEBPVP8 ...."), Some("image/webp"));
        assert_eq!(image_type(b"<html><body>not an image</body></html>"), None);
    }

    #[tokio::test]
    async fn a_link_to_this_pc_is_refused_before_anything_is_fetched() {
        let error = fetch("http://127.0.0.1:17846/v1/status")
            .await
            .expect_err("refused");
        assert_eq!(error.to_string(), UNREACHABLE);
        let error = fetch("file:///C:/Windows/win.ini")
            .await
            .expect_err("refused");
        assert!(error.to_string().contains("only http and https"), "{error}");
        assert!(fetch("not a link").await.is_err());
    }
}

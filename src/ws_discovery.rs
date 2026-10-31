use regex::Regex;
use std::{
    collections::BTreeSet,
    io::{Read, Write},
    net::{Ipv4Addr, SocketAddrV4, TcpStream, UdpSocket},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

fn scan_local_subnet() -> BTreeSet<String> {
    let Ok(route) = UdpSocket::bind("0.0.0.0:0") else {
        return BTreeSet::new();
    };
    if route.connect("1.1.1.1:80").is_err() {
        return BTreeSet::new();
    }
    let Ok(std::net::IpAddr::V4(local)) = route.local_addr().map(|a| a.ip()) else {
        return BTreeSet::new();
    };
    if !local.is_private() {
        return BTreeSet::new();
    }
    let octets = local.octets();
    let queue = Arc::new(Mutex::new(
        (1u16..=254)
            .map(|last| Ipv4Addr::new(octets[0], octets[1], octets[2], last as u8))
            .collect::<Vec<_>>(),
    ));
    let found = Arc::new(Mutex::new(BTreeSet::new()));
    let mut workers = Vec::new();
    for _ in 0..32 {
        let queue = Arc::clone(&queue);
        let found = Arc::clone(&found);
        workers.push(std::thread::spawn(move || loop {
            let ip = queue.lock().ok().and_then(|mut q| q.pop());
            let Some(ip) = ip else { break };
            let address = SocketAddrV4::new(ip, 80);
            let Ok(mut stream) =
                TcpStream::connect_timeout(&address.into(), Duration::from_millis(180))
            else {
                continue;
            };
            let _ = stream.set_read_timeout(Some(Duration::from_millis(250)));
            let request =
                format!("GET /axis-cgi/basicdeviceinfo.cgi HTTP/1.0\r\nHost: {ip}\r\n\r\n");
            if stream.write_all(request.as_bytes()).is_err() {
                continue;
            }
            let mut response = [0u8; 4096];
            let Ok(count) = stream.read(&mut response) else {
                continue;
            };
            let response = String::from_utf8_lossy(&response[..count]).to_ascii_lowercase();
            let (headers, body) = response.split_once("\r\n\r\n").unwrap_or((&response, ""));
            let axis_auth = headers.lines().any(|line| {
                line.starts_with("www-authenticate:")
                    && (line.contains("realm=\"axis") || line.contains("realm=axis"))
            });
            let axis_server = headers.lines().any(|line| {
                line.starts_with("server:")
                    && line
                        .split_once(':')
                        .map(|(_, value)| value.trim_start().starts_with("axis"))
                        .unwrap_or(false)
            });
            let axis_json = body.contains("\"prodnbr\"") || body.contains("\"prodfullname\"");
            if axis_auth || axis_server || axis_json {
                if let Ok(mut result) = found.lock() {
                    result.insert(format!("http://{ip}"));
                }
            }
        }));
    }
    for worker in workers {
        let _ = worker.join();
    }
    Arc::try_unwrap(found)
        .ok()
        .and_then(|m| m.into_inner().ok())
        .unwrap_or_default()
}

fn discover_onvif(timeout: Duration) -> BTreeSet<String> {
    let Ok(socket) = UdpSocket::bind("0.0.0.0:0") else {
        return BTreeSet::new();
    };
    let _ = socket.set_read_timeout(Some(Duration::from_millis(300)));
    let _ = socket.set_multicast_ttl_v4(2);
    let id: u64 = rand::random();
    let probe = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><e:Envelope xmlns:e="http://www.w3.org/2003/05/soap-envelope" xmlns:w="http://schemas.xmlsoap.org/ws/2004/08/addressing" xmlns:d="http://schemas.xmlsoap.org/ws/2005/04/discovery" xmlns:dn="http://www.onvif.org/ver10/network/wsdl"><e:Header><w:MessageID>uuid:{id:016x}-0000-4000-8000-000000000000</w:MessageID><w:To e:mustUnderstand="true">urn:schemas-xmlsoap-org:ws:2005:04:discovery</w:To><w:Action e:mustUnderstand="true">http://schemas.xmlsoap.org/ws/2005/04/discovery/Probe</w:Action></e:Header><e:Body><d:Probe><d:Types>dn:NetworkVideoTransmitter</d:Types></d:Probe></e:Body></e:Envelope>"#
    );
    let target = SocketAddrV4::new(Ipv4Addr::new(239, 255, 255, 250), 3702);
    for _ in 0..3 {
        let _ = socket.send_to(probe.as_bytes(), target);
    }
    let re = Regex::new(r"(?is)<(?:\w+:)?XAddrs[^>]*>(.*?)</(?:\w+:)?XAddrs>").unwrap();
    let url_re = Regex::new(r"https?://[^\s<]+").unwrap();
    let mut found = BTreeSet::new();
    let start = Instant::now();
    let mut buf = [0u8; 65535];
    while start.elapsed() < timeout {
        if let Ok((n, _)) = socket.recv_from(&mut buf) {
            let text = String::from_utf8_lossy(&buf[..n]);
            // Axis identifies itself in ONVIF scopes and/or the probe response.
            // Ignore generic printers, NVRs and unrelated ONVIF devices.
            if !text.to_ascii_lowercase().contains("axis") {
                continue;
            }
            for cap in re.captures_iter(&text) {
                for m in url_re.find_iter(&cap[1]) {
                    if let Ok(u) = url::Url::parse(m.as_str()) {
                        if let Some(host) = u.host_str() {
                            if host.starts_with("169.254.") {
                                continue;
                            }
                            found.insert(format!("{}://{}", u.scheme(), host));
                        }
                    }
                }
            }
        }
    }
    found
}

fn discover_ssdp(timeout: Duration) -> BTreeSet<String> {
    let Ok(socket) = UdpSocket::bind("0.0.0.0:0") else {
        return BTreeSet::new();
    };
    let _ = socket.set_read_timeout(Some(Duration::from_millis(300)));
    let _ = socket.set_multicast_ttl_v4(2);
    let request = b"M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 3\r\nST: ssdp:all\r\n\r\n";
    let target = SocketAddrV4::new(Ipv4Addr::new(239, 255, 255, 250), 1900);
    for _ in 0..3 {
        let _ = socket.send_to(request, target);
    }
    let mut found = BTreeSet::new();
    let start = Instant::now();
    let mut buf = [0u8; 65535];
    while start.elapsed() < timeout {
        if let Ok((n, source)) = socket.recv_from(&mut buf) {
            let text = String::from_utf8_lossy(&buf[..n]);
            if !text.to_ascii_lowercase().contains("axis") {
                continue;
            }
            if let std::net::IpAddr::V4(ip) = source.ip() {
                if !ip.is_link_local() {
                    found.insert(format!("http://{ip}"));
                }
            }
        }
    }
    found
}

pub fn discover(timeout: Duration) -> Vec<String> {
    let half = timeout / 2;
    let onvif = std::thread::spawn(move || discover_onvif(timeout));
    let ssdp = std::thread::spawn(move || discover_ssdp(half));
    let subnet = std::thread::spawn(scan_local_subnet);
    let mut found = onvif.join().unwrap_or_default();
    found.extend(ssdp.join().unwrap_or_default());
    found.extend(subnet.join().unwrap_or_default());
    found.into_iter().collect()
}

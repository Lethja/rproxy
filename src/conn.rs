use std::pin::Pin;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
};

#[cfg(feature = "https")]
use {
    std::convert::TryFrom,
    tokio_rustls::{client, server},
};

use crate::conn::StreamType::*;

#[allow(dead_code)]
pub(crate) struct Uri<'a> {
    pub(crate) uri: String,
    pub(crate) scheme: Option<&'a str>,
    pub(crate) host: Option<&'a str>,
    pub(crate) port: Option<u16>,
    pub(crate) path: Option<&'a str>,
    pub(crate) query: Option<&'a str>,
    pub(crate) path_and_query: Option<&'a str>,
}

#[derive(Debug, PartialEq)]
pub(crate) enum UriKind {
    AbsoluteAddress,
    AbsolutePath,
    RelativeAddress,
    Invalid,
}

impl<'a> From<String> for Uri<'a> {
    fn from(uri: String) -> Self {
        Uri::new(uri)
    }
}

impl<'a> From<&String> for Uri<'a> {
    fn from(uri: &String) -> Self {
        Uri::new(uri.clone())
    }
}

impl<'a> Uri<'a> {
    pub(crate) fn new(uri: String) -> Uri<'a> {
        let mut uri = Uri {
            uri,
            scheme: None,
            host: None,
            port: None,
            path: None,
            query: None,
            path_and_query: None,
        };

        let uri_ref: *mut Uri<'a> = &mut uri;
        unsafe {
            (*uri_ref).update_references();
        }
        uri
    }

    pub(crate) fn kind(&self) -> UriKind {
        if (self.scheme.is_some() || self.port.is_some())
            && self.host.is_some()
            && self.path.is_some()
        {
            return UriKind::AbsoluteAddress;
        }

        if self.host.is_some() && self.path.is_some() {
            return UriKind::RelativeAddress;
        }

        if self.path.is_some_and(|p| p.starts_with('/')) {
            return UriKind::AbsolutePath;
        }

        UriKind::Invalid
    }

    pub(crate) fn same_host_as(&self, other: &Uri) -> bool {
        other.kind() == UriKind::AbsolutePath || self.host == other.host && self.port == other.port
    }

    pub(crate) fn host_and_port(&self) -> Option<String> {
        match (self.host, self.port) {
            (Some(h), Some(p)) => format!("{h}:{p}").into(),
            (_, _) => None,
        }
    }

    pub(crate) fn update_references(&'a mut self) {
        fn find_scheme(value: &str) -> Option<&str> {
            match value.find("://") {
                None => None,
                Some(i) => Some(&value[..i + 3]),
            }
        }

        fn find_host(value: &str) -> Option<&str> {
            match value.starts_with('/') {
                true => None,
                false => {
                    let start = match value.find("://") {
                        None => 0,
                        Some(x) => x + 3,
                    };

                    let end = match value[start..].find(':') {
                        None => match value[start..].find('/') {
                            None => value.len(),
                            Some(s) => s + start,
                        },
                        Some(s) => s + start,
                    };

                    let slice = &value[start..end];
                    if slice.contains('.') {
                        return Some(slice);
                    }
                    None
                }
            }
        }

        fn scheme_to_port(value: &str) -> Option<u16> {
            match find_scheme(value) {
                None => None,
                Some(s) => match s {
                    "http://" => Some(80),
                    "https://" => Some(443),
                    _ => None,
                },
            }
        }

        fn find_port(value: &str) -> Option<u16> {
            let start = match value.find("://") {
                None => 0,
                Some(x) => x + 3,
            };

            let end = match value[start..].find('/') {
                None => value.len(),
                Some(x) => x + start,
            };

            match value[start..end].find(':') {
                None => scheme_to_port(value),
                Some(p) => match value[p + start + 1..end].parse::<u16>() {
                    Ok(p) => Some(p),
                    Err(_) => None,
                },
            }
        }

        fn slice_path(value: &str) -> (Option<&str>, Option<&str>, Option<&str>) {
            let start = match value.starts_with('/') {
                true => 0,
                false => {
                    let scheme = match value.find("://") {
                        None => 0,
                        Some(x) => x + 3,
                    };

                    match value[scheme..].find("/") {
                        None => return (None, None, None),
                        Some(x) => x + scheme,
                    }
                }
            };

            let (end, query) = match value.find('?') {
                None => (value.len(), None),
                Some(x) => (x, Some(&value[x + 1..])),
            };

            (Some(&value[start..end]), query, Some(&value[start..]))
        }

        self.scheme = find_scheme(&self.uri);
        self.host = find_host(&self.uri);
        self.port = find_port(&self.uri);
        (self.path, self.query, self.path_and_query) = slice_path(&self.uri);
    }
}

pub(crate) trait AsyncReadWriteExt: AsyncRead + AsyncWrite + Unpin {}
impl<T: AsyncRead + AsyncWrite + Unpin> AsyncReadWriteExt for T {}

enum StreamType {
    Unencrypted(TcpStream),
    #[cfg(feature = "https")]
    TlsClient(client::TlsStream<TcpStream>),
    #[cfg(feature = "https")]
    TlsServer(server::TlsStream<TcpStream>),
}

pub(crate) struct FetchRequest<'a> {
    uri: Uri<'a>,
    redirect: Option<Uri<'a>>,
    stream: StreamType,
}

impl FetchRequest<'_> {
    async fn from_uri(
        value: &Uri<'_>,
        #[cfg(feature = "https")] certificates: crate::cert::CertificateSetup,
    ) -> Option<Self> {
        let host = match value.host_and_port() {
            None => return None,
            Some(s) => s,
        };

        let redirect = None;

        #[cfg(feature = "https")]
        if value.scheme.is_some_and(|s| s == "https://") {
            let dns = match value.host {
                None => return None,
                Some(o) => o.to_string(),
            };

            use tokio_rustls::rustls::pki_types::ServerName;
            let domain = match ServerName::try_from(dns) {
                Ok(d) => d,
                Err(_) => return None,
            };

            let stream = match TcpStream::connect(host).await {
                Ok(o) => o,
                Err(_) => return None,
            };

            let stream: StreamType = match certificates.client_config.connect(domain, stream).await
            {
                Ok(s) => TlsClient(s),
                Err(_) => return None,
            };

            let uri = Uri::from(&value.uri);

            return Some(Self {
                uri,
                redirect,
                stream,
            });
        }

        let scheme = match value.scheme {
            None => return None,
            Some(s) => s,
        };

        if scheme == "http://" {
            let stream = match TcpStream::connect(host).await {
                Ok(o) => Unencrypted(o),
                Err(_) => return None,
            };

            let uri = Uri::from(&value.uri);
            return Some(FetchRequest {
                uri,
                redirect,
                stream,
            });
        }

        None
    }

    pub(crate) fn as_stream(&mut self) -> Pin<Box<dyn AsyncReadWriteExt + '_>> {
        match self.stream {
            Unencrypted(ref mut stream) => Box::pin(stream),
            #[cfg(feature = "https")]
            TlsClient(ref mut stream) => Box::pin(stream),
            #[cfg(feature = "https")]
            TlsServer(ref mut stream) => Box::pin(stream),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_uri_absolute_address() {
        let uri = Uri::new("http://example.com/path".to_string());
        assert_eq!(uri.kind(), UriKind::AbsoluteAddress);
        assert_eq!(uri.scheme, Some("http://"));
        assert_eq!(uri.host, Some("example.com"));
        assert_eq!(uri.port, Some(80));
        assert_eq!(uri.path, Some("/path"));
        assert_eq!(uri.query, None);
        assert_eq!(uri.path_and_query, uri.path);
    }

    #[test]
    fn test_uri_absolute_address_with_query() {
        let uri = Uri::new("http://example.com/path?query=something".to_string());
        assert_eq!(uri.kind(), UriKind::AbsoluteAddress);
        assert_eq!(uri.scheme, Some("http://"));
        assert_eq!(uri.host, Some("example.com"));
        assert_eq!(uri.port, Some(80));
        assert_eq!(uri.path, Some("/path"));
        assert_eq!(uri.query, Some("query=something"));
        assert_eq!(uri.path_and_query, Some("/path?query=something"));
    }

    #[test]
    fn test_uri_absolute_address_with_port() {
        let uri = Uri::new("https://example.com:8443/path?query=something".to_string());
        assert_eq!(uri.kind(), UriKind::AbsoluteAddress);
        assert_eq!(uri.scheme, Some("https://"));
        assert_eq!(uri.host, Some("example.com"));
        assert_eq!(uri.port, Some(8443));
        assert_eq!(uri.path, Some("/path"));
        assert_eq!(uri.query, Some("query=something"));
        assert_eq!(uri.path_and_query, Some("/path?query=something"));
    }

    #[test]
    fn test_uri_relative_address() {
        let uri = Uri::new("example.com/path?query=something".to_string());
        assert_eq!(uri.kind(), UriKind::RelativeAddress);
        assert_eq!(uri.scheme, None);
        assert_eq!(uri.host, Some("example.com"));
        assert_eq!(uri.port, None);
        assert_eq!(uri.path, Some("/path"));
        assert_eq!(uri.query, Some("query=something"));
        assert_eq!(uri.path_and_query, Some("/path?query=something"));
    }

    #[test]
    fn test_uri_absolute_path() {
        let uri = Uri::new("/path/to/resource".to_string());
        assert_eq!(uri.kind(), UriKind::AbsolutePath);
        assert_eq!(uri.scheme, None);
        assert_eq!(uri.host, None);
        assert_eq!(uri.port, None);
        assert_eq!(uri.path, Some("/path/to/resource"));
        assert_eq!(uri.query, None);
        assert_eq!(uri.path_and_query, Some("/path/to/resource"));
    }

    #[test]
    fn test_uri_absolute_path_with_query() {
        let uri = Uri::new("/path/to/resource?query=something".to_string());
        assert_eq!(uri.kind(), UriKind::AbsolutePath);
        assert_eq!(uri.scheme, None);
        assert_eq!(uri.host, None);
        assert_eq!(uri.port, None);
        assert_eq!(uri.path, Some("/path/to/resource"));
        assert_eq!(uri.query, Some("query=something"));
        assert_eq!(
            uri.path_and_query,
            Some("/path/to/resource?query=something")
        );
    }

    #[test]
    fn test_uri_invalid() {
        let uri = Uri::new("not_a_valid_uri".to_string());
        assert_eq!(uri.kind(), UriKind::Invalid);
        assert_eq!(uri.scheme, None);
        assert_eq!(uri.host, None);
        assert_eq!(uri.port, None);
        assert_eq!(uri.path, None);
        assert_eq!(uri.query, None);
        assert_eq!(uri.path_and_query, None);
    }
}

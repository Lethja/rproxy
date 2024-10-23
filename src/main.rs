mod http;

use crate::http::{HttpRequestHeader, HttpRequestMethod, HttpResponseHeader, HttpResponseStatus};
use std::path::PathBuf;
use tokio::fs::File;
use tokio::{
    io::{AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
};
use url::{Host, Url};

#[tokio::main]
async fn main() {
    let listener = match TcpListener::bind("0.0.0.0:3142").await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("Unable to bind server: {e}");
            return;
        }
    };

    loop {
        let (stream, _) = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("Unable to accept server socket: {e}");
                return;
            }
        };

        tokio::spawn(async move {
            handle_connection(stream).await;
        });
    }
}

async fn handle_connection(mut stream: TcpStream) {
    let mut client_buf_reader = BufReader::new(&mut stream);
    let client_request_header =
        match HttpRequestHeader::from_tcp_buffer_async(&mut client_buf_reader).await {
            None => return,
            Some(header) => header,
        };

    #[cfg(debug_assertions)]
    println!("header = {:?}", client_request_header.generate());

    match client_request_header.method {
        HttpRequestMethod::Get => {
            if client_request_header.has_relative_path() {
                match client_request_header.get_query() {
                    None => {
                        let response = HttpResponseStatus::NO_CONTENT.to_header();
                        stream
                            .write_all(response.as_bytes())
                            .await
                            .unwrap_or_else(|_| ());
                        return;
                    }
                    Some(_q) => {
                        todo!("If query is certs offer the certificate")
                    }
                };
            } else {
                let host = match url_is_http(&client_request_header.path) {
                    None => {
                        let response = HttpResponseStatus::INTERNAL_SERVER_ERROR.to_response();
                        stream
                            .write_all(response.as_bytes())
                            .await
                            .unwrap_or_else(|_| ());
                        return;
                    }
                    Some(h) => h.to_string(),
                };

                let mut fetch_stream = match TcpStream::connect(host).await {
                    Ok(s) => s,
                    Err(_) => {
                        let response = HttpResponseStatus::BAD_GATEWAY.to_response();
                        stream
                            .write_all(response.as_bytes())
                            .await
                            .unwrap_or_else(|_| ());
                        return;
                    }
                };

                let mut fetch_buf_reader = BufReader::new(&mut fetch_stream);

                let fetch_request = HttpRequestHeader {
                    method: HttpRequestMethod::Get,
                    path: client_request_header.path.clone(),
                    version: client_request_header.version,
                    headers: {
                        let mut headers = client_request_header.headers.clone();
                        headers.remove("Range");
                        headers
                    },
                };

                match fetch_buf_reader
                    .write_all(fetch_request.generate().as_ref())
                    .await
                {
                    Ok(_) => {}
                    Err(_) => {
                        let response = HttpResponseStatus::INTERNAL_SERVER_ERROR.to_response();
                        client_buf_reader
                            .write_all(response.as_bytes())
                            .await
                            .unwrap_or_else(|_| ());
                    }
                }

                let mut fetch_response_header =
                    match HttpResponseHeader::from_tcp_buffer_async(fetch_buf_reader).await {
                        None => {
                            let response = HttpResponseStatus::BAD_GATEWAY.to_response();
                            stream
                                .write_all(response.as_bytes())
                                .await
                                .unwrap_or_else(|_| ());
                            return;
                        }
                        Some(s) => s,
                    };

                let fetch_response_header_data = fetch_response_header.generate();
                match client_buf_reader
                    .write_all(fetch_response_header_data.as_bytes())
                    .await
                {
                    Ok(_) => {}
                    Err(_) => return,
                }

                match fetch_response_header.status.to_code() {
                    200 => {
                        let file = match fetch_response_header
                            .get_cache_name(&client_request_header.path)
                        {
                            None => None,
                            Some(path) => match File::create(path).await {
                                Ok(f) => Some(f),
                                Err(_) => None,
                            },
                        };

                        if let file = Some(file) {
                            todo!("Setup a way to write to file")
                        }

                        todo!("Setup a way to write to client")
                    }
                    _ => {
                        todo!("Respond without a cache file")
                    }
                }
            }
        }
        _ => {
            let response = HttpResponseStatus::METHOD_NOT_ALLOWED.to_response();
            stream
                .write_all(response.as_bytes())
                .await
                .unwrap_or_default()
        }
    }
}

fn url_is_http(url: &Url) -> Option<Host> {
    if url.scheme() != "http" {
        return None;
    }

    let host = match url.host() {
        None => return None,
        Some(s) => s,
    };

    let port = url.port_or_known_default().unwrap_or(80);

    let host = format!("{host}:{port}");

    match Host::parse(host.as_str()) {
        Ok(h) => Some(h),
        Err(_) => None,
    }
}

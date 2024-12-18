use {
    crate::{
        conn::{FetchRequest, FlightState, Flights, Uri},
        debug_print,
        http::{
            fetch_and_serve_chunk, fetch_and_serve_known_length, keep_alive_if, respond_with,
            ConnectionReturn,
            ConnectionReturn::{Close, Redirect},
            HttpRequestHeader, HttpRequestMethod, HttpResponseHeader, HttpResponseStatus,
            HttpVersion,
        },
    },
    std::{collections::VecDeque, path::PathBuf, sync::Arc, time::Duration},
    tokio::{
        fs::{create_dir_all, remove_file, File},
        io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
        time::timeout,
    },
};

#[cfg(feature = "https")]
use crate::cert::CertificateSetup;

pub(crate) async fn fetch_and_serve_file<T>(
    cache_file_path: PathBuf,
    mut stream: T,
    flights: &Arc<Flights>,
    client_request_header: HttpRequestHeader<'_>,
    #[cfg(feature = "https")] certificates: &CertificateSetup,
) -> ConnectionReturn
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    let mut fetch_request: FetchRequest =
        match FetchRequest::from_uri(&client_request_header.request) {
            Ok(o) => o,
            Err(_) => {
                return respond_with(
                    Close,
                    HttpResponseStatus::INTERNAL_SERVER_ERROR,
                    &mut stream,
                )
                .await
            }
        };

    match fetch_request
        .connect(
            #[cfg(feature = "https")]
            certificates,
        )
        .await
    {
        Ok(_) => (),
        Err(_) => {
            return respond_with(
                Close,
                HttpResponseStatus::INTERNAL_SERVER_ERROR,
                &mut stream,
            )
            .await
        }
    };

    let mut redirects: VecDeque<String> = VecDeque::new();
    redirects.push_back(fetch_request.uri().uri.clone());

    loop {
        let uri = redirects.back().unwrap();

        let mut fetch_stream = match fetch_request.as_stream() {
            None => {
                return respond_with(
                    Close,
                    HttpResponseStatus::INTERNAL_SERVER_ERROR,
                    &mut stream,
                )
                .await
            }
            Some(f) => f,
        };

        let current_uri = Uri::from(uri);

        debug_print!("Fetching {}", current_uri.uri);

        let fetch_result = fetch(
            &current_uri,
            &cache_file_path,
            flights,
            &client_request_header,
            &mut fetch_stream,
            &mut stream,
        )
        .await;

        drop(fetch_stream);

        match fetch_result {
            Redirect(r) => {
                if redirects.len() > 5 {
                    return respond_with(
                        Close,
                        HttpResponseStatus::INTERNAL_SERVER_ERROR,
                        &mut stream,
                    )
                    .await;
                }

                if redirects.contains(&r) {
                    return respond_with(
                        Close,
                        HttpResponseStatus::INTERNAL_SERVER_ERROR,
                        &mut stream,
                    )
                    .await;
                } else {
                    redirects.push_back(r);
                }

                let new_uri = Uri::from(&redirects);

                match fetch_request
                    .redirect(
                        &new_uri,
                        #[cfg(feature = "https")]
                        certificates,
                    )
                    .await
                {
                    Ok(o) => o,
                    Err(_) => {
                        return respond_with(
                            Close,
                            HttpResponseStatus::INTERNAL_SERVER_ERROR,
                            &mut stream,
                        )
                        .await
                    }
                };

                continue;
            }
            x => return x,
        }
    }

    async fn fetch<R, S>(
        uri: &Uri<'_>,
        cache_file_path: &PathBuf,
        flights: &Arc<Flights>,
        client_request_header: &HttpRequestHeader<'_>,
        fetch_stream: &mut R,
        mut stream: &mut S,
    ) -> ConnectionReturn
    where
        R: AsyncRead + AsyncWrite + Unpin,
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let host = match uri.host {
            None => {
                return respond_with(
                    keep_alive_if(client_request_header),
                    HttpResponseStatus::BAD_REQUEST,
                    stream,
                )
                .await
            }
            Some(s) => s.to_string(),
        };

        let path_and_query = match uri.path_and_query {
            None => {
                return respond_with(
                    keep_alive_if(client_request_header),
                    HttpResponseStatus::BAD_REQUEST,
                    stream,
                )
                .await
            }
            Some(s) => s.to_string(),
        };

        let fetch_request = HttpRequestHeader {
            method: HttpRequestMethod::Get,
            request: Uri::from(path_and_query),
            version: HttpVersion::from(client_request_header.version.as_str()),
            headers: {
                let mut headers = client_request_header.headers.clone();
                headers.remove("Range"); /* Not cached so need to download from start */
                headers.insert("Host".to_string(), host); /* Host field is mandatory on HTTP 1.1 */
                headers
            },
        };

        match fetch_request.generate() {
            None => {
                return respond_with(
                    keep_alive_if(client_request_header),
                    HttpResponseStatus::INTERNAL_SERVER_ERROR,
                    stream,
                )
                .await
            }
            Some(s) => {
                debug_print!("Writing header\n\n{}", s);
                if fetch_stream.write_all(s.as_bytes()).await.is_err() {
                    return respond_with(
                        keep_alive_if(client_request_header),
                        HttpResponseStatus::INTERNAL_SERVER_ERROR,
                        stream,
                    )
                    .await;
                }
            }
        };

        let mut fetch_buf_reader = BufReader::new(fetch_stream);

        let mut fetch_response_header =
            match HttpResponseHeader::from_tcp_buffer_async(&mut fetch_buf_reader).await {
                None => {
                    eprintln!("Error: unable to extract header");
                    return respond_with(
                        keep_alive_if(client_request_header),
                        HttpResponseStatus::BAD_GATEWAY,
                        stream,
                    )
                    .await;
                }
                Some(s) => s,
            };

        match fetch_response_header.status.to_code() {
            200 => {
                let cache_file_parent = match cache_file_path.parent() {
                    None => {
                        return respond_with(
                            keep_alive_if(client_request_header),
                            HttpResponseStatus::INTERNAL_SERVER_ERROR,
                            stream,
                        )
                        .await
                    }
                    Some(p) => p,
                };
                match create_dir_all(cache_file_parent).await {
                    Ok(_) => {}
                    Err(_) => {
                        return respond_with(
                            keep_alive_if(client_request_header),
                            HttpResponseStatus::INTERNAL_SERVER_ERROR,
                            stream,
                        )
                        .await
                    }
                }
                let mut file = match File::create(&cache_file_path).await {
                    Err(_) => {
                        return respond_with(
                            keep_alive_if(client_request_header),
                            HttpResponseStatus::INTERNAL_SERVER_ERROR,
                            stream,
                        )
                        .await
                    }
                    Ok(file) => file,
                };

                match write_to_client(&mut fetch_response_header, &mut stream).await {
                    Ok(o) => o,
                    Err(_) => return Close, /* Something broke */
                }

                let (mut write_file, mut write_stream) = fetch_cache_policy(&fetch_response_header);

                if let Some(v) = fetch_response_header.headers.get("Transfer-Encoding") {
                    if v.to_lowercase() == "chunked" {
                        flights
                            .takeoff(
                                cache_file_path.to_string_lossy().as_ref(),
                                FlightState::Chunks,
                            )
                            .await;
                        (write_file, write_stream) = fetch_and_serve_chunk(
                            cache_file_path,
                            &mut stream,
                            &mut fetch_buf_reader,
                            &mut file,
                            write_file,
                            write_stream,
                        )
                        .await
                    } else {
                        return respond_with(
                            keep_alive_if(client_request_header),
                            HttpResponseStatus::BAD_REQUEST,
                            stream,
                        )
                        .await;
                    }
                } else {
                    let content_length = match fetch_response_header.headers.get("Content-Length") {
                        None => {
                            return respond_with(
                                keep_alive_if(client_request_header),
                                HttpResponseStatus::INTERNAL_SERVER_ERROR,
                                stream,
                            )
                            .await
                        }
                        Some(s) => match s.parse::<u64>() {
                            Ok(u) => {
                                flights
                                    .takeoff(
                                        cache_file_path.to_string_lossy().as_ref(),
                                        FlightState::Length(u),
                                    )
                                    .await;
                                u
                            }
                            Err(_) => {
                                return respond_with(
                                    keep_alive_if(client_request_header),
                                    HttpResponseStatus::BAD_REQUEST,
                                    stream,
                                )
                                .await
                            }
                        },
                    };

                    (write_file, write_stream) = fetch_and_serve_known_length(
                        cache_file_path,
                        &mut stream,
                        content_length,
                        &mut fetch_buf_reader,
                        &mut file,
                        write_file,
                        write_stream,
                    )
                    .await;
                }

                let _ = timeout(Duration::from_millis(100), fetch_buf_reader.shutdown()).await;

                if write_stream {
                    let _ = timeout(Duration::from_millis(100), stream.shutdown()).await;
                }

                if write_file {
                    if let Some(last_modified) = fetch_response_header.headers.get("Last-Modified")
                    {
                        if let Ok(last_modified) = httpdate::parse_http_date(last_modified) {
                            let _ = timeout(
                                Duration::from_millis(100),
                                tokio::spawn(async move {
                                    let _ = file.into_std().await.set_modified(last_modified);
                                }),
                            )
                            .await;
                        }
                    }
                } else if cache_file_path.is_file() {
                    let _ = remove_file(cache_file_path).await;
                    return Close; /* Something has gone wrong mid-transmission */
                }
                return keep_alive_if(client_request_header); /* Next request ready */

                fn fetch_cache_policy(response_header: &HttpResponseHeader) -> (bool, bool) {
                    match response_header.headers.get("Cache-Control") {
                        None => (true, true),
                        Some(v) => match v.to_lowercase().as_str() {
                            "no-store" | "private" => (false, true),
                            _ => (true, true),
                        },
                    }
                }
            }
            301..303 | 307..308 => {
                let url = match fetch_response_header.headers.get("Location") {
                    None => {
                        return respond_with(
                            keep_alive_if(client_request_header),
                            HttpResponseStatus::BAD_REQUEST,
                            stream,
                        )
                        .await
                    }
                    Some(s) => s,
                };
                Redirect(String::from(url))
            }
            _x => {
                let pass_through = fetch_response_header.generate();
                debug_print!(
                    "Proxy will pass-through {_x} from server to client\n\
                 Header as follows:\n\n{pass_through}"
                );
                match stream.write_all(pass_through.as_bytes()).await {
                    Ok(_) => keep_alive_if(client_request_header),
                    Err(_) => Close,
                }
            }
        }
    }

    async fn write_to_client<T>(
        fetch_response_header: &mut HttpResponseHeader,
        stream: &mut T,
    ) -> std::io::Result<()>
    where
        T: AsyncRead + AsyncWrite + Unpin,
    {
        let fetch_response_header_data = fetch_response_header.generate();

        stream
            .write_all(fetch_response_header_data.as_bytes())
            .await
    }
}

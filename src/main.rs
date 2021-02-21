mod auth;
mod buffer;
mod cli;
mod download;
mod formatting;
mod printer;
mod request_items;
mod to_curl;
mod url;
mod utils;
mod vendored;

use std::fs::File;
use std::io::{stdin, Read};

use anyhow::{anyhow, Context, Result};
use atty::Stream;
use reqwest::blocking::Client;
use reqwest::header::{
    HeaderValue, ACCEPT, ACCEPT_ENCODING, CONNECTION, CONTENT_TYPE, RANGE, USER_AGENT,
};
use reqwest::redirect::Policy;
use reqwest::Method;

use crate::auth::parse_auth;
use crate::buffer::Buffer;
use crate::cli::{Cli, Pretty, Print, Proxy, Theme, Verify};
use crate::download::{download_file, get_file_size};
use crate::printer::Printer;
use crate::request_items::{Body, RequestItems};
use crate::url::construct_url;
use crate::utils::{test_mode, test_pretend_term};

fn get_user_agent() -> &'static str {
    if test_mode() {
        // Hard-coded user agent for the benefit of tests
        "xh/0.0.0 (test mode)"
    } else {
        concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"))
    }
}

fn main() -> Result<()> {
    std::process::exit(inner_main()?);
}

/// [`main`] is wrapped around this function so it can safely exit with an
/// exit code.
///
/// [`std::process::exit`] is a hard termination, that ends the process
/// without doing any cleanup. So we need to return from this function first.
///
/// The outer main function could also be a good place for error handling.
fn inner_main() -> Result<i32> {
    let args = Cli::from_args();

    if args.curl {
        to_curl::print_curl_translation(args)?;
        return Ok(0);
    }

    let request_items = RequestItems::new(args.request_items);
    let query = request_items.query();
    let (headers, headers_to_unset) = request_items.headers()?;
    let url = construct_url(&args.url, args.default_scheme.as_deref(), query)?;

    let ignore_stdin = args.ignore_stdin || atty::is(Stream::Stdin) || test_pretend_term();
    let body = match request_items.body(args.form, args.multipart)? {
        Some(_) if !ignore_stdin => {
            return Err(anyhow!(
                "Request body (from stdin) and Request data (key=value) cannot be mixed"
            ));
        }
        None if !ignore_stdin => {
            let mut buffer = Vec::new();
            stdin().read_to_end(&mut buffer)?;
            Some(Body::Raw(buffer))
        }
        body => body,
    };

    let method = args.method.unwrap_or_else(|| {
        if body.is_some() {
            Method::POST
        } else {
            Method::GET
        }
    });
    let redirect = match args.follow {
        true => Policy::limited(args.max_redirects.unwrap_or(10)),
        false => Policy::none(),
    };

    let mut client = Client::builder().redirect(redirect);
    let mut resume: Option<u64> = None;

    if url.scheme() == "https" {
        if args.verify == Verify::No {
            client = client.danger_accept_invalid_certs(true);
        }

        if let Verify::CustomCABundle(path) = args.verify {
            client = client.tls_built_in_root_certs(false);

            let mut buffer = Vec::new();
            let mut file = File::open(&path).with_context(|| {
                format!("Failed to open the custom CA bundle: {}", path.display())
            })?;
            file.read_to_end(&mut buffer).with_context(|| {
                format!("Failed to read the custom CA bundle: {}", path.display())
            })?;

            for pem in pem::parse_many(buffer) {
                let certificate = reqwest::Certificate::from_pem(pem::encode(&pem).as_bytes())
                    .with_context(|| {
                        format!("Failed to load the custom CA bundle: {}", path.display())
                    })?;
                client = client.add_root_certificate(certificate);
            }
        };

        if let Some(cert) = args.cert {
            let mut buffer = Vec::new();
            let mut file = File::open(&cert)
                .with_context(|| format!("Failed to open the cert file: {}", cert.display()))?;
            file.read_to_end(&mut buffer)
                .with_context(|| format!("Failed to read the cert file: {}", cert.display()))?;

            if let Some(cert_key) = args.cert_key {
                buffer.push(b'\n');

                let mut file = File::open(&cert_key).with_context(|| {
                    format!("Failed to open the cert key file: {}", cert_key.display())
                })?;
                file.read_to_end(&mut buffer).with_context(|| {
                    format!("Failed to read the cert key file: {}", cert_key.display())
                })?;
            }

            let identity = reqwest::Identity::from_pem(&buffer)
                .context("Failed to parse the cert/cert key files")?;
            client = client.identity(identity);
        };
    }

    for proxy in args.proxy.into_iter().rev() {
        client = client.proxy(match proxy {
            Proxy::Http(url) => reqwest::Proxy::http(url),
            Proxy::Https(url) => reqwest::Proxy::https(url),
            Proxy::All(url) => reqwest::Proxy::all(url),
        }?);
    }

    let client = client.build()?;

    let request = {
        let mut request_builder = client
            .request(method, url.clone())
            .header(ACCEPT_ENCODING, HeaderValue::from_static("gzip, deflate"))
            .header(CONNECTION, HeaderValue::from_static("keep-alive"))
            .header(USER_AGENT, get_user_agent());

        request_builder = match body {
            Some(Body::Form(body)) => request_builder
                .header(ACCEPT, HeaderValue::from_static("*/*"))
                .form(&body),
            Some(Body::Multipart(body)) => request_builder
                .header(ACCEPT, HeaderValue::from_static("*/*"))
                .multipart(body),
            Some(Body::Json(body)) => request_builder
                .header(ACCEPT, HeaderValue::from_static("application/json, */*"))
                .json(&body),
            Some(Body::Raw(body)) => request_builder
                .header(ACCEPT, HeaderValue::from_static("application/json, */*"))
                .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
                .body(body),
            None => request_builder.header(ACCEPT, HeaderValue::from_static("*/*")),
        };

        if args.resume {
            if let Some(file_size) = get_file_size(args.output.as_deref()) {
                request_builder = request_builder.header(RANGE, format!("bytes={}-", file_size));
                resume = Some(file_size);
            }
        }

        if let Some(auth) = args.auth {
            let (username, password) = parse_auth(auth, url.host_str().unwrap_or("<host>"))?;
            request_builder = request_builder.basic_auth(username, password);
        }
        if let Some(token) = args.bearer {
            request_builder = request_builder.bearer_auth(token);
        }

        let mut request = request_builder.headers(headers).build()?;

        headers_to_unset.iter().for_each(|h| {
            request.headers_mut().remove(h);
        });

        request
    };

    let buffer = Buffer::new(
        args.download,
        &args.output,
        atty::is(Stream::Stdout) || test_pretend_term(),
    )?;
    let is_redirect = buffer.is_redirect();
    let print = match args.print {
        Some(print) => print,
        None => Print::new(
            args.verbose,
            args.headers,
            args.body,
            args.quiet,
            args.offline,
            &buffer,
        ),
    };
    let mut printer = Printer::new(args.pretty, args.theme, args.stream, buffer);

    if print.request_headers {
        printer.print_request_headers(&request)?;
    }
    if print.request_body {
        printer.print_request_body(&request)?;
    }
    if !args.offline {
        let orig_url = request.url().clone();
        let response = client.execute(request)?;
        if print.response_headers {
            printer.print_response_headers(&response)?;
        }
        let status = response.status();
        let exit_code: i32 = match status.as_u16() {
            _ if !(args.check_status || args.download) => 0,
            300..=399 if !args.follow => 3,
            400..=499 => 4,
            500..=599 => 5,
            _ => 0,
        };
        if is_redirect && exit_code != 0 {
            eprintln!("\n{}: warning: HTTP {}\n", env!("CARGO_PKG_NAME"), status);
        }
        if args.download {
            if exit_code == 0 {
                download_file(response, args.output, &orig_url, resume, args.quiet)?;
            }
        } else if print.response_body {
            printer.print_response_body(response)?;
        }
        Ok(exit_code)
    } else {
        Ok(0)
    }
}

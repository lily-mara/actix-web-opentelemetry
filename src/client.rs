use crate::util::http_method_str;
use actix_http::{encoding::Decoder, Error, Payload, PayloadStream};
use actix_web::{
    body::Body,
    client::{ClientRequest, ClientResponse, SendRequestError},
    http::{HeaderName, HeaderValue},
    web::Bytes,
};
use futures::{future::TryFutureExt, Future, Stream};
use opentelemetry::{
    global,
    propagation::Injector,
    trace::{SpanKind, StatusCode, TraceContextExt, Tracer},
    Context,
};
use opentelemetry_semantic_conventions::trace::{
    HTTP_FLAVOR, HTTP_METHOD, HTTP_STATUS_CODE, HTTP_URL, NET_PEER_IP,
};
use serde::Serialize;
use std::fmt;
use std::str::FromStr;

/// A wrapper for the actix-web [`ClientRequest`].
///
/// [`ClientRequest`]: actix_web::client::ClientRequest
#[derive(Debug)]
pub struct InstrumentedClientRequest {
    cx: Context,
    request: ClientRequest,
}

/// OpenTelemetry extensions for actix-web's [`Client`].
///
/// [`Client`]: actix_web::client::Client
pub trait ClientExt {
    /// Trace an `actix_web::client::Client` request using the current context.
    ///
    /// Example:
    /// ```no_run
    /// use actix_web::client;
    /// use actix_web_opentelemetry::ClientExt;
    ///
    /// async fn execute_request(client: &client::Client) -> Result<(), client::SendRequestError> {
    ///     let res = client.get("http://localhost:8080")
    ///         // Add `trace_request` before `send` to any awc request to add instrumentation
    ///         .trace_request()
    ///         .send()
    ///         .await?;
    ///
    ///     println!("Response: {:?}", res);
    ///     Ok(())
    /// }
    /// ```
    fn trace_request(self) -> InstrumentedClientRequest
    where
        Self: Sized,
    {
        self.trace_request_with_context(Context::current())
    }

    /// Trace an [`actix_web::client::Client`] request using the given span context.
    ///
    ///[`actix_web::client::Client`]: actix_web::client::Client
    /// Example:
    /// ```no_run
    /// use actix_web::client;
    /// use actix_web_opentelemetry::ClientExt;
    /// use opentelemetry::Context;
    ///
    /// async fn execute_request(client: &client::Client) -> Result<(), client::SendRequestError> {
    ///     let res = client.get("http://localhost:8080")
    ///         // Add `trace_request_with_context` before `send` to any awc request to
    ///         // add instrumentation
    ///         .trace_request_with_context(Context::current())
    ///         .send()
    ///         .await?;
    ///
    ///     println!("Response: {:?}", res);
    ///     Ok(())
    /// }
    /// ```
    fn trace_request_with_context(self, cx: Context) -> InstrumentedClientRequest;
}

impl ClientExt for ClientRequest {
    fn trace_request_with_context(self, cx: Context) -> InstrumentedClientRequest {
        InstrumentedClientRequest { cx, request: self }
    }
}

type AwcResult = Result<ClientResponse<Decoder<Payload<PayloadStream>>>, SendRequestError>;

impl InstrumentedClientRequest {
    /// Generate an awc [`ClientResponse`] from a traced request with an empty body.
    ///
    /// [`ClientResponse`]: actix_web::client::ClientResponse
    pub async fn send(self) -> AwcResult {
        self.trace_request(|request| request.send()).await
    }

    /// Generate an awc [`ClientResponse`] from a traced request with the given body.
    ///
    /// [`ClientResponse`]: actix_web::client::ClientResponse
    pub async fn send_body<B>(self, body: B) -> AwcResult
    where
        B: Into<Body>,
    {
        self.trace_request(|request| request.send_body(body)).await
    }

    /// Generate an awc [`ClientResponse`] from a traced request with the given form
    /// body.
    ///
    /// [`ClientResponse`]: actix_web::client::ClientResponse
    pub async fn send_form<T: Serialize>(self, value: &T) -> AwcResult {
        self.trace_request(|request| request.send_form(value)).await
    }

    /// Generate an awc [`ClientResponse`] from a traced request with the given JSON
    /// body.
    ///
    /// [`ClientResponse`]: actix_web::client::ClientResponse
    pub async fn send_json<T: Serialize>(self, value: &T) -> AwcResult {
        self.trace_request(|request| request.send_json(value)).await
    }

    /// Generate an awc [`ClientResponse`] from a traced request with the given stream
    /// body.
    ///
    /// [`ClientResponse`]: actix_web::client::ClientResponse
    pub async fn send_stream<S, E>(self, stream: S) -> AwcResult
    where
        S: Stream<Item = Result<Bytes, E>> + Unpin + 'static,
        E: Into<Error> + 'static,
    {
        self.trace_request(|request| request.send_stream(stream))
            .await
    }

    async fn trace_request<F, R>(mut self, f: F) -> AwcResult
    where
        F: FnOnce(ClientRequest) -> R,
        R: Future<Output = AwcResult>,
    {
        let tracer = global::tracer("actix-client");
        let mut attributes = vec![
            HTTP_METHOD.string(http_method_str(self.request.get_method())),
            HTTP_URL.string(self.request.get_uri().to_string()),
            HTTP_FLAVOR.string(format!("{:?}", self.request.get_version()).replace("HTTP/", "")),
        ];

        if let Some(peer_addr) = self.request.get_peer_addr() {
            attributes.push(NET_PEER_IP.string(peer_addr.to_string()));
        }

        let span = tracer
            .span_builder(format!(
                "{} {}{}{}",
                self.request.get_method(),
                self.request
                    .get_uri()
                    .scheme()
                    .map(|s| format!("{}://", s.as_str()))
                    .unwrap_or_else(String::new),
                self.request
                    .get_uri()
                    .authority()
                    .map(|s| s.as_str())
                    .unwrap_or(""),
                self.request.get_uri().path()
            ))
            .with_kind(SpanKind::Client)
            .with_attributes(attributes)
            .start(&tracer);
        let cx = self.cx.with_span(span);

        global::get_text_map_propagator(|injector| {
            injector.inject_context(&cx, &mut ActixClientCarrier::new(&mut self.request));
        });

        f(self.request)
            .inspect_ok(|res| record_response(&res, &cx))
            .inspect_err(|err| record_err(err, &cx))
            .await
    }
}

fn record_response<T>(response: &ClientResponse<T>, cx: &Context) {
    let span = cx.span();
    span.set_attribute(HTTP_STATUS_CODE.i64(response.status().as_u16() as i64));
    span.end();
}

fn record_err<T: fmt::Debug>(err: T, cx: &Context) {
    let span = cx.span();
    span.set_status(StatusCode::Error, format!("{:?}", err));
    span.end();
}

struct ActixClientCarrier<'a> {
    request: &'a mut ClientRequest,
}

impl<'a> ActixClientCarrier<'a> {
    fn new(request: &'a mut ClientRequest) -> Self {
        ActixClientCarrier { request }
    }
}

impl<'a> Injector for ActixClientCarrier<'a> {
    fn set(&mut self, key: &str, value: String) {
        let header_name = HeaderName::from_str(key).expect("Must be header name");
        let header_value = HeaderValue::from_str(&value).expect("Must be a header value");
        self.request.headers_mut().insert(header_name, header_value);
    }
}

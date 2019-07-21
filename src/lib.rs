//! [Actix]-[AWS Lambda] connector for Actix 1.x
//!
//! [Actix]: https://crates.io/crates/actix-web
//! [AWS Lambda]: https://crates.io/crates/lambda_runtime
use actix_http::{Request, Response};
use actix_server_config::ServerConfig;
use actix_service::{IntoNewService, NewService, Service};
use actix_web::{
    dev::{MessageBody, ResponseBody},
    http::uri,
    web::{Bytes, BytesMut},
    Error,
};
use futures::Stream;
use lambda_http::{http::header::CONTENT_TYPE, Body as LambdaBody, RequestExt};
use lambda_runtime::error::HandlerError;
use log::{debug, warn};
use percent_encoding::utf8_percent_encode;
use std::{fmt::Write, marker::PhantomData, mem::replace};

/// `percent_encoding` implements the percent encoding algorithm in the WHATWG
/// URL standard which is designed to deal with input that may already be
/// partially percent-encoded. To do a full percent encoding, we add `%` to the
/// encode set.
mod enc_set {
    use percent_encoding::{define_encode_set, QUERY_ENCODE_SET};
    define_encode_set! {
        pub URL_ENCODE = [QUERY_ENCODE_SET] | {'%'}
    }
}

pub struct LambdaHttpServer<F, R, S, B>
where
    F: FnOnce() -> R,
    R: IntoNewService<S>,
    S: NewService<Config = ServerConfig, Request = Request>,
    S::Error: Into<Error>,
    S::Response: Into<Response<B>>,
    S::Service: 'static,
    B: MessageBody + 'static,
{
    factory: F,
    binary_media_type_fn: Box<dyn FnMut(&str) -> bool>,
    _t: PhantomData<(S, B)>,
}

impl<F, R, S, B> LambdaHttpServer<F, R, S, B>
where
    F: FnOnce() -> R,
    R: IntoNewService<S>,
    S: NewService<Config = ServerConfig, Request = Request>,
    S::Error: Into<Error>,
    S::Response: Into<Response<B>>,
    S::Service: 'static,
    B: MessageBody + 'static,
{
    /// Construct a `LambdaHttpServer`.
    pub fn new(app_factory: F) -> Self {
        Self {
            factory: app_factory,
            binary_media_type_fn: Box::new(|_| false),
            _t: PhantomData,
        }
    }

    /// Set a predicate that, given a content type (or an empty string if
    /// `content-type` is missing or invalid), returns a flag indicating whether
    /// the response of the specified content type should be base64-encoded.
    ///
    /// If the provided function returns `false` and the response body is not
    /// a valid UTF-8 string, `Utf8Error` will be returned as a handler error
    /// response.
    ///
    /// The default value is a function that always returns `false`.
    ///
    /// For more information about API gateway's binary body type, refer to
    /// [this documentation](https://docs.aws.amazon.com/apigateway/latest/developerguide/api-gateway-payload-encodings.html).
    pub fn binary_media_type_fn(self, value: impl FnMut(&str) -> bool + 'static) -> Self {
        Self {
            binary_media_type_fn: Box::new(value),
            ..self
        }
    }

    /// Set a set of content types transmitted as a binary response payload.
    ///
    /// This method is a wrapper for `binary_media_type_fn`.
    pub fn binary_media_types(self, value: impl IntoIterator<Item = impl Into<String>>) -> Self {
        let types: Vec<String> = value.into_iter().map(Into::into).collect();
        Self {
            binary_media_type_fn: Box::new(move |ty| types.iter().any(|e| ty == e)),
            ..self
        }
    }

    /// Start polling for API gateway and ALB events.
    ///
    /// # Panics
    ///
    /// See [`lambda_http::start`].
    pub fn start(self) -> Result<(), S::InitError> {
        // TODO: Check possible causes of `new` failure
        let mut rt = actix_rt::Runtime::new().unwrap();

        let cfg = ServerConfig::new("127.0.0.1:8080".parse().unwrap());
        let new_service = (self.factory)().into_new_service();
        let mut service = rt.block_on(new_service.new_service(&cfg))?;

        let mut binary_media_type_fn = self.binary_media_type_fn;

        // The handler is `FnMut` (doesn't have to be `Fn + 'static`)
        let lambda_http_handler =
            |mut req: lambda_http::Request,
             _ctx: lambda_runtime::Context|
             -> Result<lambda_http::Response<LambdaBody>, HandlerError> {
                // Construct `actix_http::Payload`
                let mut payload = actix_http::h1::Payload::empty();
                match req.body_mut() {
                    LambdaBody::Empty => {}
                    LambdaBody::Text(text) => {
                        payload.unread_data(replace(text, String::new()).into())
                    }
                    LambdaBody::Binary(bytes) => {
                        payload.unread_data(replace(bytes, Vec::new()).into())
                    }
                }

                let mut actix_req: Request = Request::with_payload(payload.into());

                // Set the headers
                let actix_req_head = actix_req.head_mut();
                actix_req_head.method = req.method().clone();
                actix_req_head.version = req.version();
                actix_req_head.headers = replace(req.headers_mut(), Default::default()).into();
                actix_req_head.uri = {
                    let mut builder = uri::Builder::new();
                    builder.scheme(req.uri().scheme_part().unwrap().clone());
                    builder.authority(req.uri().authority_part().unwrap().clone());

                    // Reconstruct the encoded query parameters
                    let query_params = req.query_string_parameters();
                    let mut path = req.uri().path().to_string();
                    for (i, (key, value)) in query_params.iter().enumerate() {
                        write!(
                            path,
                            "{}{}={}",
                            if i == 0 { "?" } else { "&" },
                            utf8_percent_encode(key, enc_set::URL_ENCODE),
                            utf8_percent_encode(value, enc_set::URL_ENCODE),
                        )
                        .unwrap();
                    }
                    builder.path_and_query(path.as_str());

                    debug!(
                        "Original URI = {:?}, query string parameters = {:?}",
                        req.uri(),
                        query_params
                    );

                    builder.build().unwrap()
                };

                debug!("Reconstructed URI = {:?}", actix_req_head.uri);

                // TODO: Extensions from `lambda_http::RequestExt`. There are five:
                //  - `path_parameters`
                //  - `stage_variables`
                //  - `request_context`

                // Call the inner handler
                let user_resp = rt.block_on(service.call(actix_req));

                let mut actix_resp: Response<Bytes> = user_resp
                    // Convert `S::Error` to `Error`
                    .map_err(Into::into)
                    // Synchronously evaluate the response body
                    .and_then(|success_user_resp| {
                        let mut actix_resp = success_user_resp.into();

                        let resp_bytes =
                            read_body(&mut rt, actix_resp.take_body()).map(BytesMut::freeze);

                        match resp_bytes {
                            Ok(resp_bytes) => Ok(actix_resp.set_body(resp_bytes)),
                            Err(e) => {
                                debug!("Extracing the response failed, treating it as a handler error");
                                Err(e)
                            },
                        }
                    })
                    // Construct a response for internal errors (if any)
                    .unwrap_or_else(|actix_err| {
                        debug!("Got a handler error ({:?}), generating an error response", actix_err);

                        let mut actix_resp2 = actix_err.as_response_error().render_response();

                        // Convert the body to `Bytes` from `Body`. However, this
                        // operation is fallible. Should this fail, return an empty body,
                        // ignoring the error.
                        let resp_bytes = read_body(&mut rt, actix_resp2.take_body())
                            .map(BytesMut::freeze)
                            .unwrap_or_else(|e| {
                                warn!("Failed to extract the body of the error response, ignoring: {:?}", e);

                                Default::default()
                            });

                        actix_resp2.set_body(resp_bytes)
                    });

                // Construct `lambda_http::Response`.
                let resp_body_bytes = match actix_resp.take_body() {
                    ResponseBody::Body(bytes) => bytes,
                    ResponseBody::Other(_) => unreachable!(),
                };

                // Clone the payload as a `Vec`
                // (I couldn't find a copy-less way to do this)
                let resp_body_vec = resp_body_bytes.to_vec();

                let content_type = (actix_resp.head().headers())
                    .get(CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or("");
                let is_binary = binary_media_type_fn(content_type);

                debug!(
                    "Encoding the response body as {} for content type {:?}",
                    if is_binary { "binary" } else { "text" },
                    content_type
                );

                let resp_body = if is_binary {
                    LambdaBody::Binary(resp_body_vec)
                } else {
                    LambdaBody::Text(String::from_utf8(resp_body_vec)?)
                };

                // Then, copy the header
                let mut resp = lambda_http::Response::new(resp_body);
                *resp.status_mut() = actix_resp.status();
                *resp.headers_mut() = actix_resp
                    .headers()
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();

                Ok(resp)
            };

        lambda_http::lambda!(lambda_http_handler);

        Ok(())
    }
}

fn read_body(rt: &mut actix_rt::Runtime, body: impl MessageBody) -> Result<BytesMut, Error> {
    rt.block_on(ResponseBody::Body(body).fold(BytesMut::new(), |mut x, y| {
        x.extend_from_slice(&y);
        Ok::<_, Error>(x)
    }))
}

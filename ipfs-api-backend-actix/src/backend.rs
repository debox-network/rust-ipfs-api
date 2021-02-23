use crate::error::Error;
use async_trait::async_trait;
use awc::Client;
use bytes::Bytes;
use futures::{FutureExt, Stream, StreamExt, TryFutureExt, TryStreamExt};
use http::{
    header::{HeaderName, HeaderValue},
    uri::Scheme,
    StatusCode, Uri,
};
use ipfs_api_prelude::{ApiRequest, Backend, TryFromUri};
use serde::Serialize;
use std::time::Duration;

const ACTIX_REQUEST_TIMEOUT: Duration = Duration::from_secs(90);

pub struct ActixBackend {
    base: Uri,
    client: Client,
}

impl Default for ActixBackend {
    fn default() -> Self {
        Self::from_ipfs_config()
            .unwrap_or_else(|| Self::from_host_and_port(Scheme::HTTP, "localhost", 5001).unwrap())
    }
}

impl TryFromUri for ActixBackend {
    fn build_with_base_uri(base: Uri) -> Self {
        let client = Client::default();

        ActixBackend { base, client }
    }
}

#[async_trait(?Send)]
impl Backend for ActixBackend {
    type HttpRequest = awc::SendClientRequest;

    type HttpResponse = awc::ClientResponse<
        actix_http::encoding::Decoder<actix_http::Payload<actix_http::PayloadStream>>,
    >;

    type MultipartForm = multipart::client::multipart::Form<'static>;

    type Error = Error;

    fn build_base_request<Req>(
        &self,
        req: &Req,
        form: Option<Self::MultipartForm>,
    ) -> Result<Self::HttpRequest, Error>
    where
        Req: ApiRequest,
    {
        req.absolute_url(&self.base).and_then(|url| {
            let req = if let Some(form) = form {
                self.client
                    .post(url)
                    .timeout(ACTIX_REQUEST_TIMEOUT)
                    .content_type(form.content_type())
                    .send_body(multipart::client::multipart::Body::from(form))
            } else {
                self.client.post(url).timeout(ACTIX_REQUEST_TIMEOUT).send()
            };

            Ok(req)
        })
    }

    fn get_header<'a>(res: &'a Self::HttpResponse, key: HeaderName) -> Option<&'a HeaderValue> {
        res.headers().get(key)
    }

    async fn request_raw<Req>(
        &self,
        req: Req,
        form: Option<Self::MultipartForm>,
    ) -> Result<(StatusCode, Bytes), Self::Error>
    where
        Req: ApiRequest + Serialize,
    {
        let req = self.build_base_request(&req, form)?;
        let mut res = req.await?;
        let status = res.status();
        let body = res.body().await?;

        // FIXME: Actix compat with bytes 1.0
        Ok((status, Bytes::copy_from_slice(body.as_ref())))
    }

    fn response_to_byte_stream(
        res: Self::HttpResponse,
    ) -> Box<dyn Stream<Item = Result<Bytes, Self::Error>> + Unpin> {
        let stream = res
            .map_ok(|bytes| Bytes::copy_from_slice(bytes.as_ref()))
            .err_into();

        Box::new(stream)
    }

    fn request_stream<Res, F, OutStream>(
        &self,
        req: Self::HttpRequest,
        process: F,
    ) -> Box<dyn Stream<Item = Result<Res, Self::Error>> + Unpin>
    where
        OutStream: Stream<Item = Result<Res, Self::Error>> + Unpin,
        F: 'static + Fn(Self::HttpResponse) -> OutStream,
    {
        let stream = req
            .err_into()
            .map_ok(move |mut res| {
                match res.status() {
                    StatusCode::OK => process(res).right_stream(),
                    // If the server responded with an error status code, the body
                    // still needs to be read so an error can be built. This block will
                    // read the entire body stream, then immediately return an error.
                    //
                    _ => res
                        .body()
                        .map(|maybe_body| match maybe_body {
                            Ok(body) => {
                                // FIXME: Actix compat with bytes 1.0
                                let body = Bytes::copy_from_slice(body.as_ref());

                                Err(Self::process_error_from_body(body))
                            }
                            Err(e) => Err(e.into()),
                        })
                        .into_stream()
                        .left_stream(),
                }
            })
            .try_flatten_stream();

        Box::new(stream)
    }
}

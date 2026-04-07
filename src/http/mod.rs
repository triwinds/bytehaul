pub(crate) mod request;
pub(crate) mod response;
pub(crate) mod worker;

use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper::body::Incoming;
use hyper::Response;

use crate::error::{DownloadError, TransportError};

pub(crate) type HttpRequestBody = Empty<Bytes>;
pub(crate) type HttpResponse = Response<Incoming>;

pub(crate) async fn next_data_chunk(
	body: &mut Incoming,
	read_timeout: Duration,
) -> Result<Option<Bytes>, DownloadError> {
	loop {
		let frame = tokio::time::timeout(read_timeout, body.frame())
			.await
			.map_err(|_| DownloadError::timeout("response body timed out"))?;

		match frame {
			Some(Ok(frame)) => match frame.into_data() {
				Ok(data) => return Ok(Some(data)),
				Err(_) => continue,
			},
			Some(Err(error)) => return Err(TransportError::from(error).into()),
			None => return Ok(None),
		}
	}
}

// Copyright (c) 2016-2026 Greg Steffensen and the http-serve developers
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE.txt or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT.txt or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Internal metrics tracking functions.
//! The bodies of these functions are conditionally compiled, meaning
//! they compile to zero overhead when the `metrics` feature is disabled.

use http::Response;

#[allow(unused_variables)]
pub(crate) fn record_request<B>(resp: &Response<B>, duration: std::time::Duration) {
    #[cfg(feature = "metrics")]
    {
        let status = resp.status();
        let content_type = resp
            .headers()
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("unknown");
        let encoding_label = resp
            .headers()
            .get(http::header::CONTENT_ENCODING)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("identity");
        let size = resp
            .headers()
            .get(http::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok().and_then(|s| s.parse::<u64>().ok()))
            .unwrap_or(0);

        let status_label = status.as_u16().to_string();

        metrics::counter!(
            "serdir.request_count",
            "status" => status_label.clone(),
            "content_type" => content_type.to_string(),
            "encoding" => encoding_label.to_string()
        )
        .increment(1);

        metrics::histogram!(
            "serdir.response_size_bytes",
            "content_type" => content_type.to_string()
        )
        .record(size as f64);

        metrics::histogram!(
            "serdir.request_duration_seconds",
            "status" => status_label
        )
        .record(duration.as_secs_f64());
    }
}

#[allow(unused_variables)]
pub(crate) fn record_brotli_compression(before_size: u64, after_size: u64) {
    #[cfg(feature = "metrics")]
    {
        metrics::counter!("serdir.brotli_size_before_bytes").increment(before_size);
        metrics::counter!("serdir.brotli_size_after_bytes").increment(after_size);
    }
}

#[allow(unused_variables)]
pub(crate) fn record_cache_access(hit: bool) {
    #[cfg(feature = "metrics")]
    {
        if hit {
            metrics::counter!("serdir.resource_cache_hits").increment(1);
        } else {
            metrics::counter!("serdir.resource_cache_misses").increment(1);
        }
    }
}

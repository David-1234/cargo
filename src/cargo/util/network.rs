use anyhow::Error;

use crate::util::errors::{CargoResult, HttpNotSuccessful};
use crate::util::Config;
use std::task::Poll;

pub trait PollExt<T> {
    fn expect(self, msg: &str) -> T;
}

impl<T> PollExt<T> for Poll<T> {
    #[track_caller]
    fn expect(self, msg: &str) -> T {
        match self {
            Poll::Ready(val) => val,
            Poll::Pending => panic!("{}", msg),
        }
    }
}

pub struct Retry<'a> {
    config: &'a Config,
    remaining: u32,
}

impl<'a> Retry<'a> {
    pub fn new(config: &'a Config) -> CargoResult<Retry<'a>> {
        Ok(Retry {
            config,
            remaining: config.net_config()?.retry.unwrap_or(2),
        })
    }

    /// Returns `Ok(None)` for operations that should be re-tried.
    pub fn r#try<T>(&mut self, f: impl FnOnce() -> CargoResult<T>) -> CargoResult<Option<T>> {
        match f() {
            Err(ref e) if maybe_spurious(e) && self.remaining > 0 => {
                let msg = format!(
                    "spurious network error ({} tries remaining): {}",
                    self.remaining,
                    e.root_cause(),
                );
                self.config.shell().warn(msg)?;
                self.remaining -= 1;
                Ok(None)
            }
            other => other.map(Some),
        }
    }
}

fn maybe_spurious(err: &Error) -> bool {
    if let Some(git_err) = err.downcast_ref::<git2::Error>() {
        match git_err.class() {
            git2::ErrorClass::Net
            | git2::ErrorClass::Os
            | git2::ErrorClass::Zlib
            | git2::ErrorClass::Http => return git_err.code() != git2::ErrorCode::Certificate,
            _ => (),
        }
    }
    if let Some(curl_err) = err.downcast_ref::<curl::Error>() {
        if curl_err.is_couldnt_connect()
            || curl_err.is_couldnt_resolve_proxy()
            || curl_err.is_couldnt_resolve_host()
            || curl_err.is_operation_timedout()
            || curl_err.is_recv_error()
            || curl_err.is_send_error()
            || curl_err.is_http2_error()
            || curl_err.is_http2_stream_error()
            || curl_err.is_ssl_connect_error()
            || curl_err.is_partial_file()
        {
            return true;
        }
    }
    if let Some(not_200) = err.downcast_ref::<HttpNotSuccessful>() {
        if 500 <= not_200.code && not_200.code < 600 {
            return true;
        }
    }

    use gix::protocol::transport::IsSpuriousError;

    if let Some(err) = err.downcast_ref::<crate::sources::git::fetch::Error>() {
        if err.is_spurious() {
            return true;
        }
    }

    false
}

/// Wrapper method for network call retry logic.
///
/// Retry counts provided by Config object `net.retry`. Config shell outputs
/// a warning on per retry.
///
/// Closure must return a `CargoResult`.
///
/// # Examples
///
/// ```
/// # use crate::cargo::util::{CargoResult, Config};
/// # let download_something = || return Ok(());
/// # let config = Config::default().unwrap();
/// use cargo::util::network;
/// let cargo_result = network::with_retry(&config, || download_something());
/// ```
pub fn with_retry<T, F>(config: &Config, mut callback: F) -> CargoResult<T>
where
    F: FnMut() -> CargoResult<T>,
{
    let mut retry = Retry::new(config)?;
    loop {
        if let Some(ret) = retry.r#try(&mut callback)? {
            return Ok(ret);
        }
    }
}

// When dynamically linked against libcurl, we want to ignore some failures
// when using old versions that don't support certain features.
#[macro_export]
macro_rules! try_old_curl {
    ($e:expr, $msg:expr) => {
        let result = $e;
        if cfg!(target_os = "macos") {
            if let Err(e) = result {
                warn!("ignoring libcurl {} error: {}", $msg, e);
            }
        } else {
            result.with_context(|| {
                anyhow::format_err!("failed to enable {}, is curl not built right?", $msg)
            })?;
        }
    };
}

#[test]
fn with_retry_repeats_the_call_then_works() {
    use crate::core::Shell;

    //Error HTTP codes (5xx) are considered maybe_spurious and will prompt retry
    let error1 = HttpNotSuccessful {
        code: 501,
        url: "Uri".to_string(),
        body: Vec::new(),
    }
    .into();
    let error2 = HttpNotSuccessful {
        code: 502,
        url: "Uri".to_string(),
        body: Vec::new(),
    }
    .into();
    let mut results: Vec<CargoResult<()>> = vec![Ok(()), Err(error1), Err(error2)];
    let config = Config::default().unwrap();
    *config.shell() = Shell::from_write(Box::new(Vec::new()));
    let result = with_retry(&config, || results.pop().unwrap());
    assert!(result.is_ok())
}

#[test]
fn with_retry_finds_nested_spurious_errors() {
    use crate::core::Shell;

    //Error HTTP codes (5xx) are considered maybe_spurious and will prompt retry
    //String error messages are not considered spurious
    let error1 = anyhow::Error::from(HttpNotSuccessful {
        code: 501,
        url: "Uri".to_string(),
        body: Vec::new(),
    });
    let error1 = anyhow::Error::from(error1.context("A non-spurious wrapping err"));
    let error2 = anyhow::Error::from(HttpNotSuccessful {
        code: 502,
        url: "Uri".to_string(),
        body: Vec::new(),
    });
    let error2 = anyhow::Error::from(error2.context("A second chained error"));
    let mut results: Vec<CargoResult<()>> = vec![Ok(()), Err(error1), Err(error2)];
    let config = Config::default().unwrap();
    *config.shell() = Shell::from_write(Box::new(Vec::new()));
    let result = with_retry(&config, || results.pop().unwrap());
    assert!(result.is_ok())
}

#[test]
fn curle_http2_stream_is_spurious() {
    let code = curl_sys::CURLE_HTTP2_STREAM;
    let err = curl::Error::new(code);
    assert!(maybe_spurious(&err.into()));
}

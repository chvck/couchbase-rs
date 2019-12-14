use crate::core::ServiceType;
use http::request::Builder as HttpRequestBuilder;

pub trait Authenticator: Send + Sync {
    fn auth_http_request(
        &self,
        service: ServiceType,
        request: HttpRequestBuilder,
    ) -> HttpRequestBuilder;
}

pub struct PasswordAuthenticator {
    _username: String,
    _password: String,
    cached_header: String,
}

impl PasswordAuthenticator {
    pub fn new<S: Into<String>>(username: S, password: S) -> Self {
        let username = username.into();
        let password = password.into();

        let cached = base64::encode(format!("{}:{}", username, password).as_bytes());
        Self {
            _username: username,
            _password: password,
            cached_header: cached,
        }
    }
}

impl Authenticator for PasswordAuthenticator {
    fn auth_http_request(
        &self,
        _service: ServiceType,
        request: HttpRequestBuilder,
    ) -> HttpRequestBuilder {
        request.header(http::header::AUTHORIZATION, &self.cached_header)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use hyper::Request;

    #[test]
    fn foo() {
        let request_builder = Request::builder();
        let authenticator = PasswordAuthenticator::new("foo", "bar");
        let request_builder = authenticator.auth_http_request(ServiceType::Query, request_builder);

        let headers = request_builder.headers_ref().unwrap();
        assert_eq!(headers["authorization"], "Zm9vOmJhcg==");
    }
}

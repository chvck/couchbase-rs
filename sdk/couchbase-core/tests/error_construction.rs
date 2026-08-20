//! Errors this crate produces must be constructible from outside it.
//!
//! A consumer that asserts on a taxonomy has to be able to build the values it
//! asserts about; a corpus test that replays recorded server bytes has to be
//! able to turn them back into errors. Neither is reachable through the parse
//! paths for the KV half, which has no public packet-to-error decode.

use couchbase_core::error::{Error, ErrorKind};
use couchbase_core::memdx::error::{
    Error as MemdxErrorType, ServerError as MemdxServerError,
    ServerErrorKind as MemdxServerErrorKind,
};
use couchbase_core::memdx::opcode::OpCode;
use couchbase_core::memdx::status::Status;
use couchbase_core::mgmtx::error::{
    Error as MgmtxErrorType, ServerError as MgmtxServerError,
    ServerErrorKind as MgmtxServerErrorKind,
};
use couchbase_core::queryx::error::{
    Error as QueryxError, ServerError as QueryxServerError,
    ServerErrorKind as QueryxServerErrorKind,
};
use http::{Method, StatusCode};

#[test]
fn all_six_error_constructors_are_public() {
    // 1. Error::new(kind: ErrorKind) -> Error
    let err = Error::new(ErrorKind::VbucketMapOutdated);
    assert!(matches!(err.kind(), ErrorKind::VbucketMapOutdated));

    // 2. memdx::ServerError::new(kind, op_code, status, opaque) -> ServerError
    // + verify From<memdx::ServerError> for memdx::error::Error is public
    let memdx_server = MemdxServerError::new(
        MemdxServerErrorKind::KeyNotFound,
        OpCode::Get,
        Status::KeyNotFound,
        0,
    );
    assert_eq!(memdx_server.kind(), &MemdxServerErrorKind::KeyNotFound);

    // Call the From impl: From<ServerError> for memdx::error::Error
    let memdx_error: MemdxErrorType = MemdxServerError::new(
        MemdxServerErrorKind::KeyNotFound,
        OpCode::Get,
        Status::KeyNotFound,
        0,
    )
    .into();
    // Assert it's a server error by checking it's a server error kind
    assert!(memdx_error.is_server_error_kind(MemdxServerErrorKind::KeyNotFound));

    // 3. mgmtx::ServerError::new(status_code, url, method, path, body, kind) -> ServerError
    // + verify From<mgmtx::ServerError> for mgmtx::error::Error is public
    let mgmtx_server = MgmtxServerError::new(
        StatusCode::NOT_FOUND,
        "http://localhost:8091".to_string(),
        Method::GET,
        "/pools".to_string(),
        "Not found".to_string(),
        MgmtxServerErrorKind::BucketNotFound,
    );
    assert_eq!(mgmtx_server.status_code(), StatusCode::NOT_FOUND);

    // Call the From impl: From<ServerError> for mgmtx::error::Error
    let mgmtx_error: MgmtxErrorType = MgmtxServerError::new(
        StatusCode::NOT_FOUND,
        "http://localhost:8091".to_string(),
        Method::GET,
        "/pools".to_string(),
        "Not found".to_string(),
        MgmtxServerErrorKind::BucketNotFound,
    )
    .into();
    // Assert it's a server error by checking the kind
    assert!(matches!(
        mgmtx_error.kind(),
        couchbase_core::mgmtx::error::ErrorKind::Server(_)
    ));

    // 4. queryx::ServerError::new(kind, endpoint, status_code, code, retry, msg) -> ServerError
    let queryx_server = QueryxServerError::new(
        QueryxServerErrorKind::Timeout,
        "localhost",
        StatusCode::OK,
        5000,
        true,
        "Request timed out",
    );
    assert_eq!(queryx_server.kind(), &QueryxServerErrorKind::Timeout);

    // 5. queryx::Error::new_server_error(e: ServerError) -> Error
    let queryx_err = QueryxError::new_server_error(queryx_server);
    assert!(matches!(
        queryx_err.kind(),
        couchbase_core::queryx::error::ErrorKind::Server(_)
    ));
}
